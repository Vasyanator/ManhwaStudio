/*
FILE HEADER (cleaning/tools/stamp.rs)
- Назначение: порт инструмента "Штамп" для вкладки cleaning
  (копирование пикселей из подпапок `alt_vers` в clean-overlay).
- Ключевые сущности:
  - `StampTool`: состояние UI, режима источника, фонового кэша страниц и активного штриха/прямоугольника.
  - `StampMode`: выбор между папкой `alt_vers` и clone-stamp из текущей страницы.
  - `CurrentImageStampSource`: какой слой текущей страницы используется как источник штампа.
  - `StampScratch`: временный буфер штриха по видимой области (как у `zamazka`, без спама в модель на каждый move).
  - `SourceLoadRequest/SourceLoadResult`: очередь фоновой загрузки одной текущей alt-страницы.
- Notes (English, per AGENTS.md §3):
  - Source pages are validated against BOTH overlay dimensions; a zero expected dimension means the
    overlay size is not known yet (`CanvasView::overlay_size` returned `None`), never "skip".
  - The `CurrentImage` commit writes a DENSE overlay: `overlay_pixel_for_final_color` raises the
    solved alpha to at least the dab coverage, because the page and the overlay are separate
    LINEAR-sampled quads and a minimum-alpha (stencil-shaped) solution ghosts under resampling.
  - `Color32` is premultiplied sRGBA; every colour helper here works on `to_srgba_unmultiplied()`
    and premultiplies exactly once.
  - `CurrentImage` mode paints TWO canvas markers: the fixed anchor beacon
    (`draw_source_anchor_marker`) and a MOVING indicator of the point currently being sampled
    (`draw_sampled_source_marker`, a dashed brush-radius ring plus a centre crosshair). The moving
    one takes its position from `stamp_sampled_overlay_xy`, which goes through `stamp_source_xy`
    itself, so it cannot disagree with the pixels being read. With no active stroke the offset is
    zero, so it rests on the beacon and only travels away from it while the LMB stroke runs.
- Поведение:
  - ЛКМ: штамп кистью; `Shift+ЛКМ`: временный ластик; `Ctrl+ЛКМ`: прямоугольный штамп;
    `Ctrl+Shift+ЛКМ`: прямоугольное стирание.
  - В режиме alt-версии источник берётся из выбранной папки `project/alt_vers/<name>`.
  - В режиме текущей картинки ПКМ ставит точку штампа, а ЛКМ копирует пиксели от этой точки
    с Photoshop-like смещением и опциональным поворотом.
  - На панели инструмента показывается статус, когда кэш источника текущей страницы ещё грузится.
  - Во время штриха используется локальный scratch-буфер и коммит в модель только в конце штриха.
  - Если сторона scratch-превью больше safe-порога (`16k`), инструмент не создаёт giant texture в `egui`,
    а показывает превью инкрементально через `replace_overlay_region_local`, обходя лимит texture size.
- Потоки:
  - Отдельный worker декодирует alt-страницу в `RgbaImage` и отправляет результат через channel.
*/
use super::base::{BrushToolBase, CleaningCursorOccluder, CleaningTool, StrokePoint};
use crate::canvas::{CanvasView, OverlayRectPx};
use crate::project::ProjectData;
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
use ms_thread as thread;

const STAMP_PREVIEW_TEXTURE_OPTIONS: TextureOptions = TextureOptions::LINEAR;
const STAMP_CURSOR_TEXTURE_OPTIONS: TextureOptions = TextureOptions::NEAREST;
const STAMP_SCRATCH_TEXTURE_SIDE_LIMIT: usize = 16_000;
const STAMP_SPACING_FACTOR: f32 = 0.6;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum StampMode {
    AltVersion,
    CurrentImage,
}

impl StampMode {
    fn title(self) -> &'static str {
        match self {
            Self::AltVersion => t!("cleaning.tools.stamp.mode_alt_version"),
            Self::CurrentImage => t!("cleaning.tools.stamp.mode_current_image"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CurrentImageStampSource {
    OriginalOnly,
    OverlayOnly,
    OriginalAndOverlay,
}

impl CurrentImageStampSource {
    fn title(self) -> &'static str {
        match self {
            Self::OriginalOnly => t!("cleaning.tools.stamp.source_option"),
            Self::OverlayOnly => t!("cleaning.tools.stamp.clean_option"),
            Self::OriginalAndOverlay => t!("cleaning.tools.stamp.source_plus_clean_option"),
        }
    }
}

/// One background decode job for a source page.
///
/// `overlay_size` is the `[width, height]` in overlay pixels the decoded page is expected to
/// match. A dimension of `0` means the canvas could not report that dimension yet (no overlay
/// allocated for the page — `CanvasView::overlay_size` returns `None` then), so there is
/// nothing to compare against and that axis is left unchecked; it never means "skip the check".
/// That accessor is all-or-nothing, so in practice the pair is either fully known or `[0, 0]`.
#[derive(Clone)]
struct SourceLoadRequest {
    job_id: u64,
    page_idx: usize,
    overlay_size: [usize; 2],
    path: PathBuf,
}

/// Outcome of one `SourceLoadRequest`: either the decoded page or a user-facing error.
///
/// `overlay_size` echoes the request so the GUI thread can cache the expectation the image
/// was validated against.
struct SourceLoadResult {
    job_id: u64,
    page_idx: usize,
    overlay_size: [usize; 2],
    path: PathBuf,
    image: Option<Arc<RgbaImage>>,
    error: Option<String>,
}

/// A decoded source page kept for one page index, valid for the `overlay_size` it was
/// validated against (see `SourceLoadRequest` for the meaning of a zero dimension).
struct CachedSourcePage {
    page_idx: usize,
    overlay_size: [usize; 2],
    path: PathBuf,
    image: Arc<RgbaImage>,
}

#[derive(Debug, Clone, Copy)]
struct StampAnchor {
    page_idx: usize,
    overlay_xy: [f32; 2],
}

struct StampScratch {
    page_idx: usize,
    overlay_rect: OverlayRectPx,
    scene_rect: Rect,
    base_image: egui::ColorImage,
    image: egui::ColorImage,
    mask: Vec<f32>,
    texture: Option<TextureHandle>,
    texture_options: TextureOptions,
    texture_dirty: bool,
    preview_into_canvas: bool,
    painted: bool,
}

pub struct StampTool {
    brush_base: BrushToolBase,
    mode: StampMode,
    current_image_source: CurrentImageStampSource,
    preview_opacity: u8,
    y_offset: i32,
    rotation_degrees: f32,
    rotation_wheel_accum: f32,
    snap_anchor_position: bool,
    source_anchor: Option<StampAnchor>,
    active_stroke_origin: Option<StrokePoint>,
    active_stroke_origin_overlay: Option<[i32; 2]>,
    rect_start: Option<StrokePoint>,
    rect_current: Option<StrokePoint>,
    rect_erase: bool,
    scratch: Option<StampScratch>,
    active_source: Option<Arc<RgbaImage>>,
    active_erase: bool,
    last_stroke_point: Option<StrokePoint>,
    touched_pages: HashSet<usize>,
    alt_vers_dir: Option<PathBuf>,
    project_page_paths: Vec<PathBuf>,
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
                    overlay_size: request.overlay_size,
                    path: request.path.clone(),
                    image: None,
                    error: None,
                };

                match image::open(&request.path) {
                    Ok(decoded) => {
                        let rgba = decoded.to_rgba8();
                        let size = [rgba.width() as usize, rgba.height() as usize];
                        // Both axes are validated: a page of the right width but the wrong height
                        // silently samples the wrong rows. An expected dimension of 0 is "unknown"
                        // (no overlay allocated yet), so that axis carries no expectation to check.
                        let width_ok = request.overlay_size[0] == 0 || size[0] == request.overlay_size[0];
                        let height_ok = request.overlay_size[1] == 0 || size[1] == request.overlay_size[1];
                        if width_ok && height_ok {
                            result.image = Some(Arc::new(rgba));
                        } else {
                            result.error = Some(tf!("cleaning.tools.stamp.source_size_mismatch_error", width = size[0], height = size[1], expected_width = request.overlay_size[0], expected_height = request.overlay_size[1]));
                        }
                    }
                    Err(error) => {
                        result.error = Some(tf!("cleaning.tools.stamp.open_error", request = request.path.display(), error = error));
                    }
                }
                let _ = result_tx.send(result);
            }
        });

        Self {
            brush_base: BrushToolBase::default(),
            mode: StampMode::CurrentImage,
            current_image_source: CurrentImageStampSource::OriginalAndOverlay,
            preview_opacity: 160,
            y_offset: 0,
            rotation_degrees: 0.0,
            rotation_wheel_accum: 0.0,
            snap_anchor_position: true,
            source_anchor: None,
            active_stroke_origin: None,
            active_stroke_origin_overlay: None,
            rect_start: None,
            rect_current: None,
            rect_erase: false,
            scratch: None,
            active_source: None,
            active_erase: false,
            last_stroke_point: None,
            touched_pages: HashSet::new(),
            alt_vers_dir: None,
            project_page_paths: Vec::new(),
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
        self.active_stroke_origin = None;
        self.active_stroke_origin_overlay = None;
    }

    fn handle_rotation_wheel(&mut self, delta_y: f32, fast_step: bool) -> bool {
        if delta_y.abs() <= f32::EPSILON {
            return true;
        }
        const WHEEL_NOTCH: f32 = 40.0;
        const ROTATION_STEP_DEGREES: f32 = 0.5;
        self.rotation_wheel_accum += delta_y;
        let steps = (self.rotation_wheel_accum / WHEEL_NOTCH).trunc();
        if steps.abs() <= f32::EPSILON {
            return true;
        }
        self.rotation_wheel_accum -= steps * WHEEL_NOTCH;
        let multiplier = if fast_step { 5.0 } else { 1.0 };
        self.rotation_degrees = normalize_rotation_degrees(
            self.rotation_degrees + steps * ROTATION_STEP_DEGREES * multiplier,
        );
        self.cursor_texture = None;
        true
    }

    fn set_mode(&mut self, mode: StampMode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.source_cache = None;
        self.source_load_pending = None;
        self.source_status_text = None;
        self.cursor_texture = None;
        self.clear_runtime_stroke_state();
    }

    fn effective_erase(&self, point: StrokePoint) -> bool {
        point.modifiers.shift
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
                            overlay_size: result.overlay_size,
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
        let next_page_paths = project
            .pages
            .iter()
            .map(|page| page.path.clone())
            .collect::<Vec<_>>();
        if self.project_page_paths != next_page_paths {
            self.project_page_paths = next_page_paths;
            if self.mode == StampMode::CurrentImage {
                self.source_cache = None;
                self.source_load_pending = None;
                self.source_status_text = None;
            }
        }

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
        match self.mode {
            StampMode::AltVersion => self.source_paths.get(page_idx).cloned(),
            StampMode::CurrentImage => self.project_page_paths.get(page_idx).cloned(),
        }
    }

    fn source_required_for_paint(&self) -> bool {
        self.mode == StampMode::AltVersion || self.mode == StampMode::CurrentImage
    }

    /// Whether a decoded source page matching `overlay_size` is already cached for `page_idx`.
    ///
    /// `overlay_size` is `[width, height]` in overlay pixels; `[0, 0]` means the overlay size is
    /// not known yet and no size expectation is enforced. Always `true` for modes that need no
    /// source image.
    fn source_ready_for_page(&self, page_idx: usize, overlay_size: [usize; 2]) -> bool {
        if !self.source_required_for_paint() {
            return true;
        }
        self.source_image_for_page(page_idx, overlay_size)
            .is_some()
    }

    /// Dispatches a background decode of the source page for `page_idx` unless the cache or the
    /// pending job already covers exactly this page, `overlay_size` and path.
    ///
    /// `overlay_size` is `[width, height]` in overlay pixels and becomes the size the decoded page
    /// is validated against; `[0, 0]` means unknown (see `SourceLoadRequest`). Sets
    /// `source_status_text` when no source file exists or the worker channel is gone.
    fn queue_source_load_for_page_if_needed(&mut self, page_idx: usize, overlay_size: [usize; 2]) {
        let Some(path) = self.source_path_for_page(page_idx) else {
            self.source_cache = None;
            self.source_load_pending = None;
            self.source_status_text = Some(match self.mode {
                StampMode::AltVersion => {
                    t!("cleaning.tools.stamp.no_alt_file_status").to_string()
                }
                StampMode::CurrentImage => t!("cleaning.tools.stamp.no_source_file_status").to_string(),
            });
            return;
        };

        if self.source_cache.as_ref().is_some_and(|cached| {
            cached.page_idx == page_idx
                && cached.overlay_size == overlay_size
                && cached.path == path
        }) {
            return;
        }

        if self.source_load_pending.as_ref().is_some_and(|pending| {
            pending.page_idx == page_idx
                && pending.overlay_size == overlay_size
                && pending.path == path
        }) {
            return;
        }

        self.source_cache = None;
        self.source_status_text = None;
        let request = SourceLoadRequest {
            job_id: self.next_source_job_id,
            page_idx,
            overlay_size,
            path,
        };
        self.next_source_job_id = self.next_source_job_id.saturating_add(1);
        if self.source_load_tx.send(request.clone()).is_ok() {
            self.source_load_pending = Some(request);
        } else {
            self.source_load_pending = None;
            self.source_status_text =
                Some(t!("cleaning.tools.stamp.alt_load_dispatch_error").to_string());
        }
    }

    /// Returns the cached source page for `page_idx` when it is usable at `overlay_size`.
    ///
    /// `overlay_size` is `[width, height]` in overlay pixels; a dimension of `0` is unknown (the
    /// canvas has no overlay for the page yet) and carries no expectation. Each known dimension is
    /// compared against the DECODED page's own size, never against the expectation the page was
    /// loaded with: a page decoded while the overlay size was still unknown was validated against
    /// nothing, so trusting that expectation would serve an unvalidated page the moment the real
    /// size becomes known. Comparing against the decoded size validates it exactly once, here.
    fn source_image_for_page(
        &self,
        page_idx: usize,
        overlay_size: [usize; 2],
    ) -> Option<Arc<RgbaImage>> {
        let cached = self.source_cache.as_ref()?;
        if cached.page_idx != page_idx {
            return None;
        }
        let decoded = [usize::try_from(cached.image.width()).ok()?, usize::try_from(cached.image.height()).ok()?];
        let compatible = |wanted: usize, actual: usize| wanted == 0 || wanted == actual;
        if !compatible(overlay_size[0], decoded[0]) || !compatible(overlay_size[1], decoded[1]) {
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
        if self.mode == StampMode::AltVersion && self.selected_source_name().is_none() {
            return;
        }
        if !self.source_required_for_paint() {
            return;
        }
        let page_idx = canvas.current_page_idx();
        self.current_page_idx = Some(page_idx);
        let overlay_size = canvas.overlay_size(page_idx).unwrap_or([0, 0]);
        self.queue_source_load_for_page_if_needed(page_idx, overlay_size);
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

        let overlay_size = canvas.overlay_size(point.page_idx).unwrap_or([0, 0]);
        if self.mode == StampMode::CurrentImage
            && !erase
            && self
                .source_anchor
                .is_some_and(|anchor| anchor.page_idx != point.page_idx)
        {
            self.source_status_text =
                Some(t!("cleaning.tools.stamp.set_stamp_point_page_hint").to_string());
            return false;
        }
        let source = if erase || !self.source_required_for_paint() {
            None
        } else if let Some(img) = self.source_image_for_page(point.page_idx, overlay_size) {
            Some(img)
        } else {
            self.queue_source_load_for_page_if_needed(point.page_idx, overlay_size);
            self.source_status_text =
                Some(t!("cleaning.tools.stamp.source_cache_not_ready_status").to_string());
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
        let mask_len = image.size[0].saturating_mul(image.size[1]);
        let preview_into_canvas = image.size[0] > STAMP_SCRATCH_TEXTURE_SIDE_LIMIT
            || image.size[1] > STAMP_SCRATCH_TEXTURE_SIDE_LIMIT;

        self.scratch = Some(StampScratch {
            page_idx: point.page_idx,
            overlay_rect: expanded_rect,
            scene_rect,
            base_image: image.clone(),
            image,
            mask: vec![0.0; mask_len],
            texture: None,
            texture_options: STAMP_PREVIEW_TEXTURE_OPTIONS,
            texture_dirty: true,
            preview_into_canvas,
            painted: false,
        });
        self.active_source = source;
        self.active_erase = erase;
        self.active_stroke_origin = Some(point);
        self.active_stroke_origin_overlay = canvas
            .scene_point_to_overlay_xy(point.page_idx, point.scene_pos)
            .map(|(x, y)| [x as i32, y as i32]);
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
            if self.active_erase {
                erase_stamp_segment_on_image(
                    &mut scratch.image,
                    &scratch.base_image,
                    &mut scratch.mask,
                    scratch.overlay_rect,
                    x0 as i32,
                    y0 as i32,
                    x1 as i32,
                    y1 as i32,
                    radius as i32,
                    self.brush_base.hardness(),
                );
            } else if self.mode == StampMode::AltVersion {
                let Some(source) = self.active_source.as_ref() else {
                    return;
                };
                stamp_segment_on_image(
                    &mut scratch.image,
                    &scratch.base_image,
                    &mut scratch.mask,
                    &StampSourceContext {
                        source: Some(source),
                        overlay: None,
                        mode: self.mode,
                        current_source: self.current_image_source,
                        source_anchor: None,
                        stroke_origin_overlay: None,
                        rotation_degrees: 0.0,
                        y_offset: self.y_offset,
                    },
                    scratch.overlay_rect,
                    x0 as i32,
                    y0 as i32,
                    x1 as i32,
                    y1 as i32,
                    radius as i32,
                    self.brush_base.hardness(),
                );
            } else {
                let Some(anchor) = self.source_anchor else {
                    self.source_status_text = Some(t!("cleaning.tools.stamp.set_stamp_point_hint").to_string());
                    return;
                };
                let Some(stroke_origin_overlay) = self.active_stroke_origin_overlay else {
                    return;
                };
                stamp_segment_on_image(
                    &mut scratch.image,
                    &scratch.base_image,
                    &mut scratch.mask,
                    &StampSourceContext {
                        source: self.active_source.as_ref(),
                        overlay: canvas.overlay_image(to.page_idx),
                        mode: self.mode,
                        current_source: self.current_image_source,
                        source_anchor: Some(anchor),
                        stroke_origin_overlay: Some(stroke_origin_overlay),
                        rotation_degrees: self.rotation_degrees,
                        y_offset: 0,
                    },
                    scratch.overlay_rect,
                    x0 as i32,
                    y0 as i32,
                    x1 as i32,
                    y1 as i32,
                    radius as i32,
                    self.brush_base.hardness(),
                );
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
        } else if self.mode == StampMode::AltVersion {
            let overlay_size = canvas.overlay_size(start.page_idx).unwrap_or([0, 0]);
            let Some(source) = self.source_image_for_page(start.page_idx, overlay_size) else {
                self.queue_source_load_for_page_if_needed(start.page_idx, overlay_size);
                self.source_status_text = Some(
                    t!("cleaning.tools.stamp.alt_cache_not_ready_status").to_string(),
                );
                return;
            };
            build_rect_chunk_from_source(&source, target, self.y_offset)
        } else {
            let overlay_size = canvas.overlay_size(start.page_idx).unwrap_or([0, 0]);
            if !self.source_ready_for_page(start.page_idx, overlay_size) {
                self.queue_source_load_for_page_if_needed(start.page_idx, overlay_size);
                self.source_status_text =
                    Some(t!("cleaning.tools.stamp.source_cache_not_ready_status").to_string());
                return;
            }
            let Some(anchor) = self.source_anchor else {
                self.source_status_text = Some(t!("cleaning.tools.stamp.set_stamp_point_hint").to_string());
                return;
            };
            build_rect_chunk_from_current_source(
                &StampSourceContext {
                    source: self.active_source.as_ref().or_else(|| {
                        self.source_cache
                            .as_ref()
                            .filter(|cached| cached.page_idx == start.page_idx)
                            .map(|cached| &cached.image)
                    }),
                    overlay: canvas.overlay_image(start.page_idx),
                    mode: self.mode,
                    current_source: self.current_image_source,
                    source_anchor: Some(anchor),
                    stroke_origin_overlay: canvas
                        .scene_point_to_overlay_xy(start.page_idx, start.scene_pos)
                        .map(|(x, y)| [x as i32, y as i32]),
                    rotation_degrees: self.rotation_degrees,
                    y_offset: 0,
                },
                target,
            )
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
        let source = self.source_image_for_page(page_idx, [overlay_w, overlay_h]);
        if self.source_required_for_paint() && source.is_none() {
            return None;
        }
        let (cx, cy) = canvas.scene_point_to_overlay_xy(page_idx, pointer_scene_pos)?;

        let patch = if self.mode == StampMode::AltVersion {
            let source = source.as_ref()?;
            build_stamp_patch_with_opacity(
                &StampSourceContext {
                    source: Some(source),
                    overlay: None,
                    mode: self.mode,
                    current_source: self.current_image_source,
                    source_anchor: None,
                    stroke_origin_overlay: None,
                    rotation_degrees: 0.0,
                    y_offset: self.y_offset,
                },
                cx as i32,
                cy as i32,
                radius as i32,
                self.brush_base.hardness(),
                self.preview_opacity,
            )
        } else {
            let anchor = self.source_anchor?;
            build_stamp_patch_with_opacity(
                &StampSourceContext {
                    source: source.as_ref(),
                    overlay: canvas.overlay_image(page_idx),
                    mode: self.mode,
                    current_source: self.current_image_source,
                    source_anchor: Some(anchor),
                    stroke_origin_overlay: Some([cx as i32, cy as i32]),
                    rotation_degrees: self.rotation_degrees,
                    y_offset: 0,
                },
                cx as i32,
                cy as i32,
                radius as i32,
                self.brush_base.hardness(),
                self.preview_opacity,
            )
        };
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

    /// Paints the MOVING indicator of the point the stamp is currently sampling from.
    ///
    /// `pointer_scene_pos` is the cursor in scene units. The destination overlay pixel is taken
    /// on the page under the cursor and mapped through `stamp_sampled_overlay_xy`; the marker is
    /// painted on the ANCHOR's page, which is where the pixels are read. Paint-only, no hitbox.
    ///
    /// Draws nothing outside `StampMode::CurrentImage`, without a stamp anchor, when the cursor
    /// is not over the anchor's page (no destination pixel exists there, so nothing is sampled
    /// either), or when the sampled pixel falls outside the overlay.
    fn draw_sampled_source_marker(&self, ui: &mut egui::Ui, canvas: &CanvasView, pointer_scene_pos: egui::Pos2) {
        // Number of dashes the ring is split into, kept constant so the per-frame shape count does
        // not grow with the brush radius or the zoom level.
        const RING_DASHES: f32 = 16.0;
        // Segments of the polyline the dashes are cut from; constant for the same reason.
        const RING_SEGMENTS: usize = 64;
        const CROSSHAIR_ARM_SCENE: f32 = 5.0;
        let dark = Color32::from_black_alpha(180);

        let Some(anchor) = self.source_anchor else {
            return;
        };
        let Some(dst_page_idx) = canvas.page_index_at_scene_pos(pointer_scene_pos) else {
            return;
        };
        // A CurrentImage stroke is refused when destination and source pages differ
        // (`begin_scratch_stroke`), so a cursor on another page gets no marker instead of a guess.
        if dst_page_idx != anchor.page_idx {
            return;
        }
        let Some((dst_x, dst_y)) = canvas.scene_point_to_overlay_xy(dst_page_idx, pointer_scene_pos)
        else {
            return;
        };
        let (Ok(dst_x), Ok(dst_y)) = (i32::try_from(dst_x), i32::try_from(dst_y)) else {
            return;
        };
        let Some((src_page_idx, src_overlay)) = stamp_sampled_overlay_xy(self.mode, self.source_anchor, self.rotation_degrees, self.active_stroke_origin_overlay, [dst_x, dst_y])
        else {
            return;
        };
        let Some(page_rect) = canvas.page_scene_rect(src_page_idx) else {
            return;
        };
        let Some([overlay_w, overlay_h]) = canvas.overlay_size(src_page_idx) else {
            return;
        };
        if overlay_w == 0 || overlay_h == 0 {
            return;
        }
        // `overlay_pos_to_scene_pos` CLAMPS to the page, so an out-of-page source point would be
        // pinned to the border and claim pixels are read there. Nothing is sampled outside the
        // overlay, so nothing is drawn either.
        let inside = usize::try_from(src_overlay[0]).is_ok_and(|x| x < overlay_w)
            && usize::try_from(src_overlay[1]).is_ok_and(|y| y < overlay_h);
        if !inside {
            return;
        }
        // Raw pixel index, NOT its centre: the anchor beacon is placed with the same convention,
        // so at zero offset the two markers coincide exactly instead of sitting half a pixel apart.
        let Some(center) = overlay_pos_to_scene_pos(canvas, src_page_idx, [src_overlay[0] as f32, src_overlay[1] as f32]) else {
            return;
        };

        // Same overlay-pixel -> scene-unit conversion the destination brush cursor uses, so both
        // rings show the same disc size.
        let radius_px = self.brush_base.radius_px().max(1) as f32;
        let radius_x_scene = radius_px * (page_rect.width() / overlay_w as f32);
        let radius_y_scene = radius_px * (page_rect.height() / overlay_h as f32);
        let radius_scene = ((radius_x_scene + radius_y_scene) * 0.5).max(2.0);

        let ring: Vec<egui::Pos2> = (0..=RING_SEGMENTS)
            .map(|step| {
                let angle = std::f32::consts::TAU * (step as f32) / (RING_SEGMENTS as f32);
                egui::pos2(center.x + radius_scene * angle.cos(), center.y + radius_scene * angle.sin())
            })
            .collect();
        // Dash and gap scale with the circumference so the dash COUNT stays put at any zoom.
        let dash = (std::f32::consts::TAU * radius_scene / (RING_DASHES * 2.0)).max(2.0);
        // Dark under light: the pair survives both black line art and white paper, and the dashed
        // ring plus the centre crosshair keep it distinct from the solid destination brush ring
        // and from the filled anchor beacon.
        ui.painter().extend(egui::Shape::dashed_line(&ring, egui::Stroke::new(3.0, dark), dash, dash));
        ui.painter().extend(egui::Shape::dashed_line(&ring, egui::Stroke::new(1.0, Color32::WHITE), dash, dash));
        for (width, color) in [(3.0, dark), (1.0, Color32::WHITE)] {
            ui.painter().line_segment(
                [
                    egui::pos2(center.x - CROSSHAIR_ARM_SCENE, center.y),
                    egui::pos2(center.x + CROSSHAIR_ARM_SCENE, center.y),
                ],
                egui::Stroke::new(width, color),
            );
            ui.painter().line_segment(
                [
                    egui::pos2(center.x, center.y - CROSSHAIR_ARM_SCENE),
                    egui::pos2(center.x, center.y + CROSSHAIR_ARM_SCENE),
                ],
                egui::Stroke::new(width, color),
            );
        }
    }
}

impl CleaningTool for StampTool {
    fn tool_id(&self) -> &'static str {
        "stamp"
    }

    fn title(&self) -> &'static str {
        t!("cleaning.tools.stamp.title")
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.brush_base.set_space_pan_active(false);
        self.rect_start = None;
        self.rect_current = None;
        self.rect_erase = false;
        self.clear_runtime_stroke_state();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(t!("cleaning.common.mode_label"));
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(StampMode::AltVersion.title())
                        .selected(self.mode == StampMode::AltVersion),
                )
                .clicked()
            {
                self.set_mode(StampMode::AltVersion);
            }
            if ui
                .add(
                    egui::Button::new(StampMode::CurrentImage.title())
                        .selected(self.mode == StampMode::CurrentImage),
                )
                .clicked()
            {
                self.set_mode(StampMode::CurrentImage);
            }
        });

        match self.mode {
            StampMode::AltVersion => {
                ui.label(t!("cleaning.tools.stamp.source_option"));
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
            }
            StampMode::CurrentImage => {
                ui.label(t!("cleaning.tools.stamp.pixel_source_label"));
                ui.horizontal_wrapped(|ui| {
                    for source in [
                        CurrentImageStampSource::OriginalOnly,
                        CurrentImageStampSource::OverlayOnly,
                        CurrentImageStampSource::OriginalAndOverlay,
                    ] {
                        if ui
                            .add(
                                egui::Button::new(source.title())
                                    .selected(self.current_image_source == source),
                            )
                            .clicked()
                        {
                            self.current_image_source = source;
                            self.source_status_text = None;
                        }
                    }
                });
                let mut rotation = self.rotation_degrees;
                if ui
                    .add(WheelSlider::new(&mut rotation, -180.0..=180.0).text(t!("cleaning.common.rotation_label")))
                    .changed()
                {
                    self.rotation_degrees = rotation.clamp(-180.0, 180.0);
                    self.cursor_texture = None;
                }
                ui.checkbox(&mut self.snap_anchor_position, t!("cleaning.tools.stamp.anchor_rounding_label"));
                if let Some(anchor) = self.source_anchor {
                    ui.small(tf!(
                        "cleaning.tools.stamp.stamp_point_status",
                        x = format!("{:.1}", anchor.overlay_xy[0]),
                        y = format!("{:.1}", anchor.overlay_xy[1])
                    ));
                } else {
                    ui.small(t!("cleaning.tools.stamp.place_stamp_point_hint"));
                }
            }
        }

        let mut radius = self.brush_base.radius_px();
        if ui
            .add(WheelSlider::new(&mut radius, 1..=200).text(t!("cleaning.common.size_label")))
            .changed()
        {
            self.brush_base.set_radius_px(radius);
        }

        let mut hardness = self.brush_base.hardness();
        let _ = ui.add(
            WheelSlider::new(&mut hardness, 0.0..=1.0)
                .text(t!("cleaning.common.hardness_label"))
                .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)),
        );
        let _ = self.brush_base.set_hardness(hardness);

        let mut preview = self.preview_opacity as i32;
        if ui
            .add(WheelSlider::new(&mut preview, 0..=255).text(t!("cleaning.common.preview_label")))
            .changed()
        {
            self.preview_opacity = preview.clamp(0, 255) as u8;
        }

        ui.horizontal(|ui| {
            ui.label(t!("cleaning.tools.stamp.offset_y_label"));
            ui.add(WheelSpinBox::new(&mut self.y_offset).speed(1.0));
        });

        if self.mode == StampMode::AltVersion && self.selected_source_name().is_none() {
            ui.small(t!("cleaning.tools.stamp.select_alt_folder_hint"));
        } else if self.source_loading_for_current_page() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.small(t!("cleaning.tools.stamp.source_cache_loading_status"));
            });
        } else if let Some(status) = self.source_status_text.as_ref() {
            ui.small(status);
        }

        ui.separator();
        ui.small(t!("cleaning.tools.stamp.controls_hint_1"));
        ui.small(t!("cleaning.tools.stamp.controls_hint_2"));
        ui.small(t!("cleaning.tools.stamp.controls_hint_3"));
    }

    fn on_wheel_event(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        self.brush_base.handle_wheel(delta_y, modifiers)
    }

    fn on_wheel_event_with_keys(
        &mut self,
        delta_y: f32,
        modifiers: egui::Modifiers,
        r_down: bool,
    ) -> bool {
        if r_down {
            return self.handle_rotation_wheel(delta_y, modifiers.shift);
        }
        self.rotation_wheel_accum = 0.0;
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
        if point.modifiers.ctrl {
            self.rect_start = Some(point);
            self.rect_current = Some(point);
            self.rect_erase = point.modifiers.shift;
            return;
        }
        if self.mode == StampMode::CurrentImage
            && self.source_anchor.is_none()
            && !point.modifiers.shift
        {
            self.source_status_text = Some(t!("cleaning.tools.stamp.set_stamp_point_hint").to_string());
            return;
        }
        let _ = self.begin_scratch_stroke(canvas, point, self.effective_erase(point));
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
        if self.mode == StampMode::CurrentImage {
            let overlay_xy = if self.snap_anchor_position {
                let Some((x, y)) =
                    canvas.scene_point_to_overlay_xy(point.page_idx, point.scene_pos)
                else {
                    return false;
                };
                [x as f32, y as f32]
            } else {
                let Some(pos) = scene_pos_to_overlay_pos(canvas, point.page_idx, point.scene_pos)
                else {
                    return false;
                };
                [pos.x, pos.y]
            };
            self.source_anchor = Some(StampAnchor {
                page_idx: point.page_idx,
                overlay_xy,
            });
            self.source_status_text = None;
            self.cursor_texture = None;
            return true;
        }
        false
    }

    fn wants_primary_stroke(&self, point: StrokePoint) -> bool {
        if self.brush_base.should_ignore_drawing() {
            return false;
        }
        if point.modifiers.ctrl {
            return true;
        }
        if point.modifiers.shift {
            return true;
        }
        match self.mode {
            StampMode::AltVersion => self.selected_source_idx.is_some(),
            StampMode::CurrentImage => self.source_anchor.is_some(),
        }
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
        draw_source_anchor_marker(ui, canvas, self.mode, self.source_anchor);
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

        // Painted above the fixed beacon (so the two stay readable where they overlap at zero
        // offset) and below the destination brush preview, which owns the cursor position.
        self.draw_sampled_source_marker(ui, canvas, pointer_scene_pos);

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

fn normalize_rotation_degrees(mut degrees: f32) -> f32 {
    while degrees > 180.0 {
        degrees -= 360.0;
    }
    while degrees < -180.0 {
        degrees += 360.0;
    }
    degrees
}

fn scene_pos_to_overlay_pos(
    canvas: &CanvasView,
    page_idx: usize,
    scene_pos: egui::Pos2,
) -> Option<egui::Pos2> {
    let page_rect = canvas.page_scene_rect(page_idx)?;
    let [overlay_w, overlay_h] = canvas.overlay_size(page_idx)?;
    if overlay_w == 0 || overlay_h == 0 || !page_rect.is_positive() {
        return None;
    }
    let u = ((scene_pos.x - page_rect.left()) / page_rect.width()).clamp(0.0, 1.0);
    let v = ((scene_pos.y - page_rect.top()) / page_rect.height()).clamp(0.0, 1.0);
    Some(egui::pos2(u * overlay_w as f32, v * overlay_h as f32))
}

fn overlay_pos_to_scene_pos(
    canvas: &CanvasView,
    page_idx: usize,
    overlay_pos: [f32; 2],
) -> Option<egui::Pos2> {
    let page_rect = canvas.page_scene_rect(page_idx)?;
    let [overlay_w, overlay_h] = canvas.overlay_size(page_idx)?;
    if overlay_w == 0 || overlay_h == 0 || !page_rect.is_positive() {
        return None;
    }
    let u = (overlay_pos[0] / overlay_w as f32).clamp(0.0, 1.0);
    let v = (overlay_pos[1] / overlay_h as f32).clamp(0.0, 1.0);
    Some(egui::pos2(
        page_rect.left() + page_rect.width() * u,
        page_rect.top() + page_rect.height() * v,
    ))
}

fn draw_source_anchor_marker(
    ui: &mut egui::Ui,
    canvas: &CanvasView,
    mode: StampMode,
    anchor: Option<StampAnchor>,
) {
    if mode != StampMode::CurrentImage {
        return;
    }
    let Some(anchor) = anchor else {
        return;
    };
    let Some(scene_pos) = overlay_pos_to_scene_pos(canvas, anchor.page_idx, anchor.overlay_xy)
    else {
        return;
    };
    let radius = 7.0;
    ui.painter().circle_filled(
        scene_pos,
        radius,
        Color32::from_rgba_unmultiplied(0, 0, 0, 90),
    );
    ui.painter()
        .circle_stroke(scene_pos, radius, egui::Stroke::new(2.0, Color32::WHITE));
    ui.painter().circle_stroke(
        scene_pos,
        radius - 1.5,
        egui::Stroke::new(1.0, Color32::BLACK),
    );
    ui.painter().line_segment(
        [
            egui::pos2(scene_pos.x - radius - 3.0, scene_pos.y),
            egui::pos2(scene_pos.x + radius + 3.0, scene_pos.y),
        ],
        egui::Stroke::new(1.0, Color32::WHITE),
    );
    ui.painter().line_segment(
        [
            egui::pos2(scene_pos.x, scene_pos.y - radius - 3.0),
            egui::pos2(scene_pos.x, scene_pos.y + radius + 3.0),
        ],
        egui::Stroke::new(1.0, Color32::WHITE),
    );
}

/// Overlay pixel the stamp CURRENTLY samples for one destination overlay pixel.
///
/// `dst_overlay` is a page-overlay pixel (not scene units) of the pixel that would be painted.
/// `stroke_origin_overlay` is the overlay pixel where the active stroke began, or `None` when no
/// stroke is running — the prospective origin is then the destination itself, the offset is zero,
/// and the sampled point collapses onto the anchor, i.e. the indicator rests on the beacon until
/// the first LMB press and travels away from it afterwards.
///
/// Returns the ANCHOR's page index together with the sampled overlay pixel (also not bounds
/// checked, exactly like `stamp_source_xy`). Returns `None` outside `StampMode::CurrentImage`,
/// where a different image is sampled and a marker on this page would be meaningless, and `None`
/// when no stamp anchor is set.
fn stamp_sampled_overlay_xy(mode: StampMode, anchor: Option<StampAnchor>, rotation_degrees: f32, stroke_origin_overlay: Option<[i32; 2]>, dst_overlay: [i32; 2]) -> Option<(usize, [i32; 2])> {
    if mode != StampMode::CurrentImage {
        return None;
    }
    let anchor = anchor?;
    // The position is derived from the sampling mapping itself instead of a second copy of the
    // offset/rotation maths: an indicator that can disagree with the pixels actually read would
    // be worse than no indicator at all.
    let context = StampSourceContext {
        source: None,
        overlay: None,
        mode,
        // Irrelevant to the coordinate mapping; no layer is sampled here.
        current_source: CurrentImageStampSource::OriginalOnly,
        source_anchor: Some(anchor),
        stroke_origin_overlay: Some(stroke_origin_overlay.unwrap_or(dst_overlay)),
        rotation_degrees,
        y_offset: 0,
    };
    let (src_x, src_y) = stamp_source_xy(&context, dst_overlay[0], dst_overlay[1])?;
    Some((anchor.page_idx, [src_x, src_y]))
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

struct StampSourceContext<'a> {
    source: Option<&'a Arc<RgbaImage>>,
    overlay: Option<&'a egui::ColorImage>,
    mode: StampMode,
    current_source: CurrentImageStampSource,
    source_anchor: Option<StampAnchor>,
    stroke_origin_overlay: Option<[i32; 2]>,
    rotation_degrees: f32,
    y_offset: i32,
}

struct StampPaintTarget<'a> {
    dst: &'a mut egui::ColorImage,
    base: &'a egui::ColorImage,
    mask: &'a mut [f32],
    overlay_rect: OverlayRectPx,
}

// All parameters are independent brush stroke properties; grouping would obscure painting intent.
#[allow(clippy::too_many_arguments)]
fn stamp_segment_on_image(
    dst: &mut egui::ColorImage,
    base: &egui::ColorImage,
    mask: &mut [f32],
    source: &StampSourceContext<'_>,
    dst_overlay_rect: OverlayRectPx,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    hardness: f32,
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
        let target = StampPaintTarget {
            dst,
            base,
            mask,
            overlay_rect: dst_overlay_rect,
        };
        stamp_circle_to_image(target, source, [cx, cy], radius, hardness);
    }
}

fn stamp_circle_to_image(
    target: StampPaintTarget<'_>,
    source: &StampSourceContext<'_>,
    center: [i32; 2],
    radius: i32,
    hardness: f32,
) {
    if radius <= 0 {
        return;
    }
    if target.dst.size != target.base.size
        || target.dst.pixels.len() != target.base.pixels.len()
        || target.dst.pixels.len() != target.mask.len()
    {
        return;
    }

    let center_x = center[0];
    let center_y = center[1];
    let left = center_x - radius;
    let top = center_y - radius;
    let right = center_x + radius - 1;
    let bottom = center_y + radius - 1;
    let Some(bounds) = intersect_overlay_bounds(target.overlay_rect, left, top, right, bottom)
    else {
        return;
    };

    let r2 = radius * radius;
    let radius_f = radius.max(1) as f32;
    let hard_radius = (radius_f * hardness.clamp(0.0, 1.0)).clamp(0.0, radius_f);
    let soft_span = (radius_f - hard_radius).max(f32::EPSILON);
    for oy in bounds.y..(bounds.y + bounds.h) {
        let local_y = oy as i32 - top;
        let dy = local_y - radius;
        let dst_local_y = oy.saturating_sub(target.overlay_rect.y);
        let dst_row = dst_local_y.saturating_mul(target.dst.size[0]);
        for ox in bounds.x..(bounds.x + bounds.w) {
            let local_x = ox as i32 - left;
            let dx = local_x - radius;
            let dist2 = dx * dx + dy * dy;
            if dist2 > r2 {
                continue;
            }
            let dist = (dist2 as f32).sqrt();
            let strength = if dist <= hard_radius {
                1.0
            } else {
                (1.0 - ((dist - hard_radius) / soft_span)).clamp(0.0, 1.0)
            };
            if strength <= f32::EPSILON {
                continue;
            }
            let dst_local_x = ox.saturating_sub(target.overlay_rect.x);
            let dst_idx = dst_row.saturating_add(dst_local_x);
            let Some(mask_px) = target.mask.get_mut(dst_idx) else {
                continue;
            };
            if strength <= *mask_px {
                continue;
            }
            // Destination overlay coordinates of this dab pixel; `sample_stamp_source` and
            // `stamp_source_xy` map them to the source point themselves.
            let dst_x = center_x + dx;
            let dst_y = center_y + dy;
            let Some(mut src_px) = sample_stamp_source(source, dst_x, dst_y) else {
                continue;
            };
            if src_px.a() == 0 {
                continue;
            }
            if let (Some(base_px), Some(dst_px)) = (
                target.base.pixels.get(dst_idx).copied(),
                target.dst.pixels.get_mut(dst_idx),
            ) {
                *mask_px = strength;
                if source.mode == StampMode::CurrentImage {
                    let Some(original) = source.source else {
                        continue;
                    };
                    let Some(target_original) = sample_rgba_image(original, ox as i32, oy as i32)
                    else {
                        continue;
                    };
                    let current_final = blend_source_over(target_original, base_px);
                    if source.current_source == CurrentImageStampSource::OverlayOnly {
                        // `src_px` is the clean-overlay pixel at the SOURCE point, so the colour
                        // actually visible there is that pixel over the original page at the
                        // SOURCE coordinates — not over `target_original`, which belongs to the
                        // destination. `OriginalAndOverlay` already does exactly this inside
                        // `sample_stamp_source`; an out-of-bounds source keeps the overlay pixel
                        // alone, mirroring the `TRANSPARENT` base used there.
                        let source_base = stamp_source_xy(source, dst_x, dst_y)
                            .and_then(|(sx, sy)| sample_rgba_image(original, sx, sy))
                            .unwrap_or(Color32::TRANSPARENT);
                        src_px = blend_source_over(source_base, src_px);
                    }
                    let desired_final = lerp_color_premultiplied(current_final, src_px, strength);
                    // The dab strength is the alpha FLOOR: see `overlay_pixel_for_final_color`.
                    *dst_px = overlay_pixel_for_final_color(target_original, desired_final, strength);
                } else {
                    let [r, g, b, a] = src_px.to_srgba_unmultiplied();
                    src_px = Color32::from_rgba_unmultiplied(
                        r,
                        g,
                        b,
                        ((a as f32) * strength).round().clamp(0.0, 255.0) as u8,
                    );
                    *dst_px = blend_source_over(base_px, src_px);
                }
            }
        }
    }
}

// All parameters mirror stamp_segment_on_image but this path only changes alpha.
#[allow(clippy::too_many_arguments)]
fn erase_stamp_segment_on_image(
    dst: &mut egui::ColorImage,
    base: &egui::ColorImage,
    mask: &mut [f32],
    dst_overlay_rect: OverlayRectPx,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    hardness: f32,
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
        let target = StampPaintTarget {
            dst,
            base,
            mask,
            overlay_rect: dst_overlay_rect,
        };
        erase_stamp_circle_to_image(target, [cx, cy], radius, hardness);
    }
}

fn erase_stamp_circle_to_image(
    target: StampPaintTarget<'_>,
    center: [i32; 2],
    radius: i32,
    hardness: f32,
) {
    if target.dst.size != target.base.size
        || target.dst.pixels.len() != target.base.pixels.len()
        || target.dst.pixels.len() != target.mask.len()
    {
        return;
    }
    let center_x = center[0];
    let center_y = center[1];
    let left = center_x - radius;
    let top = center_y - radius;
    let right = center_x + radius - 1;
    let bottom = center_y + radius - 1;
    let Some(bounds) = intersect_overlay_bounds(target.overlay_rect, left, top, right, bottom)
    else {
        return;
    };
    let r2 = radius * radius;
    let radius_f = radius.max(1) as f32;
    let hard_radius = (radius_f * hardness.clamp(0.0, 1.0)).clamp(0.0, radius_f);
    let soft_span = (radius_f - hard_radius).max(f32::EPSILON);
    for oy in bounds.y..(bounds.y + bounds.h) {
        let local_y = oy as i32 - top;
        let dy = local_y - radius;
        let dst_local_y = oy.saturating_sub(target.overlay_rect.y);
        let dst_row = dst_local_y.saturating_mul(target.dst.size[0]);
        for ox in bounds.x..(bounds.x + bounds.w) {
            let local_x = ox as i32 - left;
            let dx = local_x - radius;
            let dist2 = dx * dx + dy * dy;
            if dist2 > r2 {
                continue;
            }
            let dist = (dist2 as f32).sqrt();
            let strength = if dist <= hard_radius {
                1.0
            } else {
                (1.0 - ((dist - hard_radius) / soft_span)).clamp(0.0, 1.0)
            };
            let dst_local_x = ox.saturating_sub(target.overlay_rect.x);
            let dst_idx = dst_row.saturating_add(dst_local_x);
            let Some(mask_px) = target.mask.get_mut(dst_idx) else {
                continue;
            };
            if strength <= *mask_px {
                continue;
            }
            if let (Some(base_px), Some(dst_px)) = (
                target.base.pixels.get(dst_idx).copied(),
                target.dst.pixels.get_mut(dst_idx),
            ) {
                let [r, g, b, a] = base_px.to_srgba_unmultiplied();
                let next_a = ((a as f32) * (1.0 - strength)).round().clamp(0.0, 255.0) as u8;
                *mask_px = strength;
                *dst_px = Color32::from_rgba_unmultiplied(r, g, b, next_a);
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

fn build_rect_chunk_from_current_source(
    source: &StampSourceContext<'_>,
    rect: OverlayRectPx,
) -> egui::ColorImage {
    let mut out = egui::ColorImage::filled([rect.w, rect.h], Color32::TRANSPARENT);
    for y in 0..rect.h {
        let dst_row = y.saturating_mul(rect.w);
        for x in 0..rect.w {
            let dst_idx = dst_row.saturating_add(x);
            let sx = rect.x.saturating_add(x) as i32;
            let sy = rect.y.saturating_add(y) as i32;
            if let (Some(src_px), Some(dst_px)) = (
                sample_stamp_source(source, sx, sy),
                out.pixels.get_mut(dst_idx),
            ) {
                *dst_px = src_px;
            }
        }
    }
    out
}

fn build_stamp_patch_with_opacity(
    source: &StampSourceContext<'_>,
    center_x: i32,
    center_y: i32,
    radius: i32,
    hardness: f32,
    opacity: u8,
) -> egui::ColorImage {
    let side = (radius.max(1) * 2) as usize;
    let mut out = egui::ColorImage::filled([side, side], Color32::TRANSPARENT);
    if opacity == 0 {
        return out;
    }
    let r2 = radius * radius;
    let radius_f = radius.max(1) as f32;
    let hard_radius = (radius_f * hardness.clamp(0.0, 1.0)).clamp(0.0, radius_f);
    let soft_span = (radius_f - hard_radius).max(f32::EPSILON);

    for y in 0..side {
        let dy = y as i32 - radius;
        let dst_row = y.saturating_mul(side);
        for x in 0..side {
            let dx = x as i32 - radius;
            let dist2 = dx * dx + dy * dy;
            if dist2 > r2 {
                continue;
            }
            let dist = (dist2 as f32).sqrt();
            let strength = if dist <= hard_radius {
                1.0
            } else {
                (1.0 - ((dist - hard_radius) / soft_span)).clamp(0.0, 1.0)
            };
            let Some(src_px) = sample_stamp_source(source, center_x + dx, center_y + dy) else {
                continue;
            };
            let [r, g, b, mut a] = src_px.to_srgba_unmultiplied();
            if a == 0 {
                continue;
            }
            a = ((a as f32) * (opacity as f32 / 255.0) * strength)
                .round()
                .clamp(0.0, 255.0) as u8;
            let dst_idx = dst_row.saturating_add(x);
            if let Some(dst_px) = out.pixels.get_mut(dst_idx) {
                *dst_px = Color32::from_rgba_unmultiplied(r, g, b, a);
            }
        }
    }
    out
}

/// Maps a DESTINATION overlay pixel to the overlay pixel the stamp reads its colour from.
///
/// `dst_x`/`dst_y` are page-overlay pixel coordinates of the pixel being painted. In
/// `AltVersion` mode the mapping is the vertical `y_offset` only. In `CurrentImage` mode the
/// offset from the stroke origin is rotated by `rotation_degrees` (clockwise in image space,
/// y pointing down) and added to the stamp anchor, i.e. the Photoshop clone-stamp mapping.
///
/// Returns `None` in `CurrentImage` mode when no stamp anchor or no stroke origin is set;
/// the returned coordinates are NOT bounds-checked — the samplers do that.
fn stamp_source_xy(source: &StampSourceContext<'_>, dst_x: i32, dst_y: i32) -> Option<(i32, i32)> {
    match source.mode {
        StampMode::AltVersion => Some((dst_x, dst_y.saturating_add(source.y_offset))),
        StampMode::CurrentImage => {
            let anchor = source.source_anchor?;
            let [origin_x, origin_y] = source.stroke_origin_overlay?;
            let dx = dst_x.saturating_sub(origin_x) as f32;
            let dy = dst_y.saturating_sub(origin_y) as f32;
            let radians = source.rotation_degrees.to_radians();
            let (sin, cos) = radians.sin_cos();
            let rx = dx * cos - dy * sin;
            let ry = dx * sin + dy * cos;
            Some((
                (anchor.overlay_xy[0] + rx).round() as i32,
                (anchor.overlay_xy[1] + ry).round() as i32,
            ))
        }
    }
}

/// Samples the colour the stamp copies for the destination overlay pixel `dst_x`/`dst_y`.
///
/// The source point is resolved by `stamp_source_xy`; which layer is read there depends on
/// `source.current_source`. Returns `None` when the source point falls outside the sampled
/// layer, when a required layer is absent, or when the clone-stamp mapping is not set up.
fn sample_stamp_source(source: &StampSourceContext<'_>, dst_x: i32, dst_y: i32) -> Option<Color32> {
    let (src_x, src_y) = stamp_source_xy(source, dst_x, dst_y)?;

    match source.mode {
        StampMode::AltVersion => sample_rgba_image(source.source?, src_x, src_y),
        StampMode::CurrentImage => match source.current_source {
            CurrentImageStampSource::OriginalOnly => {
                sample_rgba_image(source.source?, src_x, src_y)
            }
            CurrentImageStampSource::OverlayOnly => {
                sample_overlay_image(source.overlay?, src_x, src_y)
            }
            CurrentImageStampSource::OriginalAndOverlay => {
                let base =
                    sample_rgba_image(source.source?, src_x, src_y).unwrap_or(Color32::TRANSPARENT);
                if let Some(overlay) = source
                    .overlay
                    .and_then(|overlay| sample_overlay_image(overlay, src_x, src_y))
                {
                    Some(blend_source_over(base, overlay))
                } else {
                    Some(base)
                }
            }
        },
    }
}

fn sample_rgba_image(source: &RgbaImage, x: i32, y: i32) -> Option<Color32> {
    if x < 0 || y < 0 || x >= source.width() as i32 || y >= source.height() as i32 {
        return None;
    }
    let src_w = source.width() as usize;
    let idx = ((y as usize)
        .saturating_mul(src_w)
        .saturating_add(x as usize))
    .saturating_mul(4);
    let raw = source.as_raw();
    Some(Color32::from_rgba_unmultiplied(
        *raw.get(idx)?,
        *raw.get(idx + 1)?,
        *raw.get(idx + 2)?,
        *raw.get(idx + 3)?,
    ))
}

fn sample_overlay_image(source: &egui::ColorImage, x: i32, y: i32) -> Option<Color32> {
    if x < 0 || y < 0 || x >= source.size[0] as i32 || y >= source.size[1] as i32 {
        return None;
    }
    source
        .pixels
        .get(
            (y as usize)
                .saturating_mul(source.size[0])
                .saturating_add(x as usize),
        )
        .copied()
}

/// Composites `src` over `dst` (Porter-Duff source-over) in sRGB space.
///
/// ALPHA CONVENTION: `Color32` is sRGBA u8 with PREMULTIPLIED alpha
/// (`ecolor-0.35.0/src/color32.rs:8`), so `r()`/`g()`/`b()` already return colour
/// bytes scaled by alpha. This function therefore works exclusively on the
/// UN-premultiplied representation (`to_srgba_unmultiplied()`), premultiplies once
/// itself, blends, and un-premultiplies again for `from_rgba_unmultiplied` — the same
/// discipline as `lerp_color_premultiplied`. Reading `r()`/`g()`/`b()` and then
/// multiplying by alpha again multiplies twice and darkens every translucent input.
///
/// Returns the composited colour; `src` with zero alpha is a no-op and a fully
/// transparent result is normalized to `Color32::TRANSPARENT`.
fn blend_source_over(dst: Color32, src: Color32) -> Color32 {
    let [sr, sg, sb, sa] = src.to_srgba_unmultiplied();
    let sa = sa as f32 / 255.0;
    if sa <= f32::EPSILON {
        return dst;
    }
    let [dr, dg, db, da] = dst.to_srgba_unmultiplied();
    let da = da as f32 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= f32::EPSILON {
        return Color32::TRANSPARENT;
    }
    // Straight -> premultiplied -> blend -> straight again, once and explicitly.
    let blend = |s: u8, d: u8| {
        ((s as f32 / 255.0) * sa + (d as f32 / 255.0) * da * (1.0 - sa)) / out_a
    };
    let out_r = blend(sr, dr);
    let out_g = blend(sg, dg);
    let out_b = blend(sb, db);
    Color32::from_rgba_unmultiplied(
        (out_r * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_g * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_b * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_a * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

fn lerp_color_premultiplied(from: Color32, to: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    if t <= f32::EPSILON {
        return from;
    }
    if (1.0 - t) <= f32::EPSILON {
        return to;
    }
    let [fr, fg, fb, fa] = from.to_srgba_unmultiplied();
    let [tr, tg, tb, ta] = to.to_srgba_unmultiplied();
    let fa = fa as f32 / 255.0;
    let ta = ta as f32 / 255.0;
    let out_a = fa + (ta - fa) * t;
    if out_a <= f32::EPSILON {
        return Color32::TRANSPARENT;
    }
    let fr = fr as f32 / 255.0 * fa;
    let fg = fg as f32 / 255.0 * fa;
    let fb = fb as f32 / 255.0 * fa;
    let tr = tr as f32 / 255.0 * ta;
    let tg = tg as f32 / 255.0 * ta;
    let tb = tb as f32 / 255.0 * ta;
    let out_r = (fr + (tr - fr) * t) / out_a;
    let out_g = (fg + (tg - fg) * t) / out_a;
    let out_b = (fb + (tb - fb) * t) / out_a;
    Color32::from_rgba_unmultiplied(
        (out_r * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_g * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_b * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_a * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

/// Solves for the clean-overlay pixel that shows `final_color` when composited over
/// the original page pixel `base`, at an alpha of at least `coverage`.
///
/// `coverage` is the brush dab strength in `0..=1` (values outside are clamped); it is
/// the LOWER bound on the returned alpha, not the alpha itself.
///
/// # Why not the minimum alpha
/// For a target colour `f` over an opaque original `o`, every overlay alpha `a` in
/// `[alpha_min, 1]` reproduces `f` exactly, with `p(a) = o + (f - o) / a`: `p` moves
/// monotonically from the clamp boundary at `alpha_min` to `p(1) = f`, so it stays inside
/// `[0, 1]` for the whole range. `alpha_min` — the smallest representable alpha, still
/// computed below — is exact only under pixel-perfect 1:1 compositing. On the canvas the
/// page and the clean overlay are two SEPARATE textured quads, both sampled with
/// `TextureOptions::LINEAR` (`src/canvas/overlay_runtime.rs`), so colour and alpha are
/// filtered INDEPENDENTLY and their product term is lost: a minimum-alpha solution over
/// black text on white paper is a text-shaped opaque stencil on a transparent field, and
/// at glyph coverage `w` the screen shows `w + (1 - w)^2` instead of `1.0` — a ghost of
/// the letters peaking at 0.25 error. Lifting alpha to the dab coverage keeps the composite
/// mathematically exact and makes the committed overlay DENSE over the brushed area (like
/// `zamazka`'s) instead of sparse; at hardness 100 % `coverage == 1.0`, so the patch is
/// fully opaque and immune to independent resampling.
///
/// Returns `Color32::TRANSPARENT` when `final_color` is fully transparent, and when the
/// solved alpha is below one 8-bit step — which, since the alpha is at least `coverage`,
/// can only happen where the dab itself contributes nothing.
fn overlay_pixel_for_final_color(base: Color32, final_color: Color32, coverage: f32) -> Color32 {
    let [br, bg, bb, _] = base.to_srgba_unmultiplied();
    let [fr, fg, fb, fa] = final_color.to_srgba_unmultiplied();
    if fa == 0 {
        return Color32::TRANSPARENT;
    }
    let b = [br as f32 / 255.0, bg as f32 / 255.0, bb as f32 / 255.0];
    let f = [fr as f32 / 255.0, fg as f32 / 255.0, fb as f32 / 255.0];
    let mut alpha: f32 = 0.0;
    for channel in 0..3 {
        let diff = (f[channel] - b[channel]).abs();
        if diff <= (1.0 / 255.0) {
            continue;
        }
        let needed = if f[channel] < b[channel] {
            diff / b[channel].max(f32::EPSILON)
        } else {
            diff / (1.0 - b[channel]).max(f32::EPSILON)
        };
        alpha = alpha.max(needed.clamp(0.0, 1.0));
    }
    // `alpha` is the representability floor; the dab coverage raises it, never lowers it.
    alpha = alpha.max(coverage.clamp(0.0, 1.0));
    if alpha <= (1.0 / 255.0) {
        return Color32::TRANSPARENT;
    }
    let out = [
        ((f[0] - b[0] * (1.0 - alpha)) / alpha).clamp(0.0, 1.0),
        ((f[1] - b[1] * (1.0 - alpha)) / alpha).clamp(0.0, 1.0),
        ((f[2] - b[2] * (1.0 - alpha)) / alpha).clamp(0.0, 1.0),
    ];
    Color32::from_rgba_unmultiplied(
        (out[0] * 255.0).round().clamp(0.0, 255.0) as u8,
        (out[1] * 255.0).round().clamp(0.0, 255.0) as u8,
        (out[2] * 255.0).round().clamp(0.0, 255.0) as u8,
        (alpha * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolerance of one 8-bit step, expressed in the normalized `0..=1` range.
    const ONE_STEP: f32 = 1.0 / 255.0;

    /// Composites an un-premultiplied overlay pixel over an opaque base and returns the
    /// three normalized sRGB channels, i.e. what the renderer shows at 1:1 sampling.
    fn composite_over(overlay: Color32, base: Color32) -> [f32; 3] {
        let [orr, og, ob, oa] = overlay.to_srgba_unmultiplied();
        let [br, bg, bb, _] = base.to_srgba_unmultiplied();
        let a = f32::from(oa) / 255.0;
        [
            (f32::from(orr) / 255.0) * a + (f32::from(br) / 255.0) * (1.0 - a),
            (f32::from(og) / 255.0) * a + (f32::from(bg) / 255.0) * (1.0 - a),
            (f32::from(ob) / 255.0) * a + (f32::from(bb) / 255.0) * (1.0 - a),
        ]
    }

    /// Normalized sRGB channels of a colour, ignoring its alpha.
    fn channels(color: Color32) -> [f32; 3] {
        let [r, g, b, _] = color.to_srgba_unmultiplied();
        [
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
        ]
    }

    /// A context with no image layers — enough to exercise the coordinate mapping alone.
    fn coordinate_context(mode: StampMode, rotation_degrees: f32, y_offset: i32) -> StampSourceContext<'static> {
        StampSourceContext {
            source: None,
            overlay: None,
            mode,
            current_source: CurrentImageStampSource::OriginalOnly,
            source_anchor: Some(StampAnchor { page_idx: 0, overlay_xy: [100.0, 100.0] }),
            stroke_origin_overlay: Some([10, 10]),
            rotation_degrees,
            y_offset,
        }
    }

    #[test]
    fn overlay_pixel_at_full_coverage_is_opaque_and_equals_final_color() {
        // The ghosting regression: over a white page the minimum-alpha solution was
        // fully transparent, so the page showed through the resampled overlay.
        let over_white = overlay_pixel_for_final_color(Color32::WHITE, Color32::WHITE, 1.0);
        assert_eq!(over_white, Color32::from_rgba_unmultiplied(255, 255, 255, 255));

        let over_black = overlay_pixel_for_final_color(Color32::BLACK, Color32::WHITE, 1.0);
        assert_eq!(over_black, Color32::from_rgba_unmultiplied(255, 255, 255, 255));

        // A non-neutral target must survive full coverage unchanged as well.
        let target = Color32::from_rgb(37, 180, 90);
        let solved = overlay_pixel_for_final_color(Color32::from_rgb(200, 40, 10), target, 1.0);
        assert_eq!(solved.a(), 255);
        assert_eq!(solved.to_srgba_unmultiplied(), target.to_srgba_unmultiplied());
    }

    #[test]
    fn overlay_pixel_at_partial_coverage_still_reproduces_final_color() {
        let cases = [
            (Color32::WHITE, Color32::from_rgb(128, 128, 128), 0.5_f32),
            (Color32::WHITE, Color32::from_rgb(200, 190, 210), 0.25_f32),
            (Color32::BLACK, Color32::from_rgb(64, 32, 96), 0.35_f32),
            // Coverage below the representability floor must not weaken the solution.
            (Color32::WHITE, Color32::BLACK, 0.2_f32),
        ];
        for (base, desired_final, coverage) in cases {
            let overlay = overlay_pixel_for_final_color(base, desired_final, coverage);
            let alpha = f32::from(overlay.a()) / 255.0;
            assert!(alpha + ONE_STEP >= coverage, "alpha {alpha} fell below coverage {coverage}");
            let shown = composite_over(overlay, base);
            let want = channels(desired_final);
            for channel in 0..3 {
                // One step for the alpha quantization plus one for the colour quantization.
                assert!((shown[channel] - want[channel]).abs() <= 2.0 * ONE_STEP, "channel {channel}: {shown:?} vs {want:?} (coverage {coverage})");
            }
        }
    }

    #[test]
    fn overlay_pixel_for_transparent_final_color_is_transparent() {
        assert_eq!(overlay_pixel_for_final_color(Color32::WHITE, Color32::TRANSPARENT, 1.0), Color32::TRANSPARENT);
    }

    #[test]
    fn overlay_pixel_without_coverage_or_difference_is_transparent() {
        // Nothing to paint and nothing to represent: the pixel stays untouched.
        assert_eq!(overlay_pixel_for_final_color(Color32::WHITE, Color32::WHITE, 0.0), Color32::TRANSPARENT);
    }

    #[test]
    fn blend_source_over_opaque_source_replaces_destination() {
        let src = Color32::from_rgb(12, 200, 77);
        assert_eq!(blend_source_over(Color32::from_rgb(240, 10, 10), src), Color32::from_rgba_unmultiplied(12, 200, 77, 255));
    }

    #[test]
    fn blend_source_over_transparent_source_is_a_no_op() {
        let dst = Color32::from_rgba_unmultiplied(30, 60, 90, 200);
        assert_eq!(blend_source_over(dst, Color32::TRANSPARENT), dst);
    }

    #[test]
    fn blend_source_over_half_alpha_white_over_black_is_mid_grey() {
        // Regression test for the premultiplied/straight double-multiply: with `Color32`
        // accessors returning PREMULTIPLIED bytes, the old code produced 0.502^2 = 64
        // instead of 0.502 -> 128.
        let src = Color32::from_rgba_unmultiplied(255, 255, 255, 128);
        let out = blend_source_over(Color32::BLACK, src);
        assert_eq!(out.a(), 255);
        let [r, g, b, _] = out.to_srgba_unmultiplied();
        for channel in [r, g, b] {
            let got = f32::from(channel) / 255.0;
            assert!((got - 128.0 / 255.0).abs() <= ONE_STEP, "expected mid grey, got {channel}");
        }
    }

    #[test]
    fn stamp_source_xy_without_rotation_offsets_by_the_anchor() {
        let context = coordinate_context(StampMode::CurrentImage, 0.0, 0);
        // Destination 10px right of the stroke origin -> 10px right of the anchor.
        assert_eq!(stamp_source_xy(&context, 20, 10), Some((110, 100)));
        assert_eq!(stamp_source_xy(&context, 10, 15), Some((100, 105)));
    }

    #[test]
    fn stamp_source_xy_rotates_the_offset_around_the_anchor() {
        let context = coordinate_context(StampMode::CurrentImage, 90.0, 0);
        // +x at the destination becomes +y at the source (y grows downwards).
        assert_eq!(stamp_source_xy(&context, 20, 10), Some((100, 110)));
        assert_eq!(stamp_source_xy(&context, 10, 20), Some((90, 100)));
    }

    #[test]
    fn stamp_source_xy_in_alt_version_mode_applies_only_the_y_offset() {
        let context = coordinate_context(StampMode::AltVersion, 90.0, -7);
        assert_eq!(stamp_source_xy(&context, 20, 10), Some((20, 3)));
    }

    #[test]
    fn stamp_source_xy_without_an_anchor_has_no_source_point() {
        let mut context = coordinate_context(StampMode::CurrentImage, 0.0, 0);
        context.source_anchor = None;
        assert_eq!(stamp_source_xy(&context, 20, 10), None);
    }

    /// Anchor used by the sampled-marker tests. Its overlay position matches the one
    /// `coordinate_context` builds, so the two can be compared directly; only the page differs,
    /// and the page index does not take part in the coordinate mapping.
    fn marker_anchor() -> StampAnchor {
        StampAnchor { page_idx: 3, overlay_xy: [100.0, 100.0] }
    }

    #[test]
    fn sampled_marker_without_a_stroke_sits_on_the_anchor() {
        let anchor = marker_anchor();
        // No active stroke -> zero offset -> the marker rests exactly on the beacon.
        assert_eq!(stamp_sampled_overlay_xy(StampMode::CurrentImage, Some(anchor), 0.0, None, [512, 40]), Some((3, [100, 100])));
        // A rotation cannot move a zero offset, so it stays there whatever the angle.
        assert_eq!(stamp_sampled_overlay_xy(StampMode::CurrentImage, Some(anchor), 37.0, None, [512, 40]), Some((3, [100, 100])));
        // A fractional anchor (free placement, snapping off) rounds to the nearest pixel.
        let fractional = StampAnchor { page_idx: 3, overlay_xy: [100.4, 100.6] };
        assert_eq!(stamp_sampled_overlay_xy(StampMode::CurrentImage, Some(fractional), 0.0, None, [512, 40]), Some((3, [100, 101])));
    }

    #[test]
    fn sampled_marker_follows_the_stroke_offset() {
        let anchor = marker_anchor();
        // Destination 20px right and 15px below the stroke origin -> the same offset from the anchor.
        assert_eq!(stamp_sampled_overlay_xy(StampMode::CurrentImage, Some(anchor), 0.0, Some([10, 10]), [30, 25]), Some((3, [120, 115])));
    }

    #[test]
    fn sampled_marker_matches_the_sampling_mapping_under_rotation() {
        // The real contract: the indicator must never disagree with the pixels actually read.
        let context = coordinate_context(StampMode::CurrentImage, 90.0, 0);
        for dst in [[20, 10], [10, 20], [-5, 40], [300, 275]] {
            let sampled = stamp_source_xy(&context, dst[0], dst[1]);
            let marker = stamp_sampled_overlay_xy(StampMode::CurrentImage, Some(marker_anchor()), 90.0, Some([10, 10]), dst);
            assert_eq!(marker.map(|(_, xy)| (xy[0], xy[1])), sampled, "marker disagreed with the sampling mapping at {dst:?}");
        }
    }

    #[test]
    fn sampled_marker_needs_an_anchor_and_the_current_image_mode() {
        assert_eq!(stamp_sampled_overlay_xy(StampMode::CurrentImage, None, 0.0, None, [10, 10]), None);
        // Alt-version mode samples a different image; a marker on this page would be a lie.
        assert_eq!(stamp_sampled_overlay_xy(StampMode::AltVersion, Some(marker_anchor()), 0.0, Some([10, 10]), [30, 25]), None);
    }
}
