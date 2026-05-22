/*
FILE HEADER (cleaning/tools/base.rs)
- Назначение: базовые кирпичики инструментов клининга (общий трейt инструмента, база кисти и база region-edit).
- Ключевые типы:
  - `CleaningTool`: контракт инструмента (UI, stroke-события, хоткеи, overlay-окна, курсор).
  - `BrushToolBase`: переиспользуемая логика кисти (радиус, hardness, subpixel coverage/strict
    pixel режим, хоткеи размера, scratch-штрих, tiled preview с обновлением только dirty-тайлов,
    без full-size scratch texture).
  - `RegionEditToolBase`: каркас выделения региона, фоновой загрузки композита `base+overlay`,
    показа отдельного окна редактора, zoom-жестов (как в CanvasView) и вставки результата обратно в overlay.
  - `RegionMaskInpaintToolBase`: база для mask-inpaint инструментов поверх region-edit:
    маска удаления (жёлтый полупрозрачный overlay), кисть `MaskBrush` из `src/tools/mask_brush.rs`,
    кнопки `Сгенерировать маску/Обработать/Переделать/Вернуть/Отмена/Применить`, run-пайплайн для наследника и optional
    custom-UI callback внутри окна редактора (для инструмент-специфичных параметров). Для отдельных
    инструментов есть расширенный режим второй маски (например, маска области примера).
    Вызов `run(image, mask[, sample_mask])` выполняется в фоне (worker-thread), результат поллится в UI.
    Отдельная кнопка генерации маски отправляет текущий region image в выбранный backend-детектор текста
    (ComicTextDetector, PaddleOCR или Surya) и заполняет mask-слой полученной бинарной маской без применения inpaint; параметры генерации маски
    (например, расширение mask dilation) хранятся в базе инструмента и редактируются в общем collapsible UI.
  - `RegionEditorSession`: состояние открытого окна region editor (target rect в overlay px,
    изображение, texture, статус, zoom-drag).
- Потоки:
  - Отдельный loader-thread (`spawn_region_loader_thread`) декодирует region image, чтобы не блокировать GUI;
    поток сначала проверяет общий page-кеш в `CleanOverlaysModel`, затем (при miss) декодирует страницу
    и сохраняет её в модель; дополнительно держит локальный кеш последней декодированной страницы.
- Ключевые функции:
  - `draw_overlay_ui` у `RegionEditToolBase` рендерит окно редактора, показывает промежуточное окно загрузки
    сразу после выделения и применяет патч; после перехода `loading -> editor` окно
    один раз доцентрируется под новый размер (без повторного центрирования на zoom).
  - `draw_overlay_ui_custom` у `RegionEditToolBase` даёт инструментам полный контроль над содержимым
    окна редактора (включая собственный footer-контроллер кнопок).
  - `draw_region_editor_zoom_controls` и `draw_region_editor_scroll_area` дают общий zoom/scroll UI
    для изображения в окне region editor.
  - `draw_region_editor_collapsible_section` даёт единый helper для инструмент-специфичных
    сворачиваемых секций интерфейса внутри окна region editor.
  - `draw_overlay_ui_custom_with_sample_mask` у `RegionMaskInpaintToolBase` включает второй слой маски
    и передаёт его в run-closure (для инструментов, которым нужна отдельная область примера).
  - `draw_region_editor_image_with_stroke_input` даёт общий интерактивный viewport изображения
    с конвертацией pointer->image px и drag-сегментами штриха.
  - `handle_region_editor_zoom_input` обрабатывает Ctrl/Z zoom-shortcuts:
    `-`/`=`, `0`, колесо и scrub-zoom (Ctrl/Z + ЛКМ drag).
  - `has_open_editor` используется вкладкой cleaning, чтобы блокировать zoom CanvasView,
    пока открыто окно region editor.
  - `build_composited_region_image` режет базовую страницу и композитит overlay.
*/
use crate::canvas::{CanvasView, OverlayRectPx};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::project::ProjectData;
use crate::tabs::translation::text_detector::{
    TextDetectorAiCtdOptions, TextDetectorPaddleOcrOptions, detect_ai_ctd_mask_for_image,
    detect_paddle_mask_for_image, detect_surya_mask_for_image,
};
use crate::tools::MaskBrush;
use crate::widgets::WheelSlider;
use eframe::egui;
use egui::{Color32, Pos2, Rect, TextureHandle, TextureOptions};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

const CLEANING_PREVIEW_TEXTURE_OPTIONS: TextureOptions = TextureOptions::LINEAR;
const SCRATCH_PREVIEW_TILE_SIDE: usize = 1024;
const REGION_EDITOR_TEXTURE_OPTIONS: TextureOptions = TextureOptions::LINEAR;
const REGION_EDITOR_MIN_ZOOM: f32 = 0.1;
const REGION_EDITOR_MAX_ZOOM: f32 = 8.0;
const REGION_EDITOR_ZOOM_STEP: f32 = 1.1;
const REGION_EDITOR_WINDOW_UI_PAD_X: f32 = 24.0;
const REGION_EDITOR_WINDOW_UI_PAD_Y: f32 = 84.0;
const REGION_EDITOR_WINDOW_MIN_W: f32 = 180.0;
const REGION_EDITOR_WINDOW_MIN_H: f32 = 120.0;
const REGION_EDITOR_WINDOW_MAX_W: f32 = 1800.0;
const REGION_EDITOR_WINDOW_MAX_H: f32 = 1400.0;
const REGION_EDITOR_WINDOW_VIEWPORT_CAP_FRAC: f32 = 0.90;
const REGION_EDITOR_LOADING_WINDOW_W: f32 = 340.0;
const REGION_EDITOR_LOADING_WINDOW_H: f32 = 120.0;

#[derive(Debug, Clone, Copy)]
struct BrushFalloff {
    radius: f32,
    hard_radius: f32,
    soft_span: f32,
    hardness: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct StrokePoint {
    pub page_idx: usize,
    pub scene_pos: egui::Pos2,
    pub modifiers: StrokeModifiers,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StrokeModifiers {
    pub shift: bool,
    pub ctrl: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct CleaningCursorOccluder {
    pub page_idx: usize,
    pub center_scene_pos: Pos2,
    pub radius_scene: f32,
}

pub trait CleaningTool {
    #[allow(dead_code)]
    fn tool_id(&self) -> &'static str;
    fn title(&self) -> &'static str;

    fn pytorch_required(&self) -> bool {
        false
    }

    fn activate(&mut self, _canvas: &mut CanvasView) {}

    fn deactivate(&mut self, _canvas: &mut CanvasView) {}

    fn draw_ui(&mut self, _ui: &mut egui::Ui) {}

    fn secondary_click(
        &mut self,
        _canvas: &mut CanvasView,
        _project: &ProjectData,
        _point: StrokePoint,
    ) -> bool {
        false
    }

    fn stroke_begin(&mut self, _canvas: &mut CanvasView, _point: StrokePoint) {}

    fn stroke_update(&mut self, _canvas: &mut CanvasView, _from: StrokePoint, _to: StrokePoint) {}

    fn stroke_end(&mut self, _canvas: &mut CanvasView) {}

    fn set_temporary_erase(&mut self, _erase: bool) {}

    fn block_canvas_drag_scroll_on_primary(&self) -> bool {
        false
    }

    fn block_canvas_drag_scroll_on_secondary(&self) -> bool {
        false
    }

    fn block_canvas_zoom(&self) -> bool {
        false
    }

    fn block_canvas_zoom_on_ctrl_primary(&self) -> bool {
        false
    }

    fn suppress_base_overlay_render(&self) -> bool {
        false
    }

    fn on_wheel_event(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        let _ = (delta_y, modifiers);
        false
    }

    fn on_key_event(&mut self, _ctx: &egui::Context) -> bool {
        false
    }

    fn wants_primary_stroke(&self, _point: StrokePoint) -> bool {
        true
    }

    fn set_space_pan_active(&mut self, _active: bool) {}

    fn set_ai_backend_available(&mut self, _available: bool) {}

    fn set_ai_backend_torch_available(&mut self, _available: bool) {}

    fn space_pan_active(&self) -> bool {
        false
    }

    fn draw_overlay_ui(
        &mut self,
        _ctx: &egui::Context,
        _canvas: &mut CanvasView,
        _project: &ProjectData,
    ) {
    }

    fn ensure_hover_overlay(&mut self, _canvas: &mut CanvasView, _point: StrokePoint) {}

    fn draw_cursor(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<Pos2>,
    ) {
        let _ = (ui, canvas, pointer_scene_pos);
    }

    fn captures_canvas_pointer(&self, _pointer_pos: Pos2) -> bool {
        false
    }

    fn bubble_occluder(
        &self,
        _canvas: &CanvasView,
        _pointer_scene_pos: Option<Pos2>,
    ) -> Option<CleaningCursorOccluder> {
        None
    }
}

struct ScratchTextureTile {
    texture: TextureHandle,
    texture_options: TextureOptions,
    origin_px: [usize; 2],
    size_px: [usize; 2],
}

struct ScratchStroke {
    page_idx: usize,
    overlay_rect: OverlayRectPx,
    scene_rect: Rect,
    base_image: egui::ColorImage,
    image: egui::ColorImage,
    mask: Vec<f32>,
    tiles: Vec<ScratchTextureTile>,
    dirty_tiles: HashSet<usize>,
    dirty_rect: Option<OverlayRectPx>,
}

pub struct BrushToolBase {
    radius_px: usize,
    hardness: f32,
    blend_colors: bool,
    strict_pixel_painting: bool,
    min_radius_px: usize,
    max_radius_px: usize,
    wheel_step_px: usize,
    wheel_accum: f32,
    space_pan_active: bool,
    scratch: Option<ScratchStroke>,
}

impl Default for BrushToolBase {
    fn default() -> Self {
        Self {
            radius_px: 24,
            hardness: 0.99,
            blend_colors: true,
            strict_pixel_painting: false,
            min_radius_px: 1,
            max_radius_px: 200,
            wheel_step_px: 2,
            wheel_accum: 0.0,
            space_pan_active: false,
            scratch: None,
        }
    }
}

impl BrushToolBase {
    pub fn radius_px(&self) -> usize {
        self.radius_px
    }

    pub fn set_radius_px(&mut self, radius_px: usize) -> bool {
        let next = radius_px.clamp(self.min_radius_px, self.max_radius_px);
        if next == self.radius_px {
            return false;
        }
        self.radius_px = next;
        true
    }

    pub fn hardness(&self) -> f32 {
        self.hardness
    }

    pub fn set_hardness(&mut self, hardness: f32) -> bool {
        let next = hardness.clamp(0.0, 1.0);
        if (next - self.hardness).abs() <= f32::EPSILON {
            return false;
        }
        self.hardness = next;
        true
    }

    pub fn blend_colors(&self) -> bool {
        self.blend_colors
    }

    pub fn set_blend_colors(&mut self, blend_colors: bool) -> bool {
        if self.blend_colors == blend_colors {
            return false;
        }
        self.blend_colors = blend_colors;
        true
    }

    pub fn strict_pixel_painting(&self) -> bool {
        self.strict_pixel_painting
    }

    pub fn set_strict_pixel_painting(&mut self, strict_pixel_painting: bool) -> bool {
        if self.strict_pixel_painting == strict_pixel_painting {
            return false;
        }
        self.strict_pixel_painting = strict_pixel_painting;
        true
    }

    pub fn handle_wheel(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        if !modifiers.shift {
            self.wheel_accum = 0.0;
            return false;
        }
        if delta_y.abs() <= f32::EPSILON {
            return true;
        }
        const WHEEL_NOTCH: f32 = 40.0;
        self.wheel_accum += delta_y;

        let steps = (self.wheel_accum / WHEEL_NOTCH).trunc() as isize;
        if steps == 0 {
            return true;
        }
        self.wheel_accum -= steps as f32 * WHEEL_NOTCH;
        let next = self.radius_px as isize + steps * self.wheel_step_px as isize;
        self.set_radius_px(next.max(1) as usize);
        true
    }

    pub fn handle_size_shortcuts(&mut self, ctx: &egui::Context) -> bool {
        let (minus, equals, plus) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Minus),
                i.key_pressed(egui::Key::Equals),
                i.key_pressed(egui::Key::Plus),
            )
        });
        let mut changed = false;
        if minus {
            let mut next = ((self.radius_px as f32) * 0.9).floor() as usize;
            if next >= self.radius_px && self.radius_px > self.min_radius_px {
                next = self.radius_px.saturating_sub(1);
            }
            next = next.max(self.min_radius_px);
            changed |= self.set_radius_px(next);
        }
        if equals || plus {
            let mut next = ((self.radius_px as f32) * 1.1).ceil() as usize;
            if next <= self.radius_px && self.radius_px < self.max_radius_px {
                next = self.radius_px.saturating_add(1);
            }
            next = next.min(self.max_radius_px);
            changed |= self.set_radius_px(next);
        }
        changed
    }

    pub fn set_space_pan_active(&mut self, active: bool) {
        self.space_pan_active = active;
    }

    pub fn space_pan_active(&self) -> bool {
        self.space_pan_active
    }

    pub fn should_ignore_drawing(&self) -> bool {
        self.space_pan_active
    }

    pub fn draw_circle_cursor(
        &self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<Pos2>,
    ) {
        let Some(pointer_scene_pos) = pointer_scene_pos else {
            return;
        };
        let Some(page_idx) = canvas.page_index_at_scene_pos(pointer_scene_pos) else {
            return;
        };
        let Some(radius_scene) = self.scene_radius_for_page(canvas, page_idx) else {
            return;
        };

        ui.ctx()
            .output_mut(|out| out.cursor_icon = egui::CursorIcon::None);
        ui.painter().circle_stroke(
            pointer_scene_pos,
            radius_scene,
            egui::Stroke::new(2.0, Color32::WHITE),
        );
        ui.painter().circle_stroke(
            pointer_scene_pos,
            (radius_scene - 1.0).max(0.5),
            egui::Stroke::new(1.0, Color32::BLACK),
        );
    }

    pub fn scene_radius_for_page(&self, canvas: &CanvasView, page_idx: usize) -> Option<f32> {
        let page_rect = canvas.page_scene_rect(page_idx)?;
        let [overlay_w, overlay_h] = canvas.overlay_size(page_idx)?;
        if overlay_w == 0 || overlay_h == 0 {
            return None;
        }
        let radius_x_scene = self.radius_px as f32 * (page_rect.width() / overlay_w as f32);
        let radius_y_scene = self.radius_px as f32 * (page_rect.height() / overlay_h as f32);
        Some(((radius_x_scene + radius_y_scene) * 0.5).max(0.5))
    }

    pub fn bubble_occluder(
        &self,
        canvas: &CanvasView,
        pointer_scene_pos: Option<Pos2>,
    ) -> Option<CleaningCursorOccluder> {
        let center_scene_pos = pointer_scene_pos?;
        let page_idx = canvas.page_index_at_scene_pos(center_scene_pos)?;
        let radius_scene = self.scene_radius_for_page(canvas, page_idx)?;
        Some(CleaningCursorOccluder {
            page_idx,
            center_scene_pos,
            radius_scene,
        })
    }

    pub fn scratch_active(&self) -> bool {
        self.scratch.is_some()
    }

    pub fn ensure_overlay_under_point(canvas: &mut CanvasView, point: StrokePoint) -> bool {
        if canvas.overlay_image(point.page_idx).is_some() {
            return true;
        }
        let tiny = egui::ColorImage::filled([1, 1], Color32::TRANSPARENT);
        let tiny_scene_rect = Rect::from_center_size(point.scene_pos, egui::vec2(1.0, 1.0));
        canvas.replace_overlay_region_local(point.page_idx, tiny_scene_rect, &tiny)
    }

    pub fn begin_scratch_stroke(&mut self, canvas: &mut CanvasView, point: StrokePoint) -> bool {
        if self.scratch_active() {
            return true;
        }
        let _ = Self::ensure_overlay_under_point(canvas, point);
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
        let Some([overlay_w, overlay_h]) = canvas.overlay_size(point.page_idx) else {
            return false;
        };
        let expanded_rect =
            expand_overlay_rect(base_rect, overlay_w, overlay_h, self.radius_px.max(1) + 2);
        let Some(scene_rect) =
            overlay_rect_to_scene_rect(page_scene_rect, overlay_w, overlay_h, expanded_rect)
        else {
            return false;
        };
        let base_image = extract_overlay_chunk(overlay, expanded_rect);
        let pixel_count = base_image.pixels.len();
        self.scratch = Some(ScratchStroke {
            page_idx: point.page_idx,
            overlay_rect: expanded_rect,
            scene_rect,
            image: base_image.clone(),
            base_image,
            mask: vec![0.0; pixel_count],
            tiles: Vec::new(),
            dirty_tiles: HashSet::new(),
            dirty_rect: None,
        });
        true
    }

    pub fn paint_scratch_segment(
        &mut self,
        canvas: &mut CanvasView,
        from: StrokePoint,
        to: StrokePoint,
        color: Color32,
        erase: bool,
    ) -> bool {
        let strict_pixel_painting = self.strict_pixel_painting;
        let paint_margin = self.paint_dirty_margin();
        let radius_px = self.radius_px.max(1);
        let hardness = self.hardness;
        let blend_colors = self.blend_colors;
        let Some(stroke) = self.scratch.as_mut() else {
            return false;
        };
        if from.page_idx != to.page_idx || stroke.page_idx != to.page_idx {
            return false;
        }
        let Some((x0, y0, sx0, sy0)) =
            Self::stroke_point_to_overlay_and_scratch(canvas, stroke, from, strict_pixel_painting)
        else {
            return false;
        };
        let Some((x1, y1, sx1, sy1)) =
            Self::stroke_point_to_overlay_and_scratch(canvas, stroke, to, strict_pixel_painting)
        else {
            return false;
        };
        paint_line_mask_with_hardness(
            &mut stroke.mask,
            stroke.image.size,
            sx0,
            sy0,
            sx1,
            sy1,
            radius_px as i32,
            hardness,
            strict_pixel_painting,
        );
        let dirty_overlay =
            segment_dirty_overlay_rect(x0, y0, x1, y1, paint_margin, stroke.overlay_rect);
        if let Some(dirty_overlay) = dirty_overlay {
            apply_cleaning_brush_stroke(
                &mut stroke.image,
                &stroke.base_image,
                &stroke.mask,
                color,
                erase,
                blend_colors,
                overlay_rect_to_local_rect(stroke.overlay_rect, dirty_overlay),
            );
            mark_scratch_dirty_rect(stroke, dirty_overlay);
        }
        true
    }

    pub fn paint_overlay_segment(
        &self,
        canvas: &mut CanvasView,
        from: StrokePoint,
        to: StrokePoint,
        color: Color32,
        erase: bool,
    ) -> bool {
        if from.page_idx != to.page_idx {
            return false;
        }
        let page_idx = from.page_idx;
        if !Self::ensure_overlay_under_point(canvas, from) {
            return false;
        }
        let Some((x0, y0, x0f, y0f)) =
            Self::stroke_point_to_overlay(canvas, from, self.strict_pixel_painting)
        else {
            return false;
        };
        let Some((x1, y1, x1f, y1f)) =
            Self::stroke_point_to_overlay(canvas, to, self.strict_pixel_painting)
        else {
            return false;
        };
        let Some([overlay_w, overlay_h]) = canvas.overlay_size(page_idx) else {
            return false;
        };
        let overlay_bounds = OverlayRectPx {
            x: 0,
            y: 0,
            w: overlay_w,
            h: overlay_h,
        };
        let Some(dirty_overlay) =
            segment_dirty_overlay_rect(x0, y0, x1, y1, self.paint_dirty_margin(), overlay_bounds)
        else {
            return false;
        };
        let Some(page_scene_rect) = canvas.page_scene_rect(page_idx) else {
            return false;
        };
        let Some(scene_rect) =
            overlay_rect_to_scene_rect(page_scene_rect, overlay_w, overlay_h, dirty_overlay)
        else {
            return false;
        };
        let overlay = canvas.overlay_image(page_idx);
        let base_patch = overlay
            .map(|image| extract_overlay_chunk(image, dirty_overlay))
            .unwrap_or_else(|| {
                egui::ColorImage::filled([dirty_overlay.w, dirty_overlay.h], Color32::TRANSPARENT)
            });
        let mut patch = base_patch.clone();
        let mut mask = vec![0.0; patch.pixels.len()];
        paint_line_mask_with_hardness(
            &mut mask,
            patch.size,
            x0f - dirty_overlay.x as f32,
            y0f - dirty_overlay.y as f32,
            x1f - dirty_overlay.x as f32,
            y1f - dirty_overlay.y as f32,
            self.radius_px.max(1) as i32,
            self.hardness,
            self.strict_pixel_painting,
        );
        apply_cleaning_brush_stroke(
            &mut patch,
            &base_patch,
            &mask,
            color,
            erase,
            self.blend_colors,
            None,
        );
        canvas.replace_overlay_region(page_idx, scene_rect, &patch)
    }

    pub fn commit_scratch_stroke(&mut self, canvas: &mut CanvasView) -> Option<usize> {
        let stroke = self.scratch.take()?;
        let dirty_rect = stroke.dirty_rect?;
        let page_scene_rect = canvas.page_scene_rect(stroke.page_idx)?;
        let [overlay_w, overlay_h] = canvas.overlay_size(stroke.page_idx)?;
        let scene_rect =
            overlay_rect_to_scene_rect(page_scene_rect, overlay_w, overlay_h, dirty_rect)?;
        let patch = extract_local_chunk(&stroke.image, stroke.overlay_rect, dirty_rect);
        if canvas.replace_overlay_region(stroke.page_idx, scene_rect, &patch) {
            return Some(stroke.page_idx);
        }
        None
    }

    pub fn cancel_scratch_stroke(&mut self) {
        self.scratch = None;
    }

    pub fn draw_scratch_preview(&mut self, ui: &mut egui::Ui, canvas: &CanvasView) {
        let Some(stroke) = self.scratch.as_mut() else {
            return;
        };
        let texture_options = scratch_preview_texture_options(canvas);
        ensure_scratch_tiles(stroke, ui.ctx(), texture_options);
        upload_dirty_scratch_tiles(stroke, texture_options);
        paint_scratch_tiles(stroke, ui);
    }

    fn paint_dirty_margin(&self) -> usize {
        let radius = self.radius_px.max(1);
        if self.strict_pixel_painting {
            radius
        } else {
            radius.saturating_add(1)
        }
    }

    fn stroke_point_to_overlay(
        canvas: &CanvasView,
        point: StrokePoint,
        strict_pixel_painting: bool,
    ) -> Option<(usize, usize, f32, f32)> {
        if strict_pixel_painting {
            let (x, y) = canvas.scene_point_to_overlay_xy(point.page_idx, point.scene_pos)?;
            return Some((x, y, x as f32, y as f32));
        }
        let page_rect = canvas.page_scene_rect(point.page_idx)?;
        let [overlay_w, overlay_h] = canvas.overlay_size(point.page_idx)?;
        if overlay_w == 0 || overlay_h == 0 {
            return None;
        }
        let u = ((point.scene_pos.x - page_rect.left()) / page_rect.width()).clamp(0.0, 1.0);
        let v = ((point.scene_pos.y - page_rect.top()) / page_rect.height()).clamp(0.0, 1.0);
        let x = (u * overlay_w as f32 - 0.5).clamp(0.0, overlay_w.saturating_sub(1) as f32);
        let y = (v * overlay_h as f32 - 0.5).clamp(0.0, overlay_h.saturating_sub(1) as f32);
        Some((x.round() as usize, y.round() as usize, x, y))
    }

    fn stroke_point_to_overlay_and_scratch(
        canvas: &CanvasView,
        stroke: &ScratchStroke,
        point: StrokePoint,
        strict_pixel_painting: bool,
    ) -> Option<(usize, usize, f32, f32)> {
        let (x, y, xf, yf) = Self::stroke_point_to_overlay(canvas, point, strict_pixel_painting)?;
        let sx = xf - stroke.overlay_rect.x as f32;
        let sy = yf - stroke.overlay_rect.y as f32;
        if sx < 0.0
            || sy < 0.0
            || sx > stroke.overlay_rect.w.saturating_sub(1) as f32
            || sy > stroke.overlay_rect.h.saturating_sub(1) as f32
        {
            return None;
        }
        Some((x, y, sx, sy))
    }
}

#[derive(Clone)]
struct PendingRegionSelection {
    page_idx: usize,
    scene_rect: Rect,
    source_rect: OverlayRectPx,
    source_size: [usize; 2],
    overlay_chunk: Option<egui::ColorImage>,
}

struct RegionLoadRequest {
    job_id: u64,
    page_idx: usize,
    source_rect: OverlayRectPx,
    source_size: [usize; 2],
    page_path: PathBuf,
    overlay_chunk: Option<egui::ColorImage>,
    shared_overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
}

struct RegionLoadResult {
    job_id: u64,
    page_idx: usize,
    target_rect_px: OverlayRectPx,
    image: Result<egui::ColorImage, String>,
}

pub struct RegionEditorSession {
    pub page_idx: usize,
    pub target_rect_px: OverlayRectPx,
    pub image: egui::ColorImage,
    pub texture: Option<TextureHandle>,
    pub texture_dirty: bool,
    pub status: Option<String>,
    pub zoom: f32,
    pub scroll_id: u64,
    pub zoom_drag_active: bool,
    pub zoom_drag_last_x: f32,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct RegionEditorStrokeSegment {
    pub from_x: i32,
    pub from_y: i32,
    pub to_x: i32,
    pub to_y: i32,
    pub erase: bool,
    pub shift: bool,
}

impl RegionEditorSession {
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    pub fn set_zoom(&mut self, value: f32) -> bool {
        let clamped = value.clamp(REGION_EDITOR_MIN_ZOOM, REGION_EDITOR_MAX_ZOOM);
        if (clamped - self.zoom).abs() <= f32::EPSILON {
            return false;
        }
        self.zoom = clamped;
        true
    }

    pub fn scale_zoom(&mut self, factor: f32) -> bool {
        if factor <= 0.0 {
            return false;
        }
        self.set_zoom(self.zoom * factor)
    }

    pub fn zoomed_image_size(&self) -> egui::Vec2 {
        let w = self.image.size[0].max(1) as f32;
        let h = self.image.size[1].max(1) as f32;
        egui::vec2((w * self.zoom).max(1.0), (h * self.zoom).max(1.0))
    }
}

pub struct RegionEditToolBase {
    window_id: String,
    selection_multiple: Option<usize>,
    selecting_page_idx: Option<usize>,
    selection_start_scene: Option<Pos2>,
    selection_current_scene: Option<Pos2>,
    selection_rect_scene: Option<Rect>,
    pending_selection: Option<PendingRegionSelection>,
    pending_job_id: Option<u64>,
    next_job_id: u64,
    load_error: Option<String>,
    load_tx: Sender<Option<RegionLoadRequest>>,
    load_rx: Receiver<RegionLoadResult>,
    load_thread: Option<JoinHandle<()>>,
    editor_session_id: u64,
    editor_window_rect: Option<Rect>,
    editor: Option<RegionEditorSession>,
    force_center_after_load: bool,
}

impl RegionEditToolBase {
    pub fn new(window_id: impl Into<String>, selection_multiple: Option<usize>) -> Self {
        let (load_tx, load_rx, load_thread) = spawn_region_loader_thread();
        Self {
            window_id: window_id.into(),
            selection_multiple,
            selecting_page_idx: None,
            selection_start_scene: None,
            selection_current_scene: None,
            selection_rect_scene: None,
            pending_selection: None,
            pending_job_id: None,
            next_job_id: 1,
            load_error: None,
            load_tx,
            load_rx,
            load_thread: Some(load_thread),
            editor_session_id: 0,
            editor_window_rect: None,
            editor: None,
            force_center_after_load: false,
        }
    }

    pub fn wants_primary_stroke(&self, point: StrokePoint) -> bool {
        point.modifiers.shift
    }

    pub fn begin_selection(&mut self, canvas: &CanvasView, point: StrokePoint) {
        self.selecting_page_idx = Some(point.page_idx);
        self.selection_start_scene = Some(point.scene_pos);
        self.selection_current_scene = Some(point.scene_pos);
        self.selection_rect_scene = self
            .build_selection(canvas, point.page_idx, point.scene_pos, point.scene_pos)
            .map(|sel| sel.scene_rect);
    }

    pub fn update_selection(&mut self, canvas: &CanvasView, point: StrokePoint) {
        if self.selecting_page_idx != Some(point.page_idx) {
            return;
        }
        let Some(start) = self.selection_start_scene else {
            return;
        };
        self.selection_current_scene = Some(point.scene_pos);
        self.selection_rect_scene = self
            .build_selection(canvas, point.page_idx, start, point.scene_pos)
            .map(|sel| sel.scene_rect);
    }

    pub fn end_selection(&mut self, canvas: &CanvasView) {
        let page_idx = self.selecting_page_idx.take();
        let start = self.selection_start_scene.take();
        let current = self.selection_current_scene.take();
        self.selection_rect_scene = None;
        let (Some(page_idx), Some(start), Some(current)) = (page_idx, start, current) else {
            return;
        };
        let Some(mut selection) = self.build_selection(canvas, page_idx, start, current) else {
            return;
        };
        selection.overlay_chunk = capture_overlay_chunk(canvas, page_idx, selection.scene_rect);
        self.pending_selection = Some(selection);
        self.load_error = None;
    }

    pub fn cancel_selection(&mut self) {
        self.selecting_page_idx = None;
        self.selection_start_scene = None;
        self.selection_current_scene = None;
        self.selection_rect_scene = None;
        self.pending_selection = None;
        self.pending_job_id = None;
        self.editor_window_rect = None;
        self.force_center_after_load = false;
    }

    pub fn editor_window_contains(&self, pointer_pos: Pos2) -> bool {
        self.editor_window_rect
            .as_ref()
            .is_some_and(|rect| rect.contains(pointer_pos))
    }

    pub fn has_open_editor(&self) -> bool {
        self.editor.is_some()
    }

    pub fn draw_ui_hint(&self, ui: &mut egui::Ui) {
        ui.label("Выделение: Shift+ЛКМ (прямоугольник)");
        ui.small("Esc: отмена рамки");
        if let Some(mult) = self.selection_multiple
            && mult > 1
        {
            ui.small(format!("Кратность выделения: {mult}px"));
        }
        if self.pending_job_id.is_some() {
            ui.small("Загрузка выделенной области...");
        }
        if let Some(err) = self.load_error.as_ref() {
            ui.colored_label(Color32::from_rgb(255, 120, 120), err);
        }
    }

    pub fn draw_region_editor_zoom_controls(
        ui: &mut egui::Ui,
        editor: &mut RegionEditorSession,
    ) -> bool {
        let mut changed = false;
        if ui.small_button("-").clicked() {
            changed |= editor.scale_zoom(1.0 / REGION_EDITOR_ZOOM_STEP);
        }
        let mut zoom = editor.zoom();
        if ui
            .add(
                WheelSlider::new(&mut zoom, REGION_EDITOR_MIN_ZOOM..=REGION_EDITOR_MAX_ZOOM)
                    .logarithmic(true)
                    .text("Зум"),
            )
            .changed()
        {
            changed |= editor.set_zoom(zoom);
        }
        if ui.small_button("+").clicked() {
            changed |= editor.scale_zoom(REGION_EDITOR_ZOOM_STEP);
        }
        if ui.small_button("1:1").clicked() {
            changed |= editor.set_zoom(1.0);
        }
        ui.label(format!("{:.0}%", editor.zoom() * 100.0));
        changed
    }

    pub fn draw_region_editor_scroll_area<R, AddContents>(
        ui: &mut egui::Ui,
        scroll_id: u64,
        content_size: egui::Vec2,
        add_contents: AddContents,
    ) -> R
    where
        AddContents: FnOnce(&mut egui::Ui) -> R,
    {
        let max_width = content_size.x.max(1.0).min(ui.available_width().max(1.0));
        let max_height = content_size.y.max(1.0).min(ui.available_height().max(1.0));
        egui::ScrollArea::both()
            .id_salt(("cleaning_region_editor_scroll", scroll_id))
            .auto_shrink([false, false])
            .max_width(max_width)
            .max_height(max_height)
            .show(ui, add_contents)
            .inner
    }

    pub fn draw_region_editor_collapsible_section<AddContents>(
        ui: &mut egui::Ui,
        id_source: impl std::hash::Hash,
        title: &str,
        default_open: bool,
        add_contents: AddContents,
    ) where
        AddContents: FnOnce(&mut egui::Ui),
    {
        ui.separator();
        egui::CollapsingHeader::new(title)
            .id_salt(("cleaning_region_editor_section", id_source))
            .default_open(default_open)
            .show(ui, add_contents);
    }

    #[allow(dead_code)]
    pub fn draw_region_editor_image_with_stroke_input<OnStroke>(
        ui: &mut egui::Ui,
        editor: &mut RegionEditorSession,
        last_drag_px: &mut Option<(i32, i32)>,
        mut on_stroke: OnStroke,
    ) where
        OnStroke: FnMut(&mut RegionEditorSession, RegionEditorStrokeSegment) -> bool,
    {
        Self::ensure_region_editor_texture(editor, ui.ctx());
        let preview_size = editor.zoomed_image_size();
        let scroll_id = editor.scroll_id;
        Self::draw_region_editor_scroll_area(ui, scroll_id, preview_size, |ui| {
            let Some(texture) = editor.texture.as_ref() else {
                return;
            };
            let response = ui.add(
                egui::Image::new((texture.id(), preview_size)).sense(egui::Sense::click_and_drag()),
            );

            let (primary_down, secondary_down, mods, z_down) = ui.ctx().input(|i| {
                (
                    i.pointer.primary_down(),
                    i.pointer.secondary_down(),
                    i.modifiers,
                    i.key_down(egui::Key::Z),
                )
            });
            let zoom_modifier_down = mods.ctrl || mods.command || z_down;
            if zoom_modifier_down || editor.zoom_drag_active {
                *last_drag_px = None;
            }

            let mut painted = false;
            let mut base_image_changed = false;
            if let Some(pointer_pos) = response.interact_pointer_pos()
                && response.rect.contains(pointer_pos)
                && (primary_down || secondary_down)
                && !zoom_modifier_down
                && !editor.zoom_drag_active
            {
                let (to_x, to_y) =
                    scene_pointer_to_image_px(pointer_pos, response.rect, editor.image.size);
                let (from_x, from_y) = last_drag_px.unwrap_or((to_x, to_y));
                let erase = secondary_down && !primary_down;
                base_image_changed |= on_stroke(
                    editor,
                    RegionEditorStrokeSegment {
                        from_x,
                        from_y,
                        to_x,
                        to_y,
                        erase,
                        shift: mods.shift,
                    },
                );
                *last_drag_px = Some((to_x, to_y));
                painted = true;
            }

            if painted {
                if base_image_changed {
                    editor.texture_dirty = true;
                }
                ui.ctx().request_repaint();
            }

            if !(primary_down || secondary_down) {
                *last_drag_px = None;
            }
        });
    }

    #[allow(dead_code)]
    pub fn draw_overlay_ui<OnOpen, DrawEditor>(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        window_title: &str,
        on_open: OnOpen,
        mut draw_editor: DrawEditor,
    ) where
        OnOpen: FnMut(&mut RegionEditorSession),
        DrawEditor: FnMut(&mut egui::Ui, &mut RegionEditorSession),
    {
        self.draw_overlay_ui_custom(
            ctx,
            canvas,
            project,
            window_title,
            on_open,
            |ui, editor, request_close, apply_clicked| {
                draw_editor(ui, editor);
                if let Some(status) = editor.status.as_ref() {
                    ui.separator();
                    ui.small(status);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Отмена").clicked() {
                        *request_close = true;
                    }
                    if ui.button("Применить").clicked() {
                        *apply_clicked = true;
                    }
                });
            },
        );
    }

    pub fn draw_overlay_ui_custom<OnOpen, DrawWindow>(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        window_title: &str,
        mut on_open: OnOpen,
        mut draw_window: DrawWindow,
    ) where
        OnOpen: FnMut(&mut RegionEditorSession),
        DrawWindow: FnMut(&mut egui::Ui, &mut RegionEditorSession, &mut bool, &mut bool),
    {
        self.start_pending_job(canvas, project);
        self.poll_loaded_regions(&mut on_open);

        if self.pending_job_id.is_some() {
            ctx.request_repaint();
        }

        let mut keep_open = true;
        let mut apply_clicked = false;
        let mut request_close = false;
        let mut cancel_pending = false;
        let prev_window_rect = self.editor_window_rect;
        self.editor_window_rect = None;
        if let Some(editor) = self.editor.as_mut() {
            let viewport = ctx.content_rect();
            let viewport_cap = viewport.size() * REGION_EDITOR_WINDOW_VIEWPORT_CAP_FRAC;
            let window_size =
                region_editor_target_window_size(editor.zoomed_image_size(), viewport_cap);
            let mut window = egui::Window::new(window_title)
                .id(egui::Id::new((
                    "cleaning_region_editor",
                    &self.window_id,
                    self.editor_session_id,
                )))
                .fixed_size(window_size)
                .resizable(false)
                .collapsible(false)
                .open(&mut keep_open);
            let window_pos = region_editor_window_pos(
                prev_window_rect,
                window_size,
                viewport,
                self.force_center_after_load,
            );
            window = window.current_pos(window_pos);
            let shown = window.show(ctx, |ui| {
                draw_window(ui, editor, &mut request_close, &mut apply_clicked);
            });
            self.force_center_after_load = false;
            if let Some(resp) = shown {
                let window_rect = resp.response.rect;
                self.editor_window_rect = Some(window_rect);
                Self::handle_region_editor_zoom_input(ctx, editor, window_rect);
            }
        } else if let Some(pending_job_id) = self.pending_job_id {
            let viewport = ctx.content_rect();
            let mut keep_loading_window = true;
            let mut window = egui::Window::new(window_title)
                .id(egui::Id::new((
                    "cleaning_region_editor_loading",
                    &self.window_id,
                    pending_job_id,
                )))
                .fixed_size(egui::vec2(
                    REGION_EDITOR_LOADING_WINDOW_W,
                    REGION_EDITOR_LOADING_WINDOW_H,
                ))
                .resizable(false)
                .collapsible(false)
                .open(&mut keep_loading_window);
            let loading_size = egui::vec2(
                REGION_EDITOR_LOADING_WINDOW_W,
                REGION_EDITOR_LOADING_WINDOW_H,
            );
            let window_pos =
                region_editor_window_pos(prev_window_rect, loading_size, viewport, false);
            window = window.current_pos(window_pos);
            let shown = window.show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(4.0);
                    ui.spinner();
                    ui.add_space(6.0);
                    ui.label("Подготавливаю выделенную область...");
                    ui.add_space(6.0);
                    if ui.button("Отмена").clicked() {
                        cancel_pending = true;
                    }
                });
            });
            if let Some(resp) = shown {
                self.editor_window_rect = Some(resp.response.rect);
            }
            if !keep_loading_window {
                cancel_pending = true;
            }
        }
        if request_close {
            keep_open = false;
        }

        if apply_clicked && let Some(editor) = self.editor.as_mut() {
            if canvas.replace_overlay_region_px(
                editor.page_idx,
                editor.target_rect_px,
                &editor.image,
            ) {
                keep_open = false;
            } else {
                editor.status =
                    Some("Не удалось вставить результат в слой clean overlay.".to_string());
            }
        }
        if cancel_pending {
            self.pending_job_id = None;
            self.pending_selection = None;
            self.editor = None;
            self.editor_window_rect = None;
            self.force_center_after_load = false;
        }
        if !keep_open {
            self.editor = None;
            self.editor_window_rect = None;
            self.force_center_after_load = false;
        }
    }

    fn handle_region_editor_zoom_input(
        ctx: &egui::Context,
        editor: &mut RegionEditorSession,
        window_rect: Rect,
    ) {
        let (mods, hover_pos, interact_pos, z_down, primary_down, wheel_delta_y) = ctx.input(|i| {
            (
                i.modifiers,
                i.pointer.hover_pos(),
                i.pointer.interact_pos(),
                i.key_down(egui::Key::Z),
                i.pointer.primary_down(),
                i.smooth_scroll_delta.y + i.raw_scroll_delta.y,
            )
        });
        let ctrl_or_command = mods.ctrl || mods.command;
        let zoom_modifier_down = ctrl_or_command || z_down;
        let pointer_pos = interact_pos.or(hover_pos);
        let inside_window = pointer_pos
            .map(|pos| window_rect.contains(pos))
            .unwrap_or(false);
        let mut changed = false;

        if inside_window && zoom_modifier_down && wheel_delta_y.abs() > f32::EPSILON {
            let factor = if wheel_delta_y > 0.0 {
                REGION_EDITOR_ZOOM_STEP
            } else {
                1.0 / REGION_EDITOR_ZOOM_STEP
            };
            changed |= editor.scale_zoom(factor);
            // Keep wheel delta from being interpreted as content scroll in the same frame.
            ctx.input_mut(|i| {
                i.smooth_scroll_delta = egui::Vec2::ZERO;
                i.raw_scroll_delta = egui::Vec2::ZERO;
            });
        }

        if editor.zoom_drag_active {
            if !zoom_modifier_down || !primary_down {
                editor.zoom_drag_active = false;
            } else if let Some(pos) = pointer_pos {
                let dx = pos.x - editor.zoom_drag_last_x;
                if dx.abs() > f32::EPSILON {
                    let factor = (dx * 0.005).exp().clamp(0.5, 2.0);
                    changed |= editor.scale_zoom(factor);
                }
                editor.zoom_drag_last_x = pos.x;
            }
        } else if zoom_modifier_down
            && primary_down
            && inside_window
            && let Some(pos) = pointer_pos
        {
            editor.zoom_drag_active = true;
            editor.zoom_drag_last_x = pos.x;
        }

        if zoom_modifier_down && inside_window && !ctx.wants_keyboard_input() {
            let mut zoom_in = false;
            let mut zoom_out = false;
            let mut zoom_reset = false;
            let key_modifiers = [
                ctrl_or_command.then_some(egui::Modifiers::COMMAND),
                z_down.then_some(egui::Modifiers::NONE),
            ];
            for key_mods in key_modifiers.into_iter().flatten() {
                let (plus, equals, minus, zero) = ctx.input_mut(|i| {
                    (
                        i.consume_key(key_mods, egui::Key::Plus),
                        i.consume_key(key_mods, egui::Key::Equals),
                        i.consume_key(key_mods, egui::Key::Minus),
                        i.consume_key(key_mods, egui::Key::Num0),
                    )
                });
                zoom_in |= plus || equals;
                zoom_out |= minus;
                zoom_reset |= zero;
            }
            if zoom_reset {
                changed |= editor.set_zoom(1.0);
            } else {
                if zoom_in {
                    changed |= editor.scale_zoom(REGION_EDITOR_ZOOM_STEP);
                }
                if zoom_out {
                    changed |= editor.scale_zoom(1.0 / REGION_EDITOR_ZOOM_STEP);
                }
            }
        }

        if changed || editor.zoom_drag_active {
            ctx.request_repaint();
        }
    }

    fn ensure_region_editor_texture(editor: &mut RegionEditorSession, ctx: &egui::Context) {
        if editor.texture.is_none() {
            let texture = ctx.load_texture(
                format!("cleaning-region-editor-{}", editor.scroll_id),
                editor.image.clone(),
                REGION_EDITOR_TEXTURE_OPTIONS,
            );
            editor.texture = Some(texture);
            editor.texture_dirty = false;
            return;
        }
        if editor.texture_dirty {
            if let Some(texture) = editor.texture.as_mut() {
                texture.set(editor.image.clone(), REGION_EDITOR_TEXTURE_OPTIONS);
            }
            editor.texture_dirty = false;
        }
    }

    pub fn draw_cursor(
        &self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<Pos2>,
    ) {
        let Some(pointer_scene_pos) = pointer_scene_pos else {
            return;
        };
        if canvas.page_index_at_scene_pos(pointer_scene_pos).is_some() {
            ui.ctx()
                .output_mut(|out| out.cursor_icon = egui::CursorIcon::None);
            let cross = 15.0;
            ui.painter().line_segment(
                [
                    egui::pos2(pointer_scene_pos.x - cross, pointer_scene_pos.y),
                    egui::pos2(pointer_scene_pos.x + cross, pointer_scene_pos.y),
                ],
                egui::Stroke::new(3.0, Color32::BLACK),
            );
            ui.painter().line_segment(
                [
                    egui::pos2(pointer_scene_pos.x - cross, pointer_scene_pos.y),
                    egui::pos2(pointer_scene_pos.x + cross, pointer_scene_pos.y),
                ],
                egui::Stroke::new(1.0, Color32::WHITE),
            );
            ui.painter().line_segment(
                [
                    egui::pos2(pointer_scene_pos.x, pointer_scene_pos.y - cross),
                    egui::pos2(pointer_scene_pos.x, pointer_scene_pos.y + cross),
                ],
                egui::Stroke::new(3.0, Color32::BLACK),
            );
            ui.painter().line_segment(
                [
                    egui::pos2(pointer_scene_pos.x, pointer_scene_pos.y - cross),
                    egui::pos2(pointer_scene_pos.x, pointer_scene_pos.y + cross),
                ],
                egui::Stroke::new(1.0, Color32::WHITE),
            );
        }
        if let Some(rect) = self.selection_rect_scene {
            ui.painter().rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(3.0, Color32::BLACK),
                egui::StrokeKind::Outside,
            );
            ui.painter().rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(1.0, Color32::WHITE),
                egui::StrokeKind::Outside,
            );
        }
    }

    fn start_pending_job(&mut self, canvas: &CanvasView, project: &ProjectData) {
        if self.pending_job_id.is_some() {
            return;
        }
        let Some(selection) = self.pending_selection.take() else {
            return;
        };
        let Some(page) = project
            .pages
            .iter()
            .find(|page| page.idx == selection.page_idx)
        else {
            self.load_error = Some(format!(
                "Не найдена страница с idx={} для выделенного региона.",
                selection.page_idx
            ));
            return;
        };

        let job_id = self.next_job_id;
        self.next_job_id = self.next_job_id.saturating_add(1);
        let req = RegionLoadRequest {
            job_id,
            page_idx: selection.page_idx,
            source_rect: selection.source_rect,
            source_size: selection.source_size,
            page_path: page.path.clone(),
            overlay_chunk: selection.overlay_chunk,
            shared_overlays_model: canvas.clean_overlays_model_handle(),
        };
        if self.load_tx.send(Some(req)).is_ok() {
            self.pending_job_id = Some(job_id);
            self.load_error = None;
            self.force_center_after_load = true;
        } else {
            self.load_error = Some("Не удалось отправить задачу загрузки региона.".to_string());
            self.force_center_after_load = false;
        }
    }

    fn poll_loaded_regions<OnOpen>(&mut self, on_open: &mut OnOpen)
    where
        OnOpen: FnMut(&mut RegionEditorSession),
    {
        loop {
            match self.load_rx.try_recv() {
                Ok(result) => {
                    if self.pending_job_id != Some(result.job_id) {
                        continue;
                    }
                    self.pending_job_id = None;
                    match result.image {
                        Ok(image) => {
                            let mut editor = RegionEditorSession {
                                page_idx: result.page_idx,
                                target_rect_px: result.target_rect_px,
                                image,
                                texture: None,
                                texture_dirty: true,
                                status: None,
                                zoom: 1.0,
                                scroll_id: self.editor_session_id,
                                zoom_drag_active: false,
                                zoom_drag_last_x: 0.0,
                            };
                            on_open(&mut editor);
                            self.editor_session_id = self.editor_session_id.saturating_add(1);
                            self.editor = Some(editor);
                            self.load_error = None;
                        }
                        Err(err) => {
                            self.load_error = Some(err);
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.pending_job_id = None;
                    self.load_error =
                        Some("Поток загрузки region editor завершился неожиданно.".to_string());
                    break;
                }
            }
        }
    }

    fn build_selection(
        &self,
        canvas: &CanvasView,
        page_idx: usize,
        start_scene: Pos2,
        current_scene: Pos2,
    ) -> Option<PendingRegionSelection> {
        let page_rect = canvas.page_scene_rect(page_idx)?;
        if !page_rect.is_positive() {
            return None;
        }

        let source_w = ((page_rect.width() / canvas.state.zoom.max(f32::EPSILON))
            .round()
            .max(1.0)) as usize;
        let source_h = ((page_rect.height() / canvas.state.zoom.max(f32::EPSILON))
            .round()
            .max(1.0)) as usize;
        if source_w == 0 || source_h == 0 {
            return None;
        }

        let start_clamped = clamp_scene_pos_to_rect(start_scene, page_rect);
        let current_clamped = clamp_scene_pos_to_rect(current_scene, page_rect);
        let (sx0, sy0) = scene_pos_to_source_xy(start_clamped, page_rect, source_w, source_h);
        let (mut sx1, mut sy1) =
            scene_pos_to_source_xy(current_clamped, page_rect, source_w, source_h);

        if let Some(mult) = self.selection_multiple.filter(|mult| *mult > 1) {
            sx1 = snap_selection_end(sx0, sx1, source_w as i32, mult);
            sy1 = snap_selection_end(sy0, sy1, source_h as i32, mult);
        }

        let x0 = sx0.min(sx1).clamp(0, source_w as i32);
        let x1 = sx0.max(sx1).clamp(0, source_w as i32);
        let y0 = sy0.min(sy1).clamp(0, source_h as i32);
        let y1 = sy0.max(sy1).clamp(0, source_h as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }

        let source_rect = OverlayRectPx {
            x: x0 as usize,
            y: y0 as usize,
            w: (x1 - x0) as usize,
            h: (y1 - y0) as usize,
        };
        let scene_rect = source_rect_to_scene_rect(page_rect, source_rect, [source_w, source_h])?;
        Some(PendingRegionSelection {
            page_idx,
            scene_rect,
            source_rect,
            source_size: [source_w, source_h],
            overlay_chunk: None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionInpaintMaskTarget {
    Inpaint,
    Sample,
}

#[derive(Clone)]
struct RegionInpaintRunSource {
    image: egui::ColorImage,
    mask: egui::ColorImage,
    sample_mask: Option<egui::ColorImage>,
}

type RegionInpaintRunFn = dyn Fn(
        &egui::ColorImage,
        &egui::ColorImage,
        Option<&egui::ColorImage>,
    ) -> Result<egui::ColorImage, String>
    + Send
    + Sync
    + 'static;

struct RegionInpaintJobResult {
    source: RegionInpaintRunSource,
    result: Result<egui::ColorImage, String>,
}

#[derive(Debug, Clone, Copy)]
struct RegionMaskGenerationParams {
    method: RegionMaskGenerationMethod,
    dilate_size: i32,
}

impl Default for RegionMaskGenerationParams {
    fn default() -> Self {
        Self {
            method: RegionMaskGenerationMethod::ComicTextDetector,
            dilate_size: 7,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionMaskGenerationMethod {
    ComicTextDetector,
    PaddleOcr,
    Surya,
}

impl RegionMaskGenerationMethod {
    fn label(self) -> &'static str {
        match self {
            Self::ComicTextDetector => "ComicTextDetector",
            Self::PaddleOcr => "PaddleOCR",
            Self::Surya => "Surya",
        }
    }
}

struct RegionMaskGenerationJobResult {
    result: Result<egui::ColorImage, String>,
}

struct RegionInpaintEditorState {
    session_scroll_id: u64,
    sample_mask_enabled: bool,
    active_mask_target: RegionInpaintMaskTarget,
    mask: egui::ColorImage,
    mask_texture: Option<TextureHandle>,
    mask_texture_dirty: bool,
    sample_mask: Option<egui::ColorImage>,
    sample_mask_texture: Option<TextureHandle>,
    sample_mask_texture_dirty: bool,
    last_drag_px: Option<(i32, i32)>,
    undo_stack: Vec<RegionInpaintRunSource>,
    rerun_source: Option<RegionInpaintRunSource>,
    processing_rx: Option<Receiver<RegionInpaintJobResult>>,
    mask_generation_rx: Option<Receiver<RegionMaskGenerationJobResult>>,
}

impl RegionInpaintEditorState {
    fn new(session_scroll_id: u64, size: [usize; 2], sample_mask_enabled: bool) -> Self {
        Self {
            session_scroll_id,
            sample_mask_enabled,
            active_mask_target: RegionInpaintMaskTarget::Inpaint,
            mask: egui::ColorImage::filled(size, Color32::TRANSPARENT),
            mask_texture: None,
            mask_texture_dirty: true,
            sample_mask: sample_mask_enabled
                .then(|| egui::ColorImage::filled(size, Color32::TRANSPARENT)),
            sample_mask_texture: None,
            sample_mask_texture_dirty: sample_mask_enabled,
            last_drag_px: None,
            undo_stack: Vec::new(),
            rerun_source: None,
            processing_rx: None,
            mask_generation_rx: None,
        }
    }

    fn reset_masks(&mut self, size: [usize; 2]) {
        self.mask = egui::ColorImage::filled(size, Color32::TRANSPARENT);
        self.mask_texture = None;
        self.mask_texture_dirty = true;
        if self.sample_mask_enabled {
            self.sample_mask = Some(egui::ColorImage::filled(size, Color32::TRANSPARENT));
            self.sample_mask_texture = None;
            self.sample_mask_texture_dirty = true;
        } else {
            self.sample_mask = None;
            self.sample_mask_texture = None;
            self.sample_mask_texture_dirty = false;
        }
        self.last_drag_px = None;
    }
}

pub struct RegionMaskInpaintToolBase {
    region_base: RegionEditToolBase,
    brush_base: MaskBrush,
    editor_state: Option<RegionInpaintEditorState>,
    ai_backend_available: bool,
    ai_backend_torch_available: bool,
    mask_generation_params: RegionMaskGenerationParams,
}

impl RegionMaskInpaintToolBase {
    pub fn new(window_id: impl Into<String>, selection_multiple: Option<usize>) -> Self {
        Self {
            region_base: RegionEditToolBase::new(window_id, selection_multiple),
            brush_base: MaskBrush::default(),
            editor_state: None,
            ai_backend_available: false,
            ai_backend_torch_available: false,
            mask_generation_params: RegionMaskGenerationParams::default(),
        }
    }

    pub fn wants_primary_stroke(&self, point: StrokePoint) -> bool {
        self.region_base.wants_primary_stroke(point)
    }

    pub fn begin_selection(&mut self, canvas: &CanvasView, point: StrokePoint) {
        self.region_base.begin_selection(canvas, point);
    }

    pub fn update_selection(&mut self, canvas: &CanvasView, point: StrokePoint) {
        self.region_base.update_selection(canvas, point);
    }

    pub fn end_selection(&mut self, canvas: &CanvasView) {
        self.region_base.end_selection(canvas);
    }

    pub fn cancel_selection(&mut self) {
        self.region_base.cancel_selection();
        self.editor_state = None;
    }

    pub fn draw_ui_hint(&self, ui: &mut egui::Ui) {
        self.region_base.draw_ui_hint(ui);
        ui.small("Окно: ЛКМ рисует маску, ПКМ/Shift+ЛКМ стирают маску.");
        ui.small("Кнопки: Обработать / Переделать / Вернуть / Применить.");
    }

    pub fn set_space_pan_active(&mut self, active: bool) {
        let _ = active;
    }

    pub fn set_ai_backend_available(&mut self, available: bool) {
        self.ai_backend_available = available;
    }

    pub fn set_ai_backend_torch_available(&mut self, available: bool) {
        self.ai_backend_torch_available = available;
    }

    pub fn on_wheel_event(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        self.brush_base.handle_wheel(delta_y, modifiers)
    }

    pub fn on_key_event(&mut self, ctx: &egui::Context) -> bool {
        let mut handled = self.brush_base.handle_size_shortcuts(ctx);
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.cancel_selection();
            handled = true;
        }
        handled
    }

    pub fn draw_overlay_ui<Run>(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        window_title: &str,
        run: Run,
    ) where
        Run: Fn(&egui::ColorImage, &egui::ColorImage) -> Result<egui::ColorImage, String>
            + Send
            + Sync
            + 'static,
    {
        self.draw_overlay_ui_custom(ctx, canvas, project, window_title, run, |_ui| {});
    }

    pub fn draw_overlay_ui_custom<Run, DrawCustomUi>(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        window_title: &str,
        run: Run,
        draw_custom_ui: DrawCustomUi,
    ) where
        Run: Fn(&egui::ColorImage, &egui::ColorImage) -> Result<egui::ColorImage, String>
            + Send
            + Sync
            + 'static,
        DrawCustomUi: FnMut(&mut egui::Ui),
    {
        let run = Arc::new(
            move |image: &egui::ColorImage,
                  mask: &egui::ColorImage,
                  _sample_mask: Option<&egui::ColorImage>| { run(image, mask) },
        ) as Arc<RegionInpaintRunFn>;
        self.draw_overlay_ui_custom_impl(
            ctx,
            canvas,
            project,
            window_title,
            run,
            false,
            draw_custom_ui,
        );
    }

    pub fn draw_overlay_ui_custom_with_sample_mask<Run, DrawCustomUi>(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        window_title: &str,
        run: Run,
        draw_custom_ui: DrawCustomUi,
    ) where
        Run: Fn(
                &egui::ColorImage,
                &egui::ColorImage,
                Option<&egui::ColorImage>,
            ) -> Result<egui::ColorImage, String>
            + Send
            + Sync
            + 'static,
        DrawCustomUi: FnMut(&mut egui::Ui),
    {
        let run = Arc::new(run) as Arc<RegionInpaintRunFn>;
        self.draw_overlay_ui_custom_impl(
            ctx,
            canvas,
            project,
            window_title,
            run,
            true,
            draw_custom_ui,
        );
    }

    // All parameters are distinct pixel-buffer or layout properties; grouping would obscure rendering intent.
    #[allow(clippy::too_many_arguments)]
    fn draw_overlay_ui_custom_impl<DrawCustomUi>(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        window_title: &str,
        run: Arc<RegionInpaintRunFn>,
        sample_mask_enabled: bool,
        mut draw_custom_ui: DrawCustomUi,
    ) where
        DrawCustomUi: FnMut(&mut egui::Ui),
    {
        let (region_base, brush_base, editor_state) = (
            &mut self.region_base,
            &mut self.brush_base,
            &mut self.editor_state,
        );
        let mask_generation_params = &mut self.mask_generation_params;
        region_base.draw_overlay_ui_custom(
            ctx,
            canvas,
            project,
            window_title,
            |editor| {
                if editor.status.is_none() {
                    editor.status = Some("Нарисуйте маску и нажмите «Обработать».".to_string());
                }
            },
            |ui, editor, request_close, apply_clicked| {
                Self::draw_mask_editor_window(
                    ui,
                    editor,
                    brush_base,
                    editor_state,
                    Arc::clone(&run),
                    sample_mask_enabled,
                    self.ai_backend_available,
                    self.ai_backend_torch_available,
                    mask_generation_params,
                    &mut draw_custom_ui,
                    request_close,
                    apply_clicked,
                );
            },
        );
    }

    pub fn draw_cursor(
        &self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<Pos2>,
    ) {
        self.region_base.draw_cursor(ui, canvas, pointer_scene_pos);
    }

    pub fn editor_window_contains(&self, pointer_pos: Pos2) -> bool {
        self.region_base.editor_window_contains(pointer_pos)
    }

    pub fn has_open_editor(&self) -> bool {
        self.region_base.has_open_editor()
    }

    // All parameters are distinct pixel-buffer or layout properties; grouping would obscure rendering intent.
    #[allow(clippy::too_many_arguments)]
    fn draw_mask_editor_window<Run, DrawCustomUi>(
        ui: &mut egui::Ui,
        editor: &mut RegionEditorSession,
        brush_base: &mut MaskBrush,
        editor_state_slot: &mut Option<RegionInpaintEditorState>,
        run: Arc<Run>,
        sample_mask_enabled: bool,
        ai_backend_available: bool,
        ai_backend_torch_available: bool,
        mask_generation_params: &mut RegionMaskGenerationParams,
        draw_custom_ui: &mut DrawCustomUi,
        request_close: &mut bool,
        apply_clicked: &mut bool,
    ) where
        Run: Fn(
                &egui::ColorImage,
                &egui::ColorImage,
                Option<&egui::ColorImage>,
            ) -> Result<egui::ColorImage, String>
            + Send
            + Sync
            + 'static
            + ?Sized,
        DrawCustomUi: FnMut(&mut egui::Ui),
    {
        let editor_state =
            Self::ensure_editor_state(editor, editor_state_slot, sample_mask_enabled);
        let mut processing = Self::poll_processing_result(editor, editor_state);
        let mut generating_mask = Self::poll_mask_generation_result(editor, editor_state);
        if brush_base.handle_size_shortcuts(ui.ctx()) {
            ui.ctx().request_repaint();
        }

        ui.horizontal(|ui| {
            ui.label("ЛКМ: рисовать | ПКМ/Shift+ЛКМ: стирать");
            let mut radius = brush_base.radius_px();
            if ui
                .add(WheelSlider::new(&mut radius, 1..=200).text("Кисть"))
                .changed()
            {
                brush_base.set_radius_px(radius);
            }
            ui.separator();
            if RegionEditToolBase::draw_region_editor_zoom_controls(ui, editor) {
                ui.ctx().request_repaint();
            }
        });
        if sample_mask_enabled {
            ui.horizontal_wrapped(|ui| {
                ui.label("Режим:");
                ui.selectable_value(
                    &mut editor_state.active_mask_target,
                    RegionInpaintMaskTarget::Inpaint,
                    "Удаление",
                );
                ui.selectable_value(
                    &mut editor_state.active_mask_target,
                    RegionInpaintMaskTarget::Sample,
                    "Пример",
                );
            });
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(Color32::from_rgb(255, 220, 0), "Жёлтая: удаление");
                ui.colored_label(Color32::from_rgb(90, 255, 130), "Зелёная: пример");
            });
        }

        ui.separator();
        let mask_section_id =
            ui.make_persistent_id(("cleaning_region_mask_generation_section", editor.scroll_id));
        egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            mask_section_id,
            false,
        )
        .show_header(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Параметры генерации маски");
                ui.add_space(8.0);
                egui::ComboBox::from_id_salt(("mask_generation_method", editor.scroll_id))
                    .selected_text(mask_generation_params.method.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut mask_generation_params.method,
                            RegionMaskGenerationMethod::PaddleOcr,
                            RegionMaskGenerationMethod::PaddleOcr.label(),
                        );
                        let response = ui.add_enabled(
                            ai_backend_torch_available,
                            egui::Button::new(RegionMaskGenerationMethod::Surya.label()).selected(
                                mask_generation_params.method == RegionMaskGenerationMethod::Surya,
                            ),
                        );
                        let response = if ai_backend_torch_available {
                            response
                        } else {
                            response.on_disabled_hover_text(
                                egui::RichText::new("PyTorch не установлен")
                                    .color(Color32::from_rgb(240, 102, 102)),
                            )
                        };
                        if response.clicked() {
                            mask_generation_params.method = RegionMaskGenerationMethod::Surya;
                        }
                        let response = ui.add_enabled(
                            ai_backend_torch_available,
                            egui::Button::new(
                                RegionMaskGenerationMethod::ComicTextDetector.label(),
                            )
                            .selected(
                                mask_generation_params.method
                                    == RegionMaskGenerationMethod::ComicTextDetector,
                            ),
                        );
                        let response = if ai_backend_torch_available {
                            response
                        } else {
                            response.on_disabled_hover_text(
                                egui::RichText::new("PyTorch не установлен")
                                    .color(Color32::from_rgb(240, 102, 102)),
                            )
                        };
                        if response.clicked() {
                            mask_generation_params.method =
                                RegionMaskGenerationMethod::ComicTextDetector;
                        }
                    });
            });
        })
        .body(|ui| {
            ui.add(
                WheelSlider::new(&mut mask_generation_params.dilate_size, 0..=30)
                    .text("Расширение маски"),
            );
        });
        draw_custom_ui(ui);
        Self::draw_mask_editor_image(ui, editor, brush_base, editor_state);

        if let Some(status) = editor.status.as_ref() {
            ui.separator();
            ui.small(status);
        }

        ui.separator();
        let background_busy =
            editor_state.processing_rx.is_some() || editor_state.mask_generation_rx.is_some();
        let can_generate_mask = !background_busy
            && ai_backend_available
            && match mask_generation_params.method {
                RegionMaskGenerationMethod::ComicTextDetector => ai_backend_torch_available,
                RegionMaskGenerationMethod::PaddleOcr => true,
                RegionMaskGenerationMethod::Surya => ai_backend_torch_available,
            };
        let can_run = !background_busy;
        let can_rerun = can_run && editor_state.rerun_source.is_some();
        let can_undo = can_run && !editor_state.undo_stack.is_empty();
        if !can_run {
            processing = true;
        }
        if editor_state.mask_generation_rx.is_some() {
            generating_mask = true;
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(can_generate_mask, egui::Button::new("Сгенерировать маску"))
                .on_hover_text(match (ai_backend_available, mask_generation_params.method) {
                    (false, _) => "Недоступно: ИИ бэкенд не отвечает.".to_string(),
                    (true, RegionMaskGenerationMethod::ComicTextDetector)
                        if !ai_backend_torch_available =>
                    {
                        "PyTorch не установлен".to_string()
                    }
                    (true, RegionMaskGenerationMethod::Surya) if !ai_backend_torch_available => {
                        "PyTorch не установлен".to_string()
                    }
                    _ => "Отправить текущий регион в выбранный backend-детектор и заполнить маску найденным текстом.".to_string(),
                })
                .clicked()
            {
                Self::run_mask_generation(editor, editor_state, *mask_generation_params);
            }
            if ui
                .add_enabled(can_run, egui::Button::new("Обработать"))
                .clicked()
            {
                Self::run_processing(editor, editor_state, Arc::clone(&run), false);
            }
            if ui
                .add_enabled(can_rerun, egui::Button::new("Переделать"))
                .clicked()
            {
                Self::run_processing(editor, editor_state, Arc::clone(&run), true);
            }
            if ui
                .add_enabled(can_undo, egui::Button::new("Вернуть"))
                .clicked()
            {
                Self::undo_processing(editor, editor_state);
            }
            if processing || generating_mask {
                ui.spinner();
                if processing {
                    if ui
                        .button("Отменить обработку")
                        .on_hover_text("Прервать текущую фоновую операцию (HTTP-запрос к backend может всё ещё завершиться, но результат будет отброшен)")
                        .clicked()
                    {
                        editor_state.processing_rx = None;
                        editor.status = Some("Обработка отменена.".to_string());
                    }
                } else if ui
                    .button("Отменить генерацию")
                    .on_hover_text("Прервать текущую генерацию маски (HTTP-запрос к backend может всё ещё завершиться, но результат будет отброшен)")
                    .clicked()
                {
                    editor_state.mask_generation_rx = None;
                    editor.status = Some("Генерация маски отменена.".to_string());
                }
                ui.separator();
            } else {
                ui.separator();
            }
            if ui.button("Отмена").clicked() {
                *request_close = true;
            }
            if ui
                .add_enabled(can_run, egui::Button::new("Применить"))
                .clicked()
            {
                *apply_clicked = true;
            }
        });

        if processing || generating_mask {
            ui.ctx().request_repaint();
        }
    }

    fn draw_mask_editor_image(
        ui: &mut egui::Ui,
        editor: &mut RegionEditorSession,
        brush_base: &mut MaskBrush,
        editor_state: &mut RegionInpaintEditorState,
    ) {
        RegionEditToolBase::ensure_region_editor_texture(editor, ui.ctx());
        Self::ensure_mask_textures(editor_state, ui.ctx());
        let preview_size = editor.zoomed_image_size();
        let scroll_id = editor.scroll_id;
        RegionEditToolBase::draw_region_editor_scroll_area(ui, scroll_id, preview_size, |ui| {
            let Some(texture) = editor.texture.as_ref() else {
                return;
            };
            let response = ui.add(
                egui::Image::new((texture.id(), preview_size)).sense(egui::Sense::click_and_drag()),
            );
            if let Some(sample_mask_texture) = editor_state.sample_mask_texture.as_ref() {
                ui.painter().image(
                    sample_mask_texture.id(),
                    response.rect,
                    Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            if let Some(mask_texture) = editor_state.mask_texture.as_ref() {
                ui.painter().image(
                    mask_texture.id(),
                    response.rect,
                    Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }

            let (primary_down, secondary_down, mods, z_down, smooth_scroll, raw_scroll) =
                ui.ctx().input(|i| {
                    (
                        i.pointer.primary_down(),
                        i.pointer.secondary_down(),
                        i.modifiers,
                        i.key_down(egui::Key::Z),
                        i.smooth_scroll_delta,
                        i.raw_scroll_delta,
                    )
                });
            let zoom_modifier_down = mods.ctrl || mods.command || z_down;
            if zoom_modifier_down || editor.zoom_drag_active {
                editor_state.last_drag_px = None;
            }

            let mut painted = false;
            if let Some(pointer_pos) = response.interact_pointer_pos()
                && response.rect.contains(pointer_pos)
                && (primary_down || secondary_down)
                && !zoom_modifier_down
                && !editor.zoom_drag_active
            {
                let (to_x, to_y) =
                    scene_pointer_to_image_px(pointer_pos, response.rect, editor.image.size);
                let (from_x, from_y) = editor_state.last_drag_px.unwrap_or((to_x, to_y));
                let erase = (secondary_down && !primary_down) || mods.shift;
                let paint_into_sample = editor_state.sample_mask_enabled
                    && editor_state.active_mask_target == RegionInpaintMaskTarget::Sample;
                if paint_into_sample {
                    if let Some(sample_mask) = editor_state.sample_mask.as_mut() {
                        brush_base.paint_mask_segment(
                            sample_mask,
                            from_x,
                            from_y,
                            to_x,
                            to_y,
                            erase,
                        );
                        editor_state.sample_mask_texture_dirty = true;
                    }
                } else {
                    brush_base.paint_mask_segment(
                        &mut editor_state.mask,
                        from_x,
                        from_y,
                        to_x,
                        to_y,
                        erase,
                    );
                    editor_state.mask_texture_dirty = true;
                }
                editor_state.last_drag_px = Some((to_x, to_y));
                painted = true;
            }
            if painted {
                ui.ctx().request_repaint();
            }
            if !(primary_down || secondary_down) {
                editor_state.last_drag_px = None;
            }

            if response.hovered() {
                let mut wheel_delta = smooth_scroll.y;
                if wheel_delta.abs() <= f32::EPSILON {
                    wheel_delta = raw_scroll.y;
                }
                if wheel_delta.abs() <= f32::EPSILON {
                    wheel_delta = smooth_scroll.x;
                }
                if wheel_delta.abs() <= f32::EPSILON {
                    wheel_delta = raw_scroll.x;
                }
                if mods.shift && !zoom_modifier_down && brush_base.handle_wheel(wheel_delta, mods) {
                    ui.ctx().request_repaint();
                }
            }

            if let Some(pointer_pos) = response.hover_pos().filter(|p| response.rect.contains(*p)) {
                brush_base.draw_circle_cursor_on_image(
                    ui,
                    response.rect,
                    editor.image.size,
                    pointer_pos,
                );
            }
        });
    }

    fn ensure_editor_state<'a>(
        editor: &RegionEditorSession,
        slot: &'a mut Option<RegionInpaintEditorState>,
        sample_mask_enabled: bool,
    ) -> &'a mut RegionInpaintEditorState {
        let reset_session = slot.as_ref().is_none_or(|state| {
            state.session_scroll_id != editor.scroll_id
                || state.sample_mask_enabled != sample_mask_enabled
        });
        if reset_session {
            *slot = Some(RegionInpaintEditorState::new(
                editor.scroll_id,
                editor.image.size,
                sample_mask_enabled,
            ));
        }
        let state = slot
            .as_mut()
            .expect("RegionInpaintEditorState должен существовать после инициализации");
        if state.mask.size != editor.image.size {
            state.reset_masks(editor.image.size);
        }
        state
    }

    fn ensure_mask_textures(editor_state: &mut RegionInpaintEditorState, ctx: &egui::Context) {
        if editor_state.mask_texture.is_none() {
            let tex = ctx.load_texture(
                format!("cleaning-region-mask-{}", editor_state.session_scroll_id),
                build_tinted_mask_preview(&editor_state.mask, [255, 220, 0]),
                REGION_EDITOR_TEXTURE_OPTIONS,
            );
            editor_state.mask_texture = Some(tex);
            editor_state.mask_texture_dirty = false;
        }
        if editor_state.mask_texture_dirty {
            if let Some(texture) = editor_state.mask_texture.as_mut() {
                texture.set(
                    build_tinted_mask_preview(&editor_state.mask, [255, 220, 0]),
                    REGION_EDITOR_TEXTURE_OPTIONS,
                );
            }
            editor_state.mask_texture_dirty = false;
        }

        if !editor_state.sample_mask_enabled {
            editor_state.sample_mask = None;
            editor_state.sample_mask_texture = None;
            editor_state.sample_mask_texture_dirty = false;
            return;
        }

        if editor_state.sample_mask_texture.is_none()
            && let Some(sample_mask) = editor_state.sample_mask.as_ref()
        {
            let tex = ctx.load_texture(
                format!(
                    "cleaning-region-sample-mask-{}",
                    editor_state.session_scroll_id
                ),
                build_tinted_mask_preview(sample_mask, [90, 255, 130]),
                REGION_EDITOR_TEXTURE_OPTIONS,
            );
            editor_state.sample_mask_texture = Some(tex);
            editor_state.sample_mask_texture_dirty = false;
        }
        if editor_state.sample_mask_texture_dirty {
            if let (Some(texture), Some(sample_mask)) = (
                editor_state.sample_mask_texture.as_mut(),
                editor_state.sample_mask.as_ref(),
            ) {
                texture.set(
                    build_tinted_mask_preview(sample_mask, [90, 255, 130]),
                    REGION_EDITOR_TEXTURE_OPTIONS,
                );
            }
            editor_state.sample_mask_texture_dirty = false;
        }
    }

    fn run_processing<Run>(
        editor: &mut RegionEditorSession,
        editor_state: &mut RegionInpaintEditorState,
        run: Arc<Run>,
        use_rerun_source: bool,
    ) where
        Run: Fn(
                &egui::ColorImage,
                &egui::ColorImage,
                Option<&egui::ColorImage>,
            ) -> Result<egui::ColorImage, String>
            + Send
            + Sync
            + 'static
            + ?Sized,
    {
        if editor_state.processing_rx.is_some() {
            editor.status = Some("Обработка уже выполняется...".to_string());
            return;
        }
        let source = if use_rerun_source {
            editor_state.rerun_source.clone()
        } else {
            Some(RegionInpaintRunSource {
                image: editor.image.clone(),
                mask: editor_state.mask.clone(),
                sample_mask: editor_state.sample_mask.clone(),
            })
        };
        let Some(source) = source else {
            editor.status = Some("Нет сохранённого состояния для «Переделать».".to_string());
            return;
        };

        let (tx, rx) = mpsc::channel::<RegionInpaintJobResult>();
        let source_for_worker = source.clone();
        thread::spawn(move || {
            let result = run(
                &source_for_worker.image,
                &source_for_worker.mask,
                source_for_worker.sample_mask.as_ref(),
            );
            let _ = tx.send(RegionInpaintJobResult {
                source: source_for_worker,
                result,
            });
        });
        editor_state.processing_rx = Some(rx);
        editor.status = Some("Обработка в фоне...".to_string());
    }

    fn run_mask_generation(
        editor: &mut RegionEditorSession,
        editor_state: &mut RegionInpaintEditorState,
        params: RegionMaskGenerationParams,
    ) {
        if editor_state.processing_rx.is_some() || editor_state.mask_generation_rx.is_some() {
            editor.status = Some("Фоновая операция уже выполняется...".to_string());
            return;
        }

        let image = editor.image.clone();
        let (tx, rx) = mpsc::channel::<RegionMaskGenerationJobResult>();
        thread::spawn(move || {
            let result = Self::generate_text_mask_from_ai(&image, params);
            let _ = tx.send(RegionMaskGenerationJobResult { result });
        });
        editor_state.mask_generation_rx = Some(rx);
        editor.status = Some("Генерация маски в фоне...".to_string());
    }

    fn poll_processing_result(
        editor: &mut RegionEditorSession,
        editor_state: &mut RegionInpaintEditorState,
    ) -> bool {
        let mut still_processing = false;
        let Some(rx) = editor_state.processing_rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok(job) => {
                editor_state.processing_rx = None;
                match job.result {
                    Ok(out) => {
                        let prev_size = job.source.image.size;
                        let out_size = out.size;
                        editor_state.undo_stack.push(job.source.clone());
                        editor.image = out;
                        editor.texture_dirty = true;
                        editor_state.rerun_source = Some(job.source);
                        editor_state.reset_masks(editor.image.size);
                        if prev_size != out_size {
                            editor.status = Some(format!(
                                "Внимание: размер после обработки изменился ({}x{} -> {}x{}).",
                                prev_size[0], prev_size[1], out_size[0], out_size[1]
                            ));
                        } else {
                            editor.status = Some("Обработка завершена.".to_string());
                        }
                    }
                    Err(err) => {
                        editor.status = Some(format!("Ошибка обработки: {err}"));
                    }
                }
            }
            Err(TryRecvError::Empty) => {
                still_processing = true;
            }
            Err(TryRecvError::Disconnected) => {
                editor_state.processing_rx = None;
                editor.status =
                    Some("Обработка прервана: фоновой поток завершился неожиданно.".to_string());
            }
        }
        still_processing
    }

    fn poll_mask_generation_result(
        editor: &mut RegionEditorSession,
        editor_state: &mut RegionInpaintEditorState,
    ) -> bool {
        let Some(rx) = editor_state.mask_generation_rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok(job) => {
                editor_state.mask_generation_rx = None;
                match job.result {
                    Ok(mask) => {
                        if mask.size != editor.image.size {
                            editor.status = Some(format!(
                                "Ошибка генерации маски: backend вернул размер {}x{}, ожидалось {}x{}.",
                                mask.size[0],
                                mask.size[1],
                                editor.image.size[0],
                                editor.image.size[1]
                            ));
                            return false;
                        }
                        editor_state.mask = mask;
                        editor_state.mask_texture_dirty = true;
                        editor_state.last_drag_px = None;
                        editor.status = Some("Маска сгенерирована.".to_string());
                    }
                    Err(err) => {
                        editor.status = Some(format!("Ошибка генерации маски: {err}"));
                    }
                }
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => {
                editor_state.mask_generation_rx = None;
                editor.status = Some(
                    "Генерация маски прервана: фоновой поток завершился неожиданно.".to_string(),
                );
                false
            }
        }
    }

    fn undo_processing(
        editor: &mut RegionEditorSession,
        editor_state: &mut RegionInpaintEditorState,
    ) {
        let Some(source) = editor_state.undo_stack.pop() else {
            editor.status = Some("Нет состояния для отката.".to_string());
            return;
        };
        editor.image = source.image.clone();
        editor.texture_dirty = true;
        editor_state.mask = source.mask.clone();
        editor_state.mask_texture = None;
        editor_state.mask_texture_dirty = true;
        if editor_state.sample_mask_enabled {
            editor_state.sample_mask = Some(source.sample_mask.clone().unwrap_or_else(|| {
                egui::ColorImage::filled(editor.image.size, Color32::TRANSPARENT)
            }));
            editor_state.sample_mask_texture = None;
            editor_state.sample_mask_texture_dirty = true;
        } else {
            editor_state.sample_mask = None;
            editor_state.sample_mask_texture = None;
            editor_state.sample_mask_texture_dirty = false;
        }
        editor_state.last_drag_px = None;
        editor_state.rerun_source = Some(source);
        editor.status = Some("Возврат к состоянию до обработки.".to_string());
    }

    fn generate_text_mask_from_ai(
        image: &egui::ColorImage,
        params: RegionMaskGenerationParams,
    ) -> Result<egui::ColorImage, String> {
        let (mask_size, mask_alpha, method_label) = match params.method {
            RegionMaskGenerationMethod::ComicTextDetector => {
                let options = TextDetectorAiCtdOptions {
                    mask_dilate_size: params.dilate_size,
                    ..TextDetectorAiCtdOptions::default()
                };
                let (mask_size, mask_alpha) = detect_ai_ctd_mask_for_image(image, &options)?;
                (mask_size, mask_alpha, "ComicTextDetector")
            }
            RegionMaskGenerationMethod::PaddleOcr => {
                let options = TextDetectorPaddleOcrOptions {
                    mask_dilate_size: params.dilate_size,
                };
                let (mask_size, mask_alpha) = detect_paddle_mask_for_image(image, &options)?;
                (mask_size, mask_alpha, "PaddleOCR")
            }
            RegionMaskGenerationMethod::Surya => {
                let (mask_size, mask_alpha) =
                    detect_surya_mask_for_image(image, params.dilate_size)?;
                (mask_size, mask_alpha, "Surya")
            }
        };
        let mask_w = usize::try_from(mask_size[0])
            .map_err(|_| format!("{method_label} backend вернул слишком большую ширину маски."))?;
        let mask_h = usize::try_from(mask_size[1])
            .map_err(|_| format!("{method_label} backend вернул слишком большую высоту маски."))?;
        if [mask_w, mask_h] != image.size {
            return Err(format!(
                "{method_label} backend вернул размер маски {}x{}, ожидалось {}x{}.",
                mask_w, mask_h, image.size[0], image.size[1]
            ));
        }
        let expected_len = mask_w.saturating_mul(mask_h);
        if expected_len != mask_alpha.len() {
            return Err(format!(
                "{method_label} backend вернул некорректную длину маски: {} вместо {}.",
                mask_alpha.len(),
                expected_len
            ));
        }
        let mut mask = egui::ColorImage::filled([mask_w, mask_h], Color32::TRANSPARENT);
        for (dst, alpha) in mask.pixels.iter_mut().zip(mask_alpha) {
            if alpha != 0 {
                *dst = Color32::from_rgba_unmultiplied(255, 255, 255, 255);
            }
        }
        Ok(mask)
    }
}

fn build_tinted_mask_preview(mask: &egui::ColorImage, rgb: [u8; 3]) -> egui::ColorImage {
    let mut out = egui::ColorImage::filled(mask.size, Color32::TRANSPARENT);
    for (idx, src) in mask.pixels.iter().enumerate() {
        let alpha = src.a();
        if alpha == 0 {
            continue;
        }
        let preview_alpha = ((alpha as f32) * 0.45).round().clamp(1.0, 255.0) as u8;
        out.pixels[idx] = Color32::from_rgba_unmultiplied(rgb[0], rgb[1], rgb[2], preview_alpha);
    }
    out
}

fn region_editor_target_window_size(content_size: egui::Vec2, max_size: egui::Vec2) -> egui::Vec2 {
    let content_w = content_size.x.max(1.0);
    let content_h = content_size.y.max(1.0);
    // Content size plus fixed UI/chrome budget (toolbar + footer + frame paddings).
    let mut out = egui::vec2(
        content_w + REGION_EDITOR_WINDOW_UI_PAD_X,
        content_h + REGION_EDITOR_WINDOW_UI_PAD_Y,
    );

    let hard_max_x = max_size.x.clamp(1.0, REGION_EDITOR_WINDOW_MAX_W);
    let hard_max_y = max_size.y.clamp(1.0, REGION_EDITOR_WINDOW_MAX_H);
    let hard_min_x = REGION_EDITOR_WINDOW_MIN_W.min(hard_max_x);
    let hard_min_y = REGION_EDITOR_WINDOW_MIN_H.min(hard_max_y);
    out.x = out.x.clamp(hard_min_x, hard_max_x);
    out.y = out.y.clamp(hard_min_y, hard_max_y);
    out
}

fn clamp_window_pos_to_viewport(pos: Pos2, size: egui::Vec2, viewport: Rect) -> Pos2 {
    let min_x = viewport.left();
    let min_y = viewport.top();
    let max_x = (viewport.right() - size.x).max(min_x);
    let max_y = (viewport.bottom() - size.y).max(min_y);
    egui::pos2(pos.x.clamp(min_x, max_x), pos.y.clamp(min_y, max_y))
}

fn region_editor_window_pos(
    prev_window_rect: Option<Rect>,
    size: egui::Vec2,
    viewport: Rect,
    force_center: bool,
) -> Pos2 {
    if !force_center && let Some(prev_rect) = prev_window_rect {
        return clamp_window_pos_to_viewport(prev_rect.min, prev_rect.size(), viewport);
    }
    let centered = viewport.center() - size * 0.5;
    clamp_window_pos_to_viewport(centered, size, viewport)
}

impl Drop for RegionEditToolBase {
    fn drop(&mut self) {
        let _ = self.load_tx.send(None);
        if let Some(thread) = self.load_thread.take() {
            let _ = thread.join();
        }
    }
}

struct DecodedPageCache {
    path: PathBuf,
    image: image::RgbaImage,
}

fn spawn_region_loader_thread() -> (
    Sender<Option<RegionLoadRequest>>,
    Receiver<RegionLoadResult>,
    JoinHandle<()>,
) {
    let (request_tx, request_rx) = mpsc::channel::<Option<RegionLoadRequest>>();
    let (result_tx, result_rx) = mpsc::channel::<RegionLoadResult>();
    let handle = thread::spawn(move || {
        let mut cached_page: Option<DecodedPageCache> = None;
        while let Ok(message) = request_rx.recv() {
            let Some(request) = message else {
                break;
            };
            let model_cached_page = request
                .shared_overlays_model
                .as_ref()
                .and_then(|model| model.lock().ok())
                .and_then(|mut model| model.cached_page_rgba(request.page_idx));
            if let Some(model_cached) = model_cached_page {
                cached_page = Some(DecodedPageCache {
                    path: request.page_path.clone(),
                    image: (*model_cached).clone(),
                });
            }

            let image = if cached_page
                .as_ref()
                .is_some_and(|cached| cached.path == request.page_path)
            {
                if let Some(cached) = cached_page.as_ref() {
                    build_composited_region_image(
                        &cached.image,
                        &request.page_path,
                        request.source_rect,
                        request.source_size,
                        request.overlay_chunk,
                    )
                } else {
                    Err("Внутренняя ошибка кеша region editor.".to_string())
                }
            } else {
                match decode_page_rgba(&request.page_path) {
                    Ok(decoded) => {
                        if let Some(model) = request.shared_overlays_model.as_ref()
                            && let Ok(mut locked) = model.lock()
                        {
                            let _ =
                                locked.store_cached_page_rgba(request.page_idx, decoded.clone());
                        }
                        cached_page = Some(DecodedPageCache {
                            path: request.page_path.clone(),
                            image: decoded,
                        });
                        if let Some(cached) = cached_page.as_ref() {
                            build_composited_region_image(
                                &cached.image,
                                &request.page_path,
                                request.source_rect,
                                request.source_size,
                                request.overlay_chunk,
                            )
                        } else {
                            Err("Не удалось сохранить кеш страницы region editor.".to_string())
                        }
                    }
                    Err(err) => Err(err),
                }
            };
            let _ = result_tx.send(RegionLoadResult {
                job_id: request.job_id,
                page_idx: request.page_idx,
                target_rect_px: request.source_rect,
                image,
            });
        }
    });
    (request_tx, result_rx, handle)
}

fn decode_page_rgba(page_path: &PathBuf) -> Result<image::RgbaImage, String> {
    image::open(page_path)
        .map_err(|err| format!("Не удалось открыть {}: {err}", page_path.display()))
        .map(|img| img.to_rgba8())
}

fn build_composited_region_image(
    base: &image::RgbaImage,
    page_path: &Path,
    source_rect: OverlayRectPx,
    source_size: [usize; 2],
    overlay_chunk: Option<egui::ColorImage>,
) -> Result<egui::ColorImage, String> {
    let source_w = source_size[0];
    let source_h = source_size[1];
    if source_w == 0 || source_h == 0 || source_rect.w == 0 || source_rect.h == 0 {
        return Err("Невалидный размер выделения региона.".to_string());
    }

    let base_w = base.width() as usize;
    let base_h = base.height() as usize;
    if base_w == 0 || base_h == 0 {
        return Err(format!(
            "Страница {} имеет пустой размер.",
            page_path.display()
        ));
    }

    let x0f = source_rect.x as f32 / source_w as f32;
    let y0f = source_rect.y as f32 / source_h as f32;
    let x1f = (source_rect.x + source_rect.w) as f32 / source_w as f32;
    let y1f = (source_rect.y + source_rect.h) as f32 / source_h as f32;
    let mut x0 = (x0f * base_w as f32).round() as isize;
    let mut y0 = (y0f * base_h as f32).round() as isize;
    let mut x1 = (x1f * base_w as f32).round() as isize;
    let mut y1 = (y1f * base_h as f32).round() as isize;
    x0 = x0.clamp(0, base_w as isize);
    y0 = y0.clamp(0, base_h as isize);
    x1 = x1.clamp(0, base_w as isize);
    y1 = y1.clamp(0, base_h as isize);
    if x1 <= x0 {
        x1 = (x0 + 1).min(base_w as isize);
    }
    if y1 <= y0 {
        y1 = (y0 + 1).min(base_h as isize);
    }
    let out_w = (x1 - x0).max(1) as usize;
    let out_h = (y1 - y0).max(1) as usize;
    let mut out_rgba = vec![0u8; out_w.saturating_mul(out_h).saturating_mul(4)];
    let src = base.as_raw();
    let src_stride = base_w.saturating_mul(4);
    let crop_x = (x0 as usize).saturating_mul(4);
    let row_copy_len = out_w.saturating_mul(4);
    for y in 0..out_h {
        let sy = y0 as usize + y;
        let src_row = sy.saturating_mul(src_stride);
        let src_start = src_row.saturating_add(crop_x);
        let src_end = src_start.saturating_add(row_copy_len);
        let dst_row = y.saturating_mul(out_w).saturating_mul(4);
        let dst_end = dst_row.saturating_add(row_copy_len);
        if src_end <= src.len() && dst_end <= out_rgba.len() {
            out_rgba[dst_row..dst_end].copy_from_slice(&src[src_start..src_end]);
        }
    }
    let mut base_chunk = egui::ColorImage::from_rgba_unmultiplied([out_w, out_h], &out_rgba);

    if let Some(overlay_chunk) = overlay_chunk {
        let overlay_chunk = if overlay_chunk.size == base_chunk.size {
            overlay_chunk
        } else {
            resize_color_image_nearest(&overlay_chunk, base_chunk.size)
        };
        composite_source_over(&mut base_chunk, &overlay_chunk);
    }
    Ok(base_chunk)
}

fn resize_color_image_nearest(src: &egui::ColorImage, target_size: [usize; 2]) -> egui::ColorImage {
    let target_w = target_size[0].max(1);
    let target_h = target_size[1].max(1);
    let src_w = src.size[0].max(1);
    let src_h = src.size[1].max(1);
    let mut out = egui::ColorImage::filled([target_w, target_h], Color32::TRANSPARENT);
    for y in 0..target_h {
        let sy = ((y as f32 + 0.5) * src_h as f32 / target_h as f32)
            .floor()
            .clamp(0.0, (src_h - 1) as f32) as usize;
        let dst_row = y.saturating_mul(target_w);
        let src_row = sy.saturating_mul(src_w);
        for x in 0..target_w {
            let sx = ((x as f32 + 0.5) * src_w as f32 / target_w as f32)
                .floor()
                .clamp(0.0, (src_w - 1) as f32) as usize;
            let dst_idx = dst_row.saturating_add(x);
            let src_idx = src_row.saturating_add(sx);
            out.pixels[dst_idx] = src.pixels[src_idx];
        }
    }
    out
}

fn composite_source_over(base: &mut egui::ColorImage, overlay: &egui::ColorImage) {
    if base.size != overlay.size {
        return;
    }
    for (dst_px, src_px) in base.pixels.iter_mut().zip(overlay.pixels.iter()) {
        let sa = src_px.a();
        if sa == 0 {
            continue;
        }
        if sa == 255 || dst_px.a() == 0 {
            *dst_px = *src_px;
            continue;
        }
        let [br, bg, bb, ba] = dst_px.to_srgba_unmultiplied();
        let [sr, sg, sb, sa] = src_px.to_srgba_unmultiplied();
        let ba_f = ba as f32 / 255.0;
        let sa_f = sa as f32 / 255.0;
        let out_a = sa_f + ba_f * (1.0 - sa_f);
        if out_a <= f32::EPSILON {
            *dst_px = Color32::TRANSPARENT;
            continue;
        }
        let out_r = (sr as f32 * sa_f + br as f32 * ba_f * (1.0 - sa_f)) / out_a;
        let out_g = (sg as f32 * sa_f + bg as f32 * ba_f * (1.0 - sa_f)) / out_a;
        let out_b = (sb as f32 * sa_f + bb as f32 * ba_f * (1.0 - sa_f)) / out_a;
        *dst_px = Color32::from_rgba_unmultiplied(
            out_r.round().clamp(0.0, 255.0) as u8,
            out_g.round().clamp(0.0, 255.0) as u8,
            out_b.round().clamp(0.0, 255.0) as u8,
            (out_a * 255.0).round().clamp(0.0, 255.0) as u8,
        );
    }
}

fn capture_overlay_chunk(
    canvas: &CanvasView,
    page_idx: usize,
    scene_rect: Rect,
) -> Option<egui::ColorImage> {
    let overlay = canvas.overlay_image(page_idx)?;
    let overlay_rect = canvas.scene_rect_to_overlay_rect(page_idx, scene_rect)?;
    if overlay_rect.w == 0 || overlay_rect.h == 0 {
        return None;
    }
    Some(extract_overlay_chunk(overlay, overlay_rect))
}

fn source_rect_to_scene_rect(
    page_scene_rect: Rect,
    source_rect: OverlayRectPx,
    source_size: [usize; 2],
) -> Option<Rect> {
    let source_w = source_size[0];
    let source_h = source_size[1];
    if source_w == 0 || source_h == 0 || source_rect.w == 0 || source_rect.h == 0 {
        return None;
    }
    let w = source_w as f32;
    let h = source_h as f32;
    let left = page_scene_rect.left() + page_scene_rect.width() * (source_rect.x as f32 / w);
    let top = page_scene_rect.top() + page_scene_rect.height() * (source_rect.y as f32 / h);
    let right = page_scene_rect.left()
        + page_scene_rect.width() * ((source_rect.x + source_rect.w) as f32 / w);
    let bottom = page_scene_rect.top()
        + page_scene_rect.height() * ((source_rect.y + source_rect.h) as f32 / h);
    let rect = Rect::from_min_max(egui::pos2(left, top), egui::pos2(right, bottom));
    rect.is_positive().then_some(rect)
}

fn clamp_scene_pos_to_rect(pos: Pos2, rect: Rect) -> Pos2 {
    egui::pos2(
        pos.x.clamp(rect.left(), rect.right()),
        pos.y.clamp(rect.top(), rect.bottom()),
    )
}

fn scene_pos_to_source_xy(
    scene_pos: Pos2,
    page_scene_rect: Rect,
    source_w: usize,
    source_h: usize,
) -> (i32, i32) {
    let u = ((scene_pos.x - page_scene_rect.left()) / page_scene_rect.width()).clamp(0.0, 1.0);
    let v = ((scene_pos.y - page_scene_rect.top()) / page_scene_rect.height()).clamp(0.0, 1.0);
    let x = (u * source_w as f32).round().clamp(0.0, source_w as f32) as i32;
    let y = (v * source_h as f32).round().clamp(0.0, source_h as f32) as i32;
    (x, y)
}

fn scene_pointer_to_image_px(
    scene_pos: Pos2,
    image_rect: Rect,
    image_size: [usize; 2],
) -> (i32, i32) {
    let image_w = image_size[0].max(1);
    let image_h = image_size[1].max(1);
    let rect_w = image_rect.width().max(f32::EPSILON);
    let rect_h = image_rect.height().max(f32::EPSILON);
    let x = ((scene_pos.x - image_rect.left()) / rect_w * image_w as f32)
        .round()
        .clamp(0.0, (image_w.saturating_sub(1)) as f32) as i32;
    let y = ((scene_pos.y - image_rect.top()) / rect_h * image_h as f32)
        .round()
        .clamp(0.0, (image_h.saturating_sub(1)) as f32) as i32;
    (x, y)
}

fn snap_selection_end(start: i32, end: i32, max: i32, multiple: usize) -> i32 {
    let delta = end - start;
    if delta == 0 {
        return end.clamp(0, max);
    }
    let snapped = ((delta.unsigned_abs() as usize).div_ceil(multiple) * multiple) as i32;
    let mut out = start + delta.signum() * snapped;
    out = out.clamp(0, max);
    if out == start {
        out = (start + delta.signum()).clamp(0, max);
    }
    out
}

fn expand_overlay_rect(rect: OverlayRectPx, w: usize, h: usize, margin: usize) -> OverlayRectPx {
    if w == 0 || h == 0 {
        return rect;
    }
    let x0 = rect.x.saturating_sub(margin);
    let y0 = rect.y.saturating_sub(margin);
    let x1 = rect.x.saturating_add(rect.w).saturating_add(margin).min(w);
    let y1 = rect.y.saturating_add(rect.h).saturating_add(margin).min(h);
    OverlayRectPx {
        x: x0,
        y: y0,
        w: x1.saturating_sub(x0).max(1),
        h: y1.saturating_sub(y0).max(1),
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
    let w = overlay_w as f32;
    let h = overlay_h as f32;
    let left = page_scene_rect.left() + page_scene_rect.width() * (rect.x as f32 / w);
    let top = page_scene_rect.top() + page_scene_rect.height() * (rect.y as f32 / h);
    let right = page_scene_rect.left() + page_scene_rect.width() * ((rect.x + rect.w) as f32 / w);
    let bottom = page_scene_rect.top() + page_scene_rect.height() * ((rect.y + rect.h) as f32 / h);
    let out = Rect::from_min_max(egui::pos2(left, top), egui::pos2(right, bottom));
    out.is_positive().then_some(out)
}

fn extract_overlay_chunk(image: &egui::ColorImage, rect: OverlayRectPx) -> egui::ColorImage {
    let mut out = egui::ColorImage::filled([rect.w, rect.h], Color32::TRANSPARENT);
    let src_w = image.size[0].max(1);
    let src_h = image.size[1].max(1);
    if rect.x >= src_w || rect.y >= src_h {
        return out;
    }
    let copy_w = rect.w.min(src_w.saturating_sub(rect.x));
    let copy_h = rect.h.min(src_h.saturating_sub(rect.y));
    for y in 0..copy_h {
        let sy = rect.y.saturating_add(y);
        let src_start = sy.saturating_mul(src_w).saturating_add(rect.x);
        let src_end = src_start.saturating_add(copy_w);
        let dst_start = y.saturating_mul(rect.w);
        let dst_end = dst_start.saturating_add(copy_w);
        if src_end <= image.pixels.len() && dst_end <= out.pixels.len() {
            out.pixels[dst_start..dst_end].copy_from_slice(&image.pixels[src_start..src_end]);
        }
    }
    out
}

fn mark_scratch_dirty_rect(stroke: &mut ScratchStroke, dirty_overlay: OverlayRectPx) {
    stroke.dirty_rect = Some(match stroke.dirty_rect {
        Some(existing) => union_overlay_rect(existing, dirty_overlay),
        None => dirty_overlay,
    });
    for tile_idx in scratch_tile_indices_for_overlay_rect(stroke, dirty_overlay) {
        stroke.dirty_tiles.insert(tile_idx);
    }
}

fn scratch_preview_texture_options(canvas: &CanvasView) -> TextureOptions {
    if canvas.pixel_sampling_nearest() {
        TextureOptions::NEAREST
    } else {
        CLEANING_PREVIEW_TEXTURE_OPTIONS
    }
}

fn ensure_scratch_tiles(
    stroke: &mut ScratchStroke,
    ctx: &egui::Context,
    texture_options: TextureOptions,
) {
    if !stroke.tiles.is_empty() {
        for (idx, tile) in stroke.tiles.iter().enumerate() {
            if tile.texture_options != texture_options {
                stroke.dirty_tiles.insert(idx);
            }
        }
        return;
    }
    let tiles_x = stroke.image.size[0].div_ceil(SCRATCH_PREVIEW_TILE_SIDE);
    let tiles_y = stroke.image.size[1].div_ceil(SCRATCH_PREVIEW_TILE_SIDE);
    stroke.tiles.reserve(tiles_x.saturating_mul(tiles_y));
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let origin_x = tx * SCRATCH_PREVIEW_TILE_SIDE;
            let origin_y = ty * SCRATCH_PREVIEW_TILE_SIDE;
            let tile_w = (stroke.image.size[0] - origin_x).min(SCRATCH_PREVIEW_TILE_SIDE);
            let tile_h = (stroke.image.size[1] - origin_y).min(SCRATCH_PREVIEW_TILE_SIDE);
            let tile_img =
                build_local_tile_image(&stroke.image, origin_x, origin_y, tile_w, tile_h);
            let texture = ctx.load_texture(
                format!(
                    "cleaning-scratch-preview-{}-{}-{}",
                    stroke.page_idx, origin_x, origin_y
                ),
                tile_img,
                texture_options,
            );
            stroke.tiles.push(ScratchTextureTile {
                texture,
                texture_options,
                origin_px: [origin_x, origin_y],
                size_px: [tile_w, tile_h],
            });
        }
    }
}

fn upload_dirty_scratch_tiles(stroke: &mut ScratchStroke, texture_options: TextureOptions) {
    if stroke.dirty_tiles.is_empty() {
        return;
    }
    let dirty_tiles = std::mem::take(&mut stroke.dirty_tiles);
    for tile_idx in dirty_tiles {
        let Some(tile) = stroke.tiles.get_mut(tile_idx) else {
            continue;
        };
        let tile_img = build_local_tile_image(
            &stroke.image,
            tile.origin_px[0],
            tile.origin_px[1],
            tile.size_px[0],
            tile.size_px[1],
        );
        tile.texture.set(tile_img, texture_options);
        tile.texture_options = texture_options;
    }
}

fn paint_scratch_tiles(stroke: &ScratchStroke, ui: &mut egui::Ui) {
    let src_w = stroke.image.size[0] as f32;
    let src_h = stroke.image.size[1] as f32;
    if src_w <= 0.0 || src_h <= 0.0 {
        return;
    }
    for tile in &stroke.tiles {
        let ox = tile.origin_px[0] as f32;
        let oy = tile.origin_px[1] as f32;
        let tw = tile.size_px[0] as f32;
        let th = tile.size_px[1] as f32;
        let dst = Rect::from_min_size(
            egui::pos2(
                stroke.scene_rect.left() + stroke.scene_rect.width() * (ox / src_w),
                stroke.scene_rect.top() + stroke.scene_rect.height() * (oy / src_h),
            ),
            egui::vec2(
                stroke.scene_rect.width() * (tw / src_w),
                stroke.scene_rect.height() * (th / src_h),
            ),
        );
        ui.painter().image(
            tile.texture.id(),
            dst,
            Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
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

fn scratch_tile_indices_for_overlay_rect(
    stroke: &ScratchStroke,
    dirty_overlay: OverlayRectPx,
) -> Vec<usize> {
    let Some(local_x0) = dirty_overlay.x.checked_sub(stroke.overlay_rect.x) else {
        return Vec::new();
    };
    let Some(local_y0) = dirty_overlay.y.checked_sub(stroke.overlay_rect.y) else {
        return Vec::new();
    };
    let local_x1 = local_x0
        .saturating_add(dirty_overlay.w)
        .min(stroke.overlay_rect.w)
        .saturating_sub(1);
    let local_y1 = local_y0
        .saturating_add(dirty_overlay.h)
        .min(stroke.overlay_rect.h)
        .saturating_sub(1);
    if local_x0 > local_x1 || local_y0 > local_y1 {
        return Vec::new();
    }
    let tiles_x = stroke.image.size[0].div_ceil(SCRATCH_PREVIEW_TILE_SIDE);
    let tile_x0 = local_x0 / SCRATCH_PREVIEW_TILE_SIDE;
    let tile_y0 = local_y0 / SCRATCH_PREVIEW_TILE_SIDE;
    let tile_x1 = local_x1 / SCRATCH_PREVIEW_TILE_SIDE;
    let tile_y1 = local_y1 / SCRATCH_PREVIEW_TILE_SIDE;
    let mut out = Vec::with_capacity((tile_x1 - tile_x0 + 1) * (tile_y1 - tile_y0 + 1));
    for ty in tile_y0..=tile_y1 {
        for tx in tile_x0..=tile_x1 {
            out.push(ty * tiles_x + tx);
        }
    }
    out
}

fn union_overlay_rect(a: OverlayRectPx, b: OverlayRectPx) -> OverlayRectPx {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = a.x.saturating_add(a.w).max(b.x.saturating_add(b.w));
    let y1 = a.y.saturating_add(a.h).max(b.y.saturating_add(b.h));
    OverlayRectPx {
        x: x0,
        y: y0,
        w: x1.saturating_sub(x0),
        h: y1.saturating_sub(y0),
    }
}

// All parameters are independent brush stroke properties; grouping would obscure painting intent.
#[allow(clippy::too_many_arguments)]
fn paint_line_mask_with_hardness(
    dst: &mut [f32],
    image_size: [usize; 2],
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: i32,
    hardness: f32,
    strict_pixel_painting: bool,
) {
    let r = radius.max(1);
    let dx = x1 - x0;
    let dy = y1 - y0;
    let distance = (dx * dx + dy * dy).sqrt();
    if distance <= f32::EPSILON {
        paint_circle_mask_with_hardness(
            dst,
            image_size,
            x0,
            y0,
            r,
            hardness,
            strict_pixel_painting,
        );
        return;
    }
    let step = (r as f32 * 0.45).max(1.0);
    let stamps = (distance / step).ceil() as usize;
    let mut last = (f32::NAN, f32::NAN);
    for i in 0..=stamps {
        let t = i as f32 / stamps.max(1) as f32;
        let sx = x0 + dx * t;
        let sy = y0 + dy * t;
        if strict_pixel_painting && (sx.round(), sy.round()) == last {
            continue;
        }
        paint_circle_mask_with_hardness(
            dst,
            image_size,
            sx,
            sy,
            r,
            hardness,
            strict_pixel_painting,
        );
        last = (sx.round(), sy.round());
    }
}

fn paint_circle_mask_with_hardness(
    dst: &mut [f32],
    image_size: [usize; 2],
    cx: f32,
    cy: f32,
    radius: i32,
    hardness: f32,
    strict_pixel_painting: bool,
) {
    let r = radius.max(1);
    let w_usize = image_size[0];
    let w = w_usize as i32;
    let h = image_size[1] as i32;
    let sample_margin = if strict_pixel_painting { 0.0 } else { 0.5 };
    let x0 = (cx - r as f32 - sample_margin).floor().max(0.0) as i32;
    let x1 = (cx + r as f32 + sample_margin).ceil().min((w - 1) as f32) as i32;
    let y0 = (cy - r as f32 - sample_margin).floor().max(0.0) as i32;
    let y1 = (cy + r as f32 + sample_margin).ceil().min((h - 1) as f32) as i32;
    let radius_f = r as f32;
    let hardness = hardness.clamp(0.0, 1.0);
    let hard_radius = (radius_f * hardness).clamp(0.0, radius_f);
    let falloff = BrushFalloff {
        radius: radius_f,
        hard_radius,
        soft_span: (radius_f - hard_radius).max(f32::EPSILON),
        hardness,
    };

    for y in y0..=y1 {
        let row_off = (y as usize).saturating_mul(w_usize);
        for x in x0..=x1 {
            let strength = if strict_pixel_painting {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                brush_sample_strength((dx * dx + dy * dy).sqrt(), falloff)
            } else {
                brush_pixel_coverage_strength(x, y, cx, cy, falloff)
            };
            if strength <= f32::EPSILON {
                continue;
            }
            let idx = row_off.saturating_add(x as usize);
            if let Some(mask_px) = dst.get_mut(idx) {
                *mask_px = mask_px.max(strength);
            }
        }
    }
}

fn brush_pixel_coverage_strength(x: i32, y: i32, cx: f32, cy: f32, falloff: BrushFalloff) -> f32 {
    const SAMPLE_OFFSETS: [f32; 4] = [-0.375, -0.125, 0.125, 0.375];
    let mut sum = 0.0;
    for oy in SAMPLE_OFFSETS {
        for ox in SAMPLE_OFFSETS {
            let dx = x as f32 + ox - cx;
            let dy = y as f32 + oy - cy;
            sum += brush_sample_strength((dx * dx + dy * dy).sqrt(), falloff);
        }
    }
    sum / 16.0
}

fn brush_sample_strength(dist: f32, falloff: BrushFalloff) -> f32 {
    if dist > falloff.radius {
        return 0.0;
    }
    if dist <= falloff.hard_radius || falloff.hardness >= 1.0 {
        return 1.0;
    }
    let t = ((dist - falloff.hard_radius) / falloff.soft_span).clamp(0.0, 1.0);
    1.0 - smoothstep(t)
}

fn apply_cleaning_brush_stroke(
    dst: &mut egui::ColorImage,
    base: &egui::ColorImage,
    mask: &[f32],
    color: Color32,
    erase: bool,
    blend_colors: bool,
    local_rect: Option<OverlayRectPx>,
) {
    if dst.size != base.size || dst.pixels.len() != mask.len() || base.pixels.len() != mask.len() {
        return;
    }
    let target = local_rect.unwrap_or(OverlayRectPx {
        x: 0,
        y: 0,
        w: dst.size[0],
        h: dst.size[1],
    });
    let max_x = target.x.saturating_add(target.w).min(dst.size[0]);
    let max_y = target.y.saturating_add(target.h).min(dst.size[1]);
    for y in target.y..max_y {
        let row_off = y.saturating_mul(dst.size[0]);
        for x in target.x..max_x {
            let idx = row_off.saturating_add(x);
            let Some(strength) = mask.get(idx).copied() else {
                continue;
            };
            let Some(base_px) = base.pixels.get(idx).copied() else {
                continue;
            };
            let Some(dst_px) = dst.pixels.get_mut(idx) else {
                continue;
            };
            if strength <= f32::EPSILON {
                *dst_px = base_px;
                continue;
            }
            let mut pixel = base_px;
            apply_cleaning_brush_pixel(&mut pixel, color, strength, erase, blend_colors);
            *dst_px = pixel;
        }
    }
}

fn apply_cleaning_brush_pixel(
    dst: &mut Color32,
    src: Color32,
    strength: f32,
    erase: bool,
    blend_colors: bool,
) {
    let pressure = strength.clamp(0.0, 1.0);
    if pressure <= f32::EPSILON {
        return;
    }

    if erase {
        erase_cleaning_brush_pixel(dst, pressure);
        return;
    }

    if !blend_colors {
        let [src_r, src_g, src_b, src_a] = src.to_srgba_unmultiplied();
        let out_alpha = ((src_a as f32 / 255.0) * pressure * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8;
        *dst = Color32::from_rgba_unmultiplied(src_r, src_g, src_b, out_alpha);
        return;
    }

    let [src_r, src_g, src_b, src_a] = src.to_srgba_unmultiplied();
    let [dst_r, dst_g, dst_b, dst_a] = dst.to_srgba_unmultiplied();
    let brush_alpha = src_a as f32 / 255.0;
    let src_alpha = brush_alpha * pressure;
    if src_alpha <= f32::EPSILON {
        return;
    }
    let dst_alpha = dst_a as f32 / 255.0;
    let src_r = src_r as f32 / 255.0;
    let src_g = src_g as f32 / 255.0;
    let src_b = src_b as f32 / 255.0;
    let dst_r = dst_r as f32 / 255.0;
    let dst_g = dst_g as f32 / 255.0;
    let dst_b = dst_b as f32 / 255.0;

    let raw_alpha = src_alpha + dst_alpha * (1.0 - src_alpha);
    let mut premul_r = src_r * src_alpha + dst_r * dst_alpha * (1.0 - src_alpha);
    let mut premul_g = src_g * src_alpha + dst_g * dst_alpha * (1.0 - src_alpha);
    let mut premul_b = src_b * src_alpha + dst_b * dst_alpha * (1.0 - src_alpha);

    let capped_alpha = raw_alpha.min(dst_alpha.max(brush_alpha));
    if raw_alpha > f32::EPSILON && capped_alpha < raw_alpha {
        let scale = capped_alpha / raw_alpha;
        premul_r *= scale;
        premul_g *= scale;
        premul_b *= scale;
    }
    let out_alpha = capped_alpha;
    if out_alpha <= f32::EPSILON {
        *dst = Color32::TRANSPARENT;
        return;
    }

    let out_r = (premul_r / out_alpha).clamp(0.0, 1.0);
    let out_g = (premul_g / out_alpha).clamp(0.0, 1.0);
    let out_b = (premul_b / out_alpha).clamp(0.0, 1.0);
    *dst = Color32::from_rgba_unmultiplied(
        (out_r * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_g * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_b * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_alpha * 255.0).round().clamp(0.0, 255.0) as u8,
    );
}

fn erase_cleaning_brush_pixel(dst: &mut Color32, pressure: f32) {
    let [dst_r, dst_g, dst_b, dst_a] = dst.to_srgba_unmultiplied();
    let dst_alpha = dst_a as f32 / 255.0;
    if dst_alpha <= f32::EPSILON {
        return;
    }
    let keep = 1.0 - pressure.clamp(0.0, 1.0);
    *dst = Color32::from_rgba_unmultiplied(
        dst_r,
        dst_g,
        dst_b,
        (dst_alpha * keep * 255.0).round().clamp(0.0, 255.0) as u8,
    );
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn overlay_rect_to_local_rect(
    source_overlay_rect: OverlayRectPx,
    desired_overlay_rect: OverlayRectPx,
) -> Option<OverlayRectPx> {
    let x = desired_overlay_rect.x.checked_sub(source_overlay_rect.x)?;
    let y = desired_overlay_rect.y.checked_sub(source_overlay_rect.y)?;
    Some(OverlayRectPx {
        x,
        y,
        w: desired_overlay_rect
            .w
            .min(source_overlay_rect.w.saturating_sub(x)),
        h: desired_overlay_rect
            .h
            .min(source_overlay_rect.h.saturating_sub(y)),
    })
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

fn build_local_tile_image(
    source: &egui::ColorImage,
    origin_x: usize,
    origin_y: usize,
    width: usize,
    height: usize,
) -> egui::ColorImage {
    let mut out = egui::ColorImage::filled([width, height], Color32::TRANSPARENT);
    for y in 0..height {
        let src_y = origin_y.saturating_add(y);
        let src_row = src_y.saturating_mul(source.size[0]);
        let dst_row = y.saturating_mul(width);
        for x in 0..width {
            let src_x = origin_x.saturating_add(x);
            let src_idx = src_row.saturating_add(src_x);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_pixel_coverage_reaches_partially_covered_neighbor() {
        let mut strict = vec![0.0; 9];
        paint_line_mask_with_hardness(&mut strict, [3, 3], 1.0, 1.0, 1.0, 1.0, 1, 1.0, true);

        let mut covered = vec![0.0; 9];
        paint_line_mask_with_hardness(&mut covered, [3, 3], 1.49, 1.49, 1.49, 1.49, 1, 1.0, false);

        assert_eq!(strict[8], 0.0);
        assert!(covered[8] > 0.0);
        assert!(covered[8] < 1.0);
    }

    #[test]
    fn soft_pixel_coverage_preserves_full_center_strength() {
        let mut mask = vec![0.0; 9];
        paint_line_mask_with_hardness(&mut mask, [3, 3], 1.0, 1.0, 1.0, 1.0, 2, 1.0, false);

        assert!((mask[4] - 1.0).abs() <= f32::EPSILON);
    }
}
