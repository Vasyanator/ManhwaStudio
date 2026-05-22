/*
FILE HEADER (cleaning/tools/stamp.rs)
- Назначение: порт инструмента "Штамп" для вкладки cleaning
  (копирование пикселей из подпапок `alt_vers` в clean-overlay).
- Ключевые сущности:
  - `StampTool`: состояние UI, выбора источника, фонового кэша alt-страниц и активного штриха/прямоугольника.
  - `StampScratch`: временный буфер штриха по видимой области (как у `zamazka`, без спама в модель на каждый move).
  - `SourceLoadRequest/SourceLoadResult`: очередь фоновой загрузки одной текущей alt-страницы.
- Поведение:
  - ЛКМ: штамп кистью; `Ctrl+ЛКМ`: прямоугольный штамп; `Shift+ЛКМ`: прямоугольное стирание.
  - Источник берётся из выбранной папки `project/alt_vers/<name>`; загрузка лениво и только для текущей страницы.
  - На панели инструмента показывается статус, когда кэш alt-версии текущей страницы ещё грузится.
  - Во время штриха используется локальный scratch-буфер и коммит в модель только в конце штриха.
  - Если сторона scratch-превью больше safe-порога (`16k`), инструмент не создаёт giant texture в `egui`,
    а показывает превью инкрементально через `replace_overlay_region_local`, обходя лимит texture size.
- Потоки:
  - Отдельный worker декодирует alt-страницу в `RgbaImage` и отправляет результат через channel.
*/
use super::base::{BrushToolBase, CleaningCursorOccluder, CleaningTool, StrokePoint};
use crate::canvas::{CanvasView, OverlayRectPx};
use crate::project::ProjectData;
use crate::tools::paint_line_with_brush;
use crate::widgets::{WheelComboBox, WheelSlider, WheelSpinBox};
use eframe::egui;
use egui::{Color32, Rect, TextureHandle, TextureOptions};
use image::RgbaImage;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

const STAMP_PREVIEW_TEXTURE_OPTIONS: TextureOptions = TextureOptions::LINEAR;
const STAMP_CURSOR_TEXTURE_OPTIONS: TextureOptions = TextureOptions::NEAREST;
const STAMP_SCRATCH_TEXTURE_SIDE_LIMIT: usize = 16_000;
const STAMP_SPACING_FACTOR: f32 = 0.6;

#[derive(Clone)]
struct SourceLoadRequest {
    job_id: u64,
    page_idx: usize,
    overlay_width: usize,
    path: PathBuf,
}

struct SourceLoadResult {
    job_id: u64,
    page_idx: usize,
    overlay_width: usize,
    path: PathBuf,
    image: Option<Arc<RgbaImage>>,
    error: Option<String>,
}

struct CachedSourcePage {
    page_idx: usize,
    overlay_width: usize,
    path: PathBuf,
    image: Arc<RgbaImage>,
}

struct StampScratch {
    page_idx: usize,
    overlay_rect: OverlayRectPx,
    scene_rect: Rect,
    image: egui::ColorImage,
    texture: Option<TextureHandle>,
    texture_options: TextureOptions,
    texture_dirty: bool,
    preview_into_canvas: bool,
    painted: bool,
}

pub struct StampTool {
    brush_base: BrushToolBase,
    preview_opacity: u8,
    y_offset: i32,
    rect_start: Option<StrokePoint>,
    rect_current: Option<StrokePoint>,
    rect_erase: bool,
    scratch: Option<StampScratch>,
    active_source: Option<Arc<RgbaImage>>,
    active_erase: bool,
    last_stroke_point: Option<StrokePoint>,
    touched_pages: HashSet<usize>,
    alt_vers_dir: Option<PathBuf>,
    source_dirs: Vec<String>,
    selected_source_idx: Option<usize>,
    source_paths: Vec<PathBuf>,
    source_cache: Option<CachedSourcePage>,
    source_load_tx: Sender<SourceLoadRequest>,
    source_load_rx: Receiver<SourceLoadResult>,
    source_load_pending: Option<SourceLoadRequest>,
    next_source_job_id: u64,
    source_status_text: Option<String>,
    current_page_idx: Option<usize>,
    cursor_texture: Option<TextureHandle>,
    cursor_texture_size: [usize; 2],
}

impl Default for StampTool {
    fn default() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<SourceLoadRequest>();
        let (result_tx, result_rx) = mpsc::channel::<SourceLoadResult>();
        thread::spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let mut result = SourceLoadResult {
                    job_id: request.job_id,
                    page_idx: request.page_idx,
                    overlay_width: request.overlay_width,
                    path: request.path.clone(),
                    image: None,
                    error: None,
                };

                match image::open(&request.path) {
                    Ok(decoded) => {
                        let rgba = decoded.to_rgba8();
                        let width = rgba.width() as usize;
                        if request.overlay_width > 0 && width != request.overlay_width {
                            result.error = Some(format!(
                                "Ширина источника ({width}px) не совпадает с overlay ({})",
                                request.overlay_width
                            ));
                        } else {
                            result.image = Some(Arc::new(rgba));
                        }
                    }
                    Err(error) => {
                        result.error = Some(format!(
                            "Не удалось открыть {}: {error}",
                            request.path.display()
                        ));
                    }
                }
                let _ = result_tx.send(result);
            }
        });

        Self {
            brush_base: BrushToolBase::default(),
            preview_opacity: 160,
            y_offset: 0,
            rect_start: None,
            rect_current: None,
            rect_erase: false,
            scratch: None,
            active_source: None,
            active_erase: false,
            last_stroke_point: None,
            touched_pages: HashSet::new(),
            alt_vers_dir: None,
            source_dirs: Vec::new(),
            selected_source_idx: None,
            source_paths: Vec::new(),
            source_cache: None,
            source_load_tx: request_tx,
            source_load_rx: result_rx,
            source_load_pending: None,
            next_source_job_id: 1,
            source_status_text: None,
            current_page_idx: None,
            cursor_texture: None,
            cursor_texture_size: [0, 0],
        }
    }
}

impl StampTool {
    fn clear_runtime_stroke_state(&mut self) {
        self.scratch = None;
        self.active_source = None;
        self.active_erase = false;
        self.last_stroke_point = None;
    }

    fn poll_source_loader(&mut self) {
        loop {
            match self.source_load_rx.try_recv() {
                Ok(result) => {
                    let still_pending = self
                        .source_load_pending
                        .as_ref()
                        .is_some_and(|pending| pending.job_id == result.job_id);
                    if !still_pending {
                        continue;
                    }
                    self.source_load_pending = None;
                    if let Some(img) = result.image {
                        self.source_cache = Some(CachedSourcePage {
                            page_idx: result.page_idx,
                            overlay_width: result.overlay_width,
                            path: result.path,
                            image: img,
                        });
                        self.source_status_text = None;
                    } else {
                        self.source_cache = None;
                        self.source_status_text = result.error;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn sync_source_dirs(&mut self, project: &ProjectData) {
        let alt_dir = project.paths.alt_vers_dir.clone();
        if self
            .alt_vers_dir
            .as_ref()
            .is_some_and(|cached| cached == &alt_dir)
        {
            return;
        }
        self.alt_vers_dir = Some(alt_dir.clone());
        self.source_dirs = list_alt_version_dirs(&alt_dir);
        if self
            .selected_source_idx
            .is_some_and(|idx| idx >= self.source_dirs.len())
        {
            self.selected_source_idx = None;
        }
        if self.selected_source_idx.is_none() && !self.source_dirs.is_empty() {
            self.selected_source_idx = Some(0);
        }
        self.reload_source_paths();
    }

    fn selected_source_name(&self) -> Option<&str> {
        self.selected_source_idx
            .and_then(|idx| self.source_dirs.get(idx))
            .map(String::as_str)
    }

    fn set_selected_source_idx(&mut self, idx: Option<usize>) {
        if self.selected_source_idx == idx {
            return;
        }
        self.selected_source_idx = idx;
        self.reload_source_paths();
    }

    fn reload_source_paths(&mut self) {
        let Some(base_dir) = self.alt_vers_dir.as_ref() else {
            self.source_paths.clear();
            self.source_cache = None;
            self.source_load_pending = None;
            self.source_status_text = None;
            return;
        };
        let Some(subdir) = self.selected_source_name() else {
            self.source_paths.clear();
            self.source_cache = None;
            self.source_load_pending = None;
            self.source_status_text = None;
            return;
        };
        self.source_paths = list_source_images(&base_dir.join(subdir));
        self.source_cache = None;
        self.source_load_pending = None;
        self.source_status_text = None;
    }

    fn source_path_for_page(&self, page_idx: usize) -> Option<PathBuf> {
        self.source_paths.get(page_idx).cloned()
    }

    fn queue_source_load_for_page_if_needed(&mut self, page_idx: usize, overlay_width: usize) {
        let Some(path) = self.source_path_for_page(page_idx) else {
            self.source_cache = None;
            self.source_load_pending = None;
            self.source_status_text =
                Some("Для текущей страницы нет файла в выбранной alt-версии.".to_string());
            return;
        };

        if self.source_cache.as_ref().is_some_and(|cached| {
            cached.page_idx == page_idx
                && cached.overlay_width == overlay_width
                && cached.path == path
        }) {
            return;
        }

        if self.source_load_pending.as_ref().is_some_and(|pending| {
            pending.page_idx == page_idx
                && pending.overlay_width == overlay_width
                && pending.path == path
        }) {
            return;
        }

        self.source_cache = None;
        self.source_status_text = None;
        let request = SourceLoadRequest {
            job_id: self.next_source_job_id,
            page_idx,
            overlay_width,
            path,
        };
        self.next_source_job_id = self.next_source_job_id.saturating_add(1);
        if self.source_load_tx.send(request.clone()).is_ok() {
            self.source_load_pending = Some(request);
        } else {
            self.source_load_pending = None;
            self.source_status_text =
                Some("Не удалось отправить задачу фоновой загрузки alt-версии.".to_string());
        }
    }

    fn source_image_for_page(
        &self,
        page_idx: usize,
        overlay_width: usize,
    ) -> Option<Arc<RgbaImage>> {
        let cached = self.source_cache.as_ref()?;
        if cached.page_idx != page_idx {
            return None;
        }
        if overlay_width > 0 && cached.overlay_width > 0 && cached.overlay_width != overlay_width {
            return None;
        }
        Some(Arc::clone(&cached.image))
    }

    fn source_loading_for_current_page(&self) -> bool {
        let Some(page_idx) = self.current_page_idx else {
            return false;
        };
        let pending_matches_page = self
            .source_load_pending
            .as_ref()
            .is_some_and(|pending| pending.page_idx == page_idx);
        let has_ready_cache = self
            .source_cache
            .as_ref()
            .is_some_and(|cached| cached.page_idx == page_idx);
        pending_matches_page && !has_ready_cache
    }

    fn ensure_current_page_source_prefetch(&mut self, canvas: &CanvasView) {
        if self.selected_source_name().is_none() {
            return;
        }
        let page_idx = canvas.current_page_idx();
        self.current_page_idx = Some(page_idx);
        let overlay_width = canvas
            .overlay_size(page_idx)
            .map(|size| size[0])
            .unwrap_or(0);
        self.queue_source_load_for_page_if_needed(page_idx, overlay_width);
    }

    fn begin_scratch_stroke(
        &mut self,
        canvas: &mut CanvasView,
        point: StrokePoint,
        erase: bool,
    ) -> bool {
        if self.brush_base.should_ignore_drawing() {
            return false;
        }
        if self.scratch.is_some() {
            return true;
        }

        let radius = self.brush_base.radius_px().max(1);
        if canvas.overlay_image(point.page_idx).is_none() {
            let tiny = egui::ColorImage::filled([1, 1], Color32::TRANSPARENT);
            let tiny_scene_rect = Rect::from_center_size(point.scene_pos, egui::vec2(1.0, 1.0));
            let _ = canvas.replace_overlay_region_local(point.page_idx, tiny_scene_rect, &tiny);
        }

        let overlay_width = canvas
            .overlay_size(point.page_idx)
            .map(|size| size[0])
            .unwrap_or(0);
        let source = if erase {
            None
        } else if let Some(img) = self.source_image_for_page(point.page_idx, overlay_width) {
            Some(img)
        } else {
            self.queue_source_load_for_page_if_needed(point.page_idx, overlay_width);
            self.source_status_text =
                Some("Кэш альтернативной версии для текущей страницы ещё не готов.".to_string());
            return false;
        };

        let Some(overlay) = canvas.overlay_image(point.page_idx) else {
            return false;
        };
        let Some(page_scene_rect) = canvas.page_scene_rect(point.page_idx) else {
            return false;
        };
        let visible_scene_rect = canvas
            .visible_scene_rect()
            .unwrap_or(page_scene_rect)
            .intersect(page_scene_rect);
        if !visible_scene_rect.is_positive() {
            return false;
        }
        let Some(base_rect) = canvas.scene_rect_to_overlay_rect(point.page_idx, visible_scene_rect)
        else {
            return false;
        };
        let [overlay_w, overlay_h] = overlay.size;
        if overlay_w == 0 || overlay_h == 0 {
            return false;
        }

        let expanded_rect = expand_overlay_rect(base_rect, overlay_w, overlay_h, radius + 2);
        let Some(scene_rect) =
            overlay_rect_to_scene_rect(page_scene_rect, overlay_w, overlay_h, expanded_rect)
        else {
            return false;
        };

        let image = extract_overlay_chunk(overlay, expanded_rect);
        let preview_into_canvas = image.size[0] > STAMP_SCRATCH_TEXTURE_SIDE_LIMIT
            || image.size[1] > STAMP_SCRATCH_TEXTURE_SIDE_LIMIT;

        self.scratch = Some(StampScratch {
            page_idx: point.page_idx,
            overlay_rect: expanded_rect,
            scene_rect,
            image,
            texture: None,
            texture_options: STAMP_PREVIEW_TEXTURE_OPTIONS,
            texture_dirty: true,
            preview_into_canvas,
            painted: false,
        });
        self.active_source = source;
        self.active_erase = erase;
        self.last_stroke_point = Some(point);
        self.paint_scratch_segment(canvas, point, point);
        true
    }

    fn paint_scratch_segment(
        &mut self,
        canvas: &mut CanvasView,
        from: StrokePoint,
        to: StrokePoint,
    ) {
        let Some(scratch_page_idx) = self.scratch.as_ref().map(|scratch| scratch.page_idx) else {
            return;
        };
        if from.page_idx != to.page_idx || to.page_idx != scratch_page_idx {
            return;
        }
        let Some((x0, y0)) = canvas.scene_point_to_overlay_xy(to.page_idx, from.scene_pos) else {
            return;
        };
        let Some((x1, y1)) = canvas.scene_point_to_overlay_xy(to.page_idx, to.scene_pos) else {
            return;
        };
        let radius = self.brush_base.radius_px().max(1);

        let mut dirty_overlay: Option<OverlayRectPx> = None;
        if let Some(scratch) = self.scratch.as_mut() {
            let Some((sx0, sy0)) = map_overlay_to_local(&scratch.overlay_rect, x0, y0) else {
                return;
            };
            let Some((sx1, sy1)) = map_overlay_to_local(&scratch.overlay_rect, x1, y1) else {
                return;
            };
            if self.active_erase {
                paint_line_with_brush(
                    &mut scratch.image,
                    sx0 as i32,
                    sy0 as i32,
                    sx1 as i32,
                    sy1 as i32,
                    radius as i32,
                    Color32::TRANSPARENT,
                    true,
                );
            } else if let Some(source) = self.active_source.as_ref() {
                stamp_segment_on_image(
                    &mut scratch.image,
                    source,
                    scratch.overlay_rect,
                    x0 as i32,
                    y0 as i32,
                    x1 as i32,
                    y1 as i32,
                    radius as i32,
                    self.y_offset,
                );
            } else {
                return;
            }
            scratch.texture_dirty = true;
            scratch.painted = true;
            if scratch.preview_into_canvas {
                dirty_overlay =
                    segment_dirty_overlay_rect(x0, y0, x1, y1, radius, scratch.overlay_rect);
            }
        }

        if let Some(dirty_overlay) = dirty_overlay {
            self.flush_scratch_patch_to_canvas(canvas, dirty_overlay);
        }
    }

    fn flush_scratch_patch_to_canvas(
        &mut self,
        canvas: &mut CanvasView,
        dirty_overlay: OverlayRectPx,
    ) {
        let Some(scratch) = self.scratch.as_ref() else {
            return;
        };
        let Some(page_scene_rect) = canvas.page_scene_rect(scratch.page_idx) else {
            return;
        };
        let [overlay_w, overlay_h] = match canvas.overlay_size(scratch.page_idx) {
            Some(size) => size,
            None => return,
        };
        let Some(scene_rect) =
            overlay_rect_to_scene_rect(page_scene_rect, overlay_w, overlay_h, dirty_overlay)
        else {
            return;
        };
        let patch = extract_local_chunk(&scratch.image, scratch.overlay_rect, dirty_overlay);
        let _ = canvas.replace_overlay_region_local(scratch.page_idx, scene_rect, &patch);
    }

    fn draw_scratch_preview(&mut self, ui: &mut egui::Ui, canvas: &CanvasView) {
        let Some(scratch) = self.scratch.as_mut() else {
            return;
        };
        if scratch.preview_into_canvas {
            return;
        }
        let texture_options = if canvas.pixel_sampling_nearest() {
            TextureOptions::NEAREST
        } else {
            STAMP_PREVIEW_TEXTURE_OPTIONS
        };
        if scratch.texture.is_none() {
            let texture = ui.ctx().load_texture(
                "cleaning-stamp-scratch-preview",
                scratch.image.clone(),
                texture_options,
            );
            scratch.texture = Some(texture);
            scratch.texture_options = texture_options;
            scratch.texture_dirty = false;
        } else if scratch.texture_dirty || scratch.texture_options != texture_options {
            if let Some(texture) = scratch.texture.as_mut() {
                texture.set(scratch.image.clone(), texture_options);
            }
            scratch.texture_options = texture_options;
            scratch.texture_dirty = false;
        }
        let Some(texture) = scratch.texture.as_ref() else {
            return;
        };
        ui.painter().image(
            texture.id(),
            scratch.scene_rect,
            Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    fn finish_active_stroke(&mut self, canvas: &mut CanvasView) {
        let Some(scratch) = self.scratch.take() else {
            self.clear_runtime_stroke_state();
            return;
        };
        if scratch.painted {
            if !scratch.preview_into_canvas {
                let _ = canvas.replace_overlay_region_local(
                    scratch.page_idx,
                    scratch.scene_rect,
                    &scratch.image,
                );
            }
            self.touched_pages.insert(scratch.page_idx);
        }
        self.clear_runtime_stroke_state();
    }

    fn commit_touched_pages(&mut self, canvas: &mut CanvasView) {
        for page_idx in self.touched_pages.drain() {
            let _ = canvas.commit_overlay_page_to_model(page_idx);
        }
    }

    fn commit_rect(&mut self, canvas: &mut CanvasView) {
        let (Some(start), Some(end)) = (self.rect_start, self.rect_current) else {
            return;
        };
        if start.page_idx != end.page_idx {
            return;
        }
        let scene_rect = Rect::from_two_pos(start.scene_pos, end.scene_pos);
        if !scene_rect.is_positive() {
            return;
        }
        let Some(target) = canvas.scene_rect_to_overlay_rect(start.page_idx, scene_rect) else {
            return;
        };
        if target.w == 0 || target.h == 0 {
            return;
        }

        let chunk = if self.rect_erase {
            egui::ColorImage::filled([target.w, target.h], Color32::TRANSPARENT)
        } else {
            let overlay_width = canvas
                .overlay_size(start.page_idx)
                .map(|size| size[0])
                .unwrap_or(0);
            let Some(source) = self.source_image_for_page(start.page_idx, overlay_width) else {
                self.queue_source_load_for_page_if_needed(start.page_idx, overlay_width);
                self.source_status_text = Some(
                    "Кэш альтернативной версии для текущей страницы ещё не готов.".to_string(),
                );
                return;
            };
            build_rect_chunk_from_source(&source, target, self.y_offset)
        };

        if canvas.replace_overlay_region(start.page_idx, scene_rect, &chunk) {
            self.touched_pages.insert(start.page_idx);
        }
    }

    fn draw_cursor_preview(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: egui::Pos2,
    ) -> Option<egui::Pos2> {
        let page_idx = canvas.page_index_at_scene_pos(pointer_scene_pos)?;
        let page_scene_rect = canvas.page_scene_rect(page_idx)?;
        let [overlay_w, overlay_h] = canvas.overlay_size(page_idx)?;
        if overlay_w == 0 || overlay_h == 0 {
            return None;
        }
        let radius = self.brush_base.radius_px().max(1);
        let source = self.source_image_for_page(page_idx, overlay_w)?;
        let (cx, cy) = canvas.scene_point_to_overlay_xy(page_idx, pointer_scene_pos)?;

        let patch = build_stamp_patch_with_opacity(
            &source,
            cx as i32,
            cy as i32,
            radius as i32,
            self.y_offset,
            self.preview_opacity,
        );
        let all_transparent = patch.pixels.iter().all(|px| px.a() == 0);
        if all_transparent {
            return None;
        }
        if self.cursor_texture.is_none() || self.cursor_texture_size != patch.size {
            self.cursor_texture = Some(ui.ctx().load_texture(
                "cleaning-stamp-cursor-preview",
                patch.clone(),
                STAMP_CURSOR_TEXTURE_OPTIONS,
            ));
            self.cursor_texture_size = patch.size;
        } else if let Some(texture) = self.cursor_texture.as_mut() {
            texture.set(patch, STAMP_CURSOR_TEXTURE_OPTIONS);
        }
        let texture = self.cursor_texture.as_ref()?;

        ui.ctx()
            .output_mut(|out| out.cursor_icon = egui::CursorIcon::None);
        let px_to_scene_x = page_scene_rect.width() / overlay_w as f32;
        let px_to_scene_y = page_scene_rect.height() / overlay_h as f32;
        let snapped_center = egui::pos2(
            page_scene_rect.left() + (cx as f32 + 0.5) * px_to_scene_x,
            page_scene_rect.top() + (cy as f32 + 0.5) * px_to_scene_y,
        );
        let side_scene_x = (radius as f32 * 2.0) * px_to_scene_x;
        let side_scene_y = (radius as f32 * 2.0) * px_to_scene_y;
        let preview_rect = Rect::from_min_size(
            egui::pos2(
                snapped_center.x - radius as f32 * px_to_scene_x,
                snapped_center.y - radius as f32 * px_to_scene_y,
            ),
            egui::vec2(side_scene_x, side_scene_y),
        );
        ui.painter().image(
            texture.id(),
            preview_rect,
            Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        Some(snapped_center)
    }

    fn draw_circle_outline_at(
        &self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        center_scene_pos: egui::Pos2,
    ) {
        let Some(page_idx) = canvas.page_index_at_scene_pos(center_scene_pos) else {
            return;
        };
        let Some(page_rect) = canvas.page_scene_rect(page_idx) else {
            return;
        };
        let Some([overlay_w, overlay_h]) = canvas.overlay_size(page_idx) else {
            return;
        };
        if overlay_w == 0 || overlay_h == 0 {
            return;
        }
        let radius_x_scene =
            self.brush_base.radius_px() as f32 * (page_rect.width() / overlay_w as f32);
        let radius_y_scene =
            self.brush_base.radius_px() as f32 * (page_rect.height() / overlay_h as f32);
        let radius_scene = ((radius_x_scene + radius_y_scene) * 0.5).max(0.5);
        ui.ctx()
            .output_mut(|out| out.cursor_icon = egui::CursorIcon::None);
        ui.painter().circle_stroke(
            center_scene_pos,
            radius_scene,
            egui::Stroke::new(2.0, Color32::WHITE),
        );
        ui.painter().circle_stroke(
            center_scene_pos,
            (radius_scene - 1.0).max(0.5),
            egui::Stroke::new(1.0, Color32::BLACK),
        );
    }
}

impl CleaningTool for StampTool {
    fn tool_id(&self) -> &'static str {
        "stamp"
    }

    fn title(&self) -> &'static str {
        "Штамп"
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.brush_base.set_space_pan_active(false);
        self.rect_start = None;
        self.rect_current = None;
        self.rect_erase = false;
        self.clear_runtime_stroke_state();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        ui.label("Исходник");
        let mut selected = self.selected_source_idx.map(|idx| idx + 1).unwrap_or(0);
        WheelComboBox::from_id_salt("cleaning_stamp_source_dir")
            .selected_text(
                self.selected_source_name()
                    .map(str::to_string)
                    .unwrap_or_else(|| "—".to_string()),
            )
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut selected, 0, "—");
                for (idx, name) in self.source_dirs.iter().enumerate() {
                    ui.selectable_value(&mut selected, idx + 1, name);
                }
            });
        let next_idx = if selected == 0 {
            None
        } else {
            Some(selected - 1)
        };
        if next_idx != self.selected_source_idx {
            self.set_selected_source_idx(next_idx);
        }

        let mut radius = self.brush_base.radius_px();
        if ui
            .add(WheelSlider::new(&mut radius, 1..=200).text("Размер"))
            .changed()
        {
            self.brush_base.set_radius_px(radius);
        }

        let mut preview = self.preview_opacity as i32;
        if ui
            .add(WheelSlider::new(&mut preview, 0..=255).text("Превью"))
            .changed()
        {
            self.preview_opacity = preview.clamp(0, 255) as u8;
        }

        ui.horizontal(|ui| {
            ui.label("Смещение Y");
            ui.add(WheelSpinBox::new(&mut self.y_offset).speed(1.0));
        });

        if self.selected_source_name().is_none() {
            ui.small("Выберите папку в `alt_vers`.");
        } else if self.source_loading_for_current_page() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.small("Кэш alt-версии для текущей страницы загружается...");
            });
        } else if let Some(status) = self.source_status_text.as_ref() {
            ui.small(status);
        }

        ui.separator();
        ui.small("ЛКМ: штамп, Ctrl+ЛКМ: прямоугольник, Shift+ЛКМ: прямоуг. стирание");
    }

    fn on_wheel_event(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        self.brush_base.handle_wheel(delta_y, modifiers)
    }

    fn on_key_event(&mut self, ctx: &egui::Context) -> bool {
        self.brush_base.handle_size_shortcuts(ctx)
    }

    fn set_space_pan_active(&mut self, active: bool) {
        self.brush_base.set_space_pan_active(active);
    }

    fn space_pan_active(&self) -> bool {
        self.brush_base.space_pan_active()
    }

    fn stroke_begin(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        if self.brush_base.should_ignore_drawing() {
            return;
        }
        if point.modifiers.ctrl || point.modifiers.shift {
            self.rect_start = Some(point);
            self.rect_current = Some(point);
            self.rect_erase = point.modifiers.shift;
            return;
        }
        let _ = self.begin_scratch_stroke(canvas, point, false);
    }

    fn stroke_update(&mut self, canvas: &mut CanvasView, from: StrokePoint, to: StrokePoint) {
        if self.brush_base.should_ignore_drawing() {
            return;
        }
        if self.rect_start.is_some() {
            self.rect_current = Some(to);
            return;
        }
        self.last_stroke_point = Some(to);
        self.paint_scratch_segment(canvas, from, to);
    }

    fn stroke_end(&mut self, canvas: &mut CanvasView) {
        if self.rect_start.is_some() {
            self.commit_rect(canvas);
            self.rect_start = None;
            self.rect_current = None;
            self.rect_erase = false;
        }
        self.finish_active_stroke(canvas);
        self.commit_touched_pages(canvas);
    }

    fn secondary_click(
        &mut self,
        canvas: &mut CanvasView,
        _project: &ProjectData,
        point: StrokePoint,
    ) -> bool {
        if self.brush_base.should_ignore_drawing() {
            return false;
        }
        if self.begin_scratch_stroke(canvas, point, true) {
            self.finish_active_stroke(canvas);
            self.commit_touched_pages(canvas);
            return true;
        }
        false
    }

    fn wants_primary_stroke(&self, point: StrokePoint) -> bool {
        if self.brush_base.should_ignore_drawing() {
            return false;
        }
        if point.modifiers.ctrl || point.modifiers.shift {
            return true;
        }
        self.selected_source_idx.is_some()
    }

    fn block_canvas_drag_scroll_on_primary(&self) -> bool {
        !self.brush_base.space_pan_active()
    }

    fn block_canvas_drag_scroll_on_secondary(&self) -> bool {
        !self.brush_base.space_pan_active()
    }

    fn block_canvas_zoom_on_ctrl_primary(&self) -> bool {
        true
    }

    fn suppress_base_overlay_render(&self) -> bool {
        self.scratch
            .as_ref()
            .is_some_and(|scratch| !scratch.preview_into_canvas)
    }

    fn draw_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        self.sync_source_dirs(project);
        self.poll_source_loader();
        self.ensure_current_page_source_prefetch(canvas);
        if self.scratch.is_some() || self.source_load_pending.is_some() {
            ctx.request_repaint();
        }
    }

    fn draw_cursor(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<egui::Pos2>,
    ) {
        self.draw_scratch_preview(ui, canvas);
        if let (Some(start), Some(current)) = (self.rect_start, self.rect_current)
            && start.page_idx == current.page_idx
        {
            let rect = Rect::from_two_pos(start.scene_pos, current.scene_pos);
            if rect.is_positive() {
                ui.painter().rect_stroke(
                    rect,
                    0.0,
                    egui::Stroke::new(1.0, Color32::from_gray(160)),
                    egui::StrokeKind::Outside,
                );
            }
        }
        if self.brush_base.should_ignore_drawing() {
            return;
        }
        let Some(pointer_scene_pos) = pointer_scene_pos else {
            return;
        };

        if let Some(snapped_center) = self.draw_cursor_preview(ui, canvas, pointer_scene_pos) {
            self.draw_circle_outline_at(ui, canvas, snapped_center);
        } else {
            self.brush_base
                .draw_circle_cursor(ui, canvas, Some(pointer_scene_pos));
        }
    }

    fn ensure_hover_overlay(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        if self.brush_base.should_ignore_drawing() {
            return;
        }
        let _ = BrushToolBase::ensure_overlay_under_point(canvas, point);
    }

    fn bubble_occluder(
        &self,
        canvas: &CanvasView,
        pointer_scene_pos: Option<egui::Pos2>,
    ) -> Option<CleaningCursorOccluder> {
        if self.brush_base.should_ignore_drawing() || self.rect_start.is_some() {
            return None;
        }
        self.brush_base.bubble_occluder(canvas, pointer_scene_pos)
    }
}

fn list_alt_version_dirs(base_dir: &Path) -> Vec<String> {
    if !base_dir.is_dir() {
        return Vec::new();
    }
    let mut folders = Vec::new();
    let Ok(entries) = fs::read_dir(base_dir) else {
        return folders;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            folders.push(name.to_string());
        }
    }
    folders.sort_by_key(|name| name.to_ascii_lowercase());
    folders
}

fn list_source_images(dir: &Path) -> Vec<PathBuf> {
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut entries = Vec::new();
    let Ok(read_dir) = fs::read_dir(dir) else {
        return entries;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
            entries.push(path);
        }
    }
    entries.sort_by(|a, b| numeric_file_sort(a, b));
    entries
}

fn numeric_file_sort(a: &Path, b: &Path) -> Ordering {
    numeric_file_key(a).cmp(&numeric_file_key(b))
}

fn numeric_file_key(path: &Path) -> (u8, u64, String, u8, String) {
    let base = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let ext_weight = match ext.as_str() {
        "png" => 0,
        "jpg" | "jpeg" => 1,
        _ => 2,
    };
    if !stem.is_empty() && stem.chars().all(|ch| ch.is_ascii_digit()) {
        return (
            0,
            stem.parse::<u64>().unwrap_or(0),
            String::new(),
            ext_weight,
            base.to_ascii_lowercase(),
        );
    }
    (
        1,
        0,
        stem.to_ascii_lowercase(),
        ext_weight,
        base.to_ascii_lowercase(),
    )
}

fn expand_overlay_rect(
    rect: OverlayRectPx,
    max_w: usize,
    max_h: usize,
    pad: usize,
) -> OverlayRectPx {
    let x0 = rect.x.saturating_sub(pad);
    let y0 = rect.y.saturating_sub(pad);
    let x1 = rect.x.saturating_add(rect.w).saturating_add(pad).min(max_w);
    let y1 = rect.y.saturating_add(rect.h).saturating_add(pad).min(max_h);
    OverlayRectPx {
        x: x0,
        y: y0,
        w: x1.saturating_sub(x0),
        h: y1.saturating_sub(y0),
    }
}

fn overlay_rect_to_scene_rect(
    page_scene_rect: Rect,
    overlay_w: usize,
    overlay_h: usize,
    rect: OverlayRectPx,
) -> Option<Rect> {
    if overlay_w == 0 || overlay_h == 0 || rect.w == 0 || rect.h == 0 {
        return None;
    }
    let u0 = rect.x as f32 / overlay_w as f32;
    let v0 = rect.y as f32 / overlay_h as f32;
    let u1 = (rect.x + rect.w) as f32 / overlay_w as f32;
    let v1 = (rect.y + rect.h) as f32 / overlay_h as f32;
    let min = egui::pos2(
        page_scene_rect.left() + page_scene_rect.width() * u0,
        page_scene_rect.top() + page_scene_rect.height() * v0,
    );
    let max = egui::pos2(
        page_scene_rect.left() + page_scene_rect.width() * u1,
        page_scene_rect.top() + page_scene_rect.height() * v1,
    );
    let rect = Rect::from_min_max(min, max);
    rect.is_positive().then_some(rect)
}

fn extract_overlay_chunk(source: &egui::ColorImage, rect: OverlayRectPx) -> egui::ColorImage {
    let mut out = egui::ColorImage::filled([rect.w, rect.h], Color32::TRANSPARENT);
    let src_w = source.size[0];
    let src_h = source.size[1];
    for y in 0..rect.h {
        let sy = rect.y + y;
        if sy >= src_h {
            continue;
        }
        let src_row = sy.saturating_mul(src_w);
        let dst_row = y.saturating_mul(rect.w);
        for x in 0..rect.w {
            let sx = rect.x + x;
            if sx >= src_w {
                continue;
            }
            let src_idx = src_row.saturating_add(sx);
            let dst_idx = dst_row.saturating_add(x);
            if let (Some(src_px), Some(dst_px)) =
                (source.pixels.get(src_idx), out.pixels.get_mut(dst_idx))
            {
                *dst_px = *src_px;
            }
        }
    }
    out
}

fn map_overlay_to_local(rect: &OverlayRectPx, x: usize, y: usize) -> Option<(usize, usize)> {
    let local_x = x.checked_sub(rect.x)?;
    let local_y = y.checked_sub(rect.y)?;
    if local_x >= rect.w || local_y >= rect.h {
        return None;
    }
    Some((local_x, local_y))
}

// All parameters are independent brush stroke properties; grouping would obscure painting intent.
#[allow(clippy::too_many_arguments)]
fn stamp_segment_on_image(
    dst: &mut egui::ColorImage,
    source: &RgbaImage,
    dst_overlay_rect: OverlayRectPx,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    y_offset: i32,
) {
    if radius <= 0 {
        return;
    }
    let dx = (x1 - x0) as f32;
    let dy = (y1 - y0) as f32;
    let distance = dx.hypot(dy);
    let spacing = (radius as f32 * STAMP_SPACING_FACTOR).max(1.0);
    let steps = ((distance / spacing).floor() as usize).max(1);
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let cx = (x0 as f32 + dx * t).round() as i32;
        let cy = (y0 as f32 + dy * t).round() as i32;
        stamp_circle_to_image(dst, source, dst_overlay_rect, cx, cy, radius, y_offset);
    }
}

fn stamp_circle_to_image(
    dst: &mut egui::ColorImage,
    source: &RgbaImage,
    dst_overlay_rect: OverlayRectPx,
    center_x: i32,
    center_y: i32,
    radius: i32,
    y_offset: i32,
) {
    if radius <= 0 {
        return;
    }
    let src_w = source.width() as i32;
    let src_h = source.height() as i32;
    let src_raw = source.as_raw();

    let left = center_x - radius;
    let top = center_y - radius;
    let right = center_x + radius - 1;
    let bottom = center_y + radius - 1;
    let Some(bounds) = intersect_overlay_bounds(dst_overlay_rect, left, top, right, bottom) else {
        return;
    };

    let r2 = radius * radius;
    let src_center_y = center_y + y_offset;
    for oy in bounds.y..(bounds.y + bounds.h) {
        let local_y = oy as i32 - top;
        let dy = local_y - radius;
        let dst_local_y = oy.saturating_sub(dst_overlay_rect.y);
        let dst_row = dst_local_y.saturating_mul(dst.size[0]);
        for ox in bounds.x..(bounds.x + bounds.w) {
            let local_x = ox as i32 - left;
            let dx = local_x - radius;
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let src_x = center_x + dx;
            let src_y = src_center_y + dy;
            if src_x < 0 || src_y < 0 || src_x >= src_w || src_y >= src_h {
                continue;
            }
            let src_idx = ((src_y as usize)
                .saturating_mul(src_w as usize)
                .saturating_add(src_x as usize))
            .saturating_mul(4);
            let sr = *src_raw.get(src_idx).unwrap_or(&0);
            let sg = *src_raw.get(src_idx + 1).unwrap_or(&0);
            let sb = *src_raw.get(src_idx + 2).unwrap_or(&0);
            let sa = *src_raw.get(src_idx + 3).unwrap_or(&0);
            if sa == 0 {
                continue;
            }
            let dst_local_x = ox.saturating_sub(dst_overlay_rect.x);
            let dst_idx = dst_row.saturating_add(dst_local_x);
            if let Some(dst_px) = dst.pixels.get_mut(dst_idx) {
                *dst_px =
                    blend_source_over(*dst_px, Color32::from_rgba_unmultiplied(sr, sg, sb, sa));
            }
        }
    }
}

fn segment_dirty_overlay_rect(
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    radius: usize,
    clip: OverlayRectPx,
) -> Option<OverlayRectPx> {
    let min_x = x0.min(x1).saturating_sub(radius);
    let min_y = y0.min(y1).saturating_sub(radius);
    let max_x = x0
        .max(x1)
        .saturating_add(radius)
        .min(clip.x.saturating_add(clip.w).saturating_sub(1));
    let max_y = y0
        .max(y1)
        .saturating_add(radius)
        .min(clip.y.saturating_add(clip.h).saturating_sub(1));
    if max_x < min_x || max_y < min_y {
        return None;
    }
    intersect_overlay_bounds(clip, min_x as i32, min_y as i32, max_x as i32, max_y as i32)
}

fn intersect_overlay_bounds(
    clip: OverlayRectPx,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
) -> Option<OverlayRectPx> {
    let clip_left = clip.x as i32;
    let clip_top = clip.y as i32;
    let clip_right = clip.x.saturating_add(clip.w).saturating_sub(1) as i32;
    let clip_bottom = clip.y.saturating_add(clip.h).saturating_sub(1) as i32;
    let x0 = left.max(clip_left);
    let y0 = top.max(clip_top);
    let x1 = right.min(clip_right);
    let y1 = bottom.min(clip_bottom);
    if x1 < x0 || y1 < y0 {
        return None;
    }
    Some(OverlayRectPx {
        x: x0 as usize,
        y: y0 as usize,
        w: (x1 - x0 + 1) as usize,
        h: (y1 - y0 + 1) as usize,
    })
}

fn extract_local_chunk(
    source: &egui::ColorImage,
    source_overlay_rect: OverlayRectPx,
    desired_overlay_rect: OverlayRectPx,
) -> egui::ColorImage {
    let mut out = egui::ColorImage::filled(
        [desired_overlay_rect.w, desired_overlay_rect.h],
        Color32::TRANSPARENT,
    );
    for y in 0..desired_overlay_rect.h {
        let overlay_y = desired_overlay_rect.y + y;
        let Some(local_y) = overlay_y.checked_sub(source_overlay_rect.y) else {
            continue;
        };
        if local_y >= source_overlay_rect.h {
            continue;
        }
        let src_row = local_y.saturating_mul(source.size[0]);
        let dst_row = y.saturating_mul(desired_overlay_rect.w);
        for x in 0..desired_overlay_rect.w {
            let overlay_x = desired_overlay_rect.x + x;
            let Some(local_x) = overlay_x.checked_sub(source_overlay_rect.x) else {
                continue;
            };
            if local_x >= source_overlay_rect.w {
                continue;
            }
            let src_idx = src_row.saturating_add(local_x);
            let dst_idx = dst_row.saturating_add(x);
            if let (Some(src_px), Some(dst_px)) =
                (source.pixels.get(src_idx), out.pixels.get_mut(dst_idx))
            {
                *dst_px = *src_px;
            }
        }
    }
    out
}

fn build_rect_chunk_from_source(
    source: &RgbaImage,
    rect: OverlayRectPx,
    y_offset: i32,
) -> egui::ColorImage {
    let mut out = egui::ColorImage::filled([rect.w, rect.h], Color32::TRANSPARENT);
    let src_w = source.width() as i32;
    let src_h = source.height() as i32;
    let src_raw = source.as_raw();
    for y in 0..rect.h {
        let src_y = rect.y as i32 + y as i32 + y_offset;
        if src_y < 0 || src_y >= src_h {
            continue;
        }
        let dst_row = y.saturating_mul(rect.w);
        for x in 0..rect.w {
            let src_x = rect.x as i32 + x as i32;
            if src_x < 0 || src_x >= src_w {
                continue;
            }
            let src_idx = ((src_y as usize)
                .saturating_mul(src_w as usize)
                .saturating_add(src_x as usize))
            .saturating_mul(4);
            let dst_idx = dst_row.saturating_add(x);
            if let Some(dst_px) = out.pixels.get_mut(dst_idx) {
                *dst_px = Color32::from_rgba_unmultiplied(
                    *src_raw.get(src_idx).unwrap_or(&0),
                    *src_raw.get(src_idx + 1).unwrap_or(&0),
                    *src_raw.get(src_idx + 2).unwrap_or(&0),
                    *src_raw.get(src_idx + 3).unwrap_or(&0),
                );
            }
        }
    }
    out
}

fn build_stamp_patch_with_opacity(
    source: &RgbaImage,
    center_x: i32,
    center_y: i32,
    radius: i32,
    y_offset: i32,
    opacity: u8,
) -> egui::ColorImage {
    let side = (radius.max(1) * 2) as usize;
    let mut out = egui::ColorImage::filled([side, side], Color32::TRANSPARENT);
    if opacity == 0 {
        return out;
    }
    let src_w = source.width() as i32;
    let src_h = source.height() as i32;
    let src_raw = source.as_raw();
    let r2 = radius * radius;
    let src_center_y = center_y + y_offset;

    for y in 0..side {
        let dy = y as i32 - radius;
        let src_y = src_center_y + dy;
        let dst_row = y.saturating_mul(side);
        for x in 0..side {
            let dx = x as i32 - radius;
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let src_x = center_x + dx;
            if src_x < 0 || src_y < 0 || src_x >= src_w || src_y >= src_h {
                continue;
            }
            let src_idx = ((src_y as usize)
                .saturating_mul(src_w as usize)
                .saturating_add(src_x as usize))
            .saturating_mul(4);
            let mut a = *src_raw.get(src_idx + 3).unwrap_or(&0);
            if a == 0 {
                continue;
            }
            a = (((a as u16) * (opacity as u16)) / 255) as u8;
            let dst_idx = dst_row.saturating_add(x);
            if let Some(dst_px) = out.pixels.get_mut(dst_idx) {
                *dst_px = Color32::from_rgba_unmultiplied(
                    *src_raw.get(src_idx).unwrap_or(&0),
                    *src_raw.get(src_idx + 1).unwrap_or(&0),
                    *src_raw.get(src_idx + 2).unwrap_or(&0),
                    a,
                );
            }
        }
    }
    out
}

fn blend_source_over(dst: Color32, src: Color32) -> Color32 {
    let sa = src.a() as f32 / 255.0;
    if sa <= f32::EPSILON {
        return dst;
    }
    let da = dst.a() as f32 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= f32::EPSILON {
        return Color32::TRANSPARENT;
    }
    let sr = src.r() as f32 / 255.0;
    let sg = src.g() as f32 / 255.0;
    let sb = src.b() as f32 / 255.0;
    let dr = dst.r() as f32 / 255.0;
    let dg = dst.g() as f32 / 255.0;
    let db = dst.b() as f32 / 255.0;
    let out_r = (sr * sa + dr * da * (1.0 - sa)) / out_a;
    let out_g = (sg * sa + dg * da * (1.0 - sa)) / out_a;
    let out_b = (sb * sa + db * da * (1.0 - sa)) / out_a;
    Color32::from_rgba_unmultiplied(
        (out_r * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_g * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_b * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_a * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}
