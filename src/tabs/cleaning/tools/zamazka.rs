/*
FILE HEADER (cleaning/tools/zamazka.rs)
- Назначение: основной инструмент клининга "Замазка" для рисования в clean-overlay.
- Ключевые сущности:
  - `ZamazkaTool`: состояние кисти/режима, прямоугольного выделения и кэша базовых страниц.
  - `ZamazkaMode`: режимы `Brush` / `Eraser` / `Eyedropper` / `Rect`.
- Поведение:
  - Кисть/ластик рисуют через локальный scratch-буфер; preview показывается tiled scratch-overlay,
    а commit в shared-модель делается только на `stroke_end`; по умолчанию кисть использует
    subpixel coverage, а строгий пиксельный режим оставлен как опция инструмента.
  - Пипетка сначала берёт цвет из overlay, затем fallback на базовую страницу (подгрузка в фоне).
  - Для пипетки base-страница под курсором предварительно подгружается в фоне на каждом кадре,
    чтобы цвет можно было взять до первого рисования на странице.
  - Прямоугольный режим формирует scene-rect и вставляет chunk через `replace_overlay_region`.
  - Для `Ctrl+ЛКМ` (временный прямоугольник) и `Ctrl+Shift+ЛКМ` (временное прямоугольное
    стирание) инструмент блокирует zoom CanvasView на эту комбинацию, чтобы не конфликтовать с
    zoom-drag.
- Потоки:
  - Отдельный worker загружает base-изображения страниц для пипетки, чтобы не блокировать GUI.
*/
use super::base::{BrushToolBase, CleaningCursorOccluder, CleaningTool, StrokePoint};
use crate::canvas::CanvasView;
use crate::project::ProjectData;
use crate::widgets::{WheelComboBox, WheelSlider};
use eframe::egui;
use egui::color_picker::{self, Alpha};
use egui::{Color32, Rect};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use ms_thread as thread;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ZamazkaMode {
    Brush,
    Eraser,
    Eyedropper,
    Rect,
}

impl ZamazkaMode {
    fn title(self) -> &'static str {
        match self {
            Self::Brush => "Кисть",
            Self::Eraser => "Ластик",
            Self::Eyedropper => "Пипетка",
            Self::Rect => "Прямоуг.",
        }
    }
}

pub struct ZamazkaTool {
    brush_base: BrushToolBase,
    color: Color32,
    mode: ZamazkaMode,
    temporary_erase: bool,
    rect_erase: bool,
    rect_stroke_erase: bool,
    rect_start: Option<StrokePoint>,
    rect_current: Option<StrokePoint>,
    touched_pages: HashSet<usize>,
    base_images: HashMap<usize, egui::ColorImage>,
    base_load_pending: HashSet<usize>,
    base_load_tx: Sender<(usize, PathBuf)>,
    base_load_rx: Receiver<(usize, Option<egui::ColorImage>)>,
}

impl Default for ZamazkaTool {
    fn default() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<(usize, PathBuf)>();
        let (result_tx, result_rx) = mpsc::channel::<(usize, Option<egui::ColorImage>)>();
        thread::spawn(move || {
            while let Ok((page_idx, path)) = request_rx.recv() {
                let loaded = image::open(&path).ok().map(|img| {
                    let rgba = img.to_rgba8();
                    egui::ColorImage::from_rgba_unmultiplied(
                        [rgba.width() as usize, rgba.height() as usize],
                        rgba.as_raw(),
                    )
                });
                let _ = result_tx.send((page_idx, loaded));
            }
        });
        Self {
            brush_base: BrushToolBase::default(),
            color: Color32::from_rgba_unmultiplied(255, 0, 0, 255),
            mode: ZamazkaMode::Brush,
            temporary_erase: false,
            rect_erase: false,
            rect_stroke_erase: false,
            rect_start: None,
            rect_current: None,
            touched_pages: HashSet::new(),
            base_images: HashMap::new(),
            base_load_pending: HashSet::new(),
            base_load_tx: request_tx,
            base_load_rx: result_rx,
        }
    }
}

impl ZamazkaTool {
    fn poll_base_images(&mut self) {
        loop {
            match self.base_load_rx.try_recv() {
                Ok((page_idx, maybe_img)) => {
                    self.base_load_pending.remove(&page_idx);
                    if let Some(img) = maybe_img {
                        self.base_images.insert(page_idx, img);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn queue_base_image_load_if_needed(&mut self, page_idx: usize, path: PathBuf) {
        if self.base_images.contains_key(&page_idx) || self.base_load_pending.contains(&page_idx) {
            return;
        }
        if self.base_load_tx.send((page_idx, path)).is_ok() {
            self.base_load_pending.insert(page_idx);
        }
    }

    fn sample_overlay_color(&self, canvas: &CanvasView, point: StrokePoint) -> Option<Color32> {
        let (x, y) = canvas.scene_point_to_overlay_xy(point.page_idx, point.scene_pos)?;
        let overlay = canvas.overlay_image(point.page_idx)?;
        let width = overlay.size[0];
        let idx = y.saturating_mul(width).saturating_add(x);
        let px = *overlay.pixels.get(idx)?;
        (px.a() > 0).then_some(px)
    }

    fn sample_base_color_from_cache(
        &mut self,
        canvas: &CanvasView,
        point: StrokePoint,
    ) -> Option<Color32> {
        let page_rect = canvas.page_scene_rect(point.page_idx)?;
        if page_rect.width() <= f32::EPSILON || page_rect.height() <= f32::EPSILON {
            return None;
        }
        let u = ((point.scene_pos.x - page_rect.left()) / page_rect.width()).clamp(0.0, 1.0);
        let v = ((point.scene_pos.y - page_rect.top()) / page_rect.height()).clamp(0.0, 1.0);
        self.poll_base_images();

        if let Some(base) = self.base_images.get(&point.page_idx) {
            let bw = base.size[0];
            let bh = base.size[1];
            if bw == 0 || bh == 0 {
                return None;
            }
            let bx = (u * bw as f32)
                .round()
                .clamp(0.0, (bw.saturating_sub(1)) as f32) as usize;
            let by = (v * bh as f32)
                .round()
                .clamp(0.0, (bh.saturating_sub(1)) as f32) as usize;
            let idx = by.saturating_mul(bw).saturating_add(bx);
            return base.pixels.get(idx).copied();
        }

        None
    }

    fn queue_base_for_page_if_needed(&mut self, project: &ProjectData, page_idx: usize) {
        if let Some(page) = project.pages.iter().find(|p| p.idx == page_idx) {
            self.queue_base_image_load_if_needed(page_idx, page.path.clone());
        }
    }

    fn effective_erase(&self, point: StrokePoint) -> bool {
        point.modifiers.shift || self.temporary_erase || self.mode == ZamazkaMode::Eraser
    }

    fn paint_segment(&self, canvas: &mut CanvasView, from: StrokePoint, to: StrokePoint) {
        if self.brush_base.should_ignore_drawing() {
            return;
        }
        let _ = self.brush_base.paint_overlay_segment(
            canvas,
            from,
            to,
            self.color,
            self.effective_erase(from),
        );
    }

    fn sample_color(&mut self, canvas: &CanvasView, point: StrokePoint) {
        if let Some(px) = self.sample_overlay_color(canvas, point) {
            self.color = px;
            return;
        }
        if let Some(px) = self.sample_base_color_from_cache(canvas, point) {
            self.color = px;
        }
    }

    fn sample_color_with_base(
        &mut self,
        canvas: &CanvasView,
        project: &ProjectData,
        point: StrokePoint,
    ) {
        if let Some(px) = self.sample_overlay_color(canvas, point) {
            self.color = px;
            return;
        }
        if let Some(px) = self.sample_base_color_from_cache(canvas, point) {
            self.color = px;
            return;
        }
        self.queue_base_for_page_if_needed(project, point.page_idx);
    }

    fn prefetch_base_under_pointer(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
    ) {
        self.poll_base_images();
        let pointer_scene_pos = ctx.input(|i| i.pointer.hover_pos());
        let Some(pointer_scene_pos) = pointer_scene_pos else {
            return;
        };
        let Some(page_idx) = canvas.page_index_at_scene_pos(pointer_scene_pos) else {
            return;
        };
        self.queue_base_for_page_if_needed(project, page_idx);
    }

    fn rect_scene(start: StrokePoint, end: StrokePoint) -> Option<Rect> {
        if start.page_idx != end.page_idx {
            return None;
        }
        let rect = Rect::from_two_pos(start.scene_pos, end.scene_pos);
        if rect.is_positive() { Some(rect) } else { None }
    }

    fn commit_rect(&mut self, canvas: &mut CanvasView) {
        let (Some(start), Some(end)) = (self.rect_start, self.rect_current) else {
            return;
        };
        let Some(scene_rect) = Self::rect_scene(start, end) else {
            return;
        };
        let Some(target) = canvas.scene_rect_to_overlay_rect(start.page_idx, scene_rect) else {
            return;
        };
        if target.w == 0 || target.h == 0 {
            return;
        }
        let fill = if self.rect_stroke_erase {
            Color32::TRANSPARENT
        } else {
            self.color
        };
        let chunk = egui::ColorImage::filled([target.w, target.h], fill);
        let _ = canvas.replace_overlay_region(start.page_idx, scene_rect, &chunk);
    }

    fn draw_color_cross(
        &self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<egui::Pos2>,
    ) {
        let Some(pointer_scene_pos) = pointer_scene_pos else {
            return;
        };
        let Some(page_idx) = canvas.page_index_at_scene_pos(pointer_scene_pos) else {
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
        let radius_scene = ((radius_x_scene + radius_y_scene) * 0.5).max(6.0);
        let half = (radius_scene * 0.45).max(6.0);

        ui.painter().line_segment(
            [
                egui::pos2(pointer_scene_pos.x - half, pointer_scene_pos.y),
                egui::pos2(pointer_scene_pos.x + half, pointer_scene_pos.y),
            ],
            egui::Stroke::new(1.5, self.color),
        );
        ui.painter().line_segment(
            [
                egui::pos2(pointer_scene_pos.x, pointer_scene_pos.y - half),
                egui::pos2(pointer_scene_pos.x, pointer_scene_pos.y + half),
            ],
            egui::Stroke::new(1.5, self.color),
        );
    }
}

impl CleaningTool for ZamazkaTool {
    fn tool_id(&self) -> &'static str {
        "zamazka"
    }

    fn title(&self) -> &'static str {
        "Замазка"
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.brush_base.set_space_pan_active(false);
        self.brush_base.cancel_scratch_stroke();
        self.rect_stroke_erase = false;
        self.rect_start = None;
        self.rect_current = None;
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        ui.label("Режим");
        WheelComboBox::from_id_salt("cleaning_zamazka_mode")
            .selected_text(self.mode.title())
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut self.mode,
                    ZamazkaMode::Brush,
                    ZamazkaMode::Brush.title(),
                );
                ui.selectable_value(
                    &mut self.mode,
                    ZamazkaMode::Eraser,
                    ZamazkaMode::Eraser.title(),
                );
                ui.selectable_value(
                    &mut self.mode,
                    ZamazkaMode::Eyedropper,
                    ZamazkaMode::Eyedropper.title(),
                );
                ui.selectable_value(&mut self.mode, ZamazkaMode::Rect, ZamazkaMode::Rect.title());
            });

        let mut radius = self.brush_base.radius_px();
        if ui
            .add(WheelSlider::new(&mut radius, 1..=200).text("Размер"))
            .changed()
        {
            self.brush_base.set_radius_px(radius);
        }

        let mut hardness = self.brush_base.hardness();
        let _ = ui.add(
            WheelSlider::new(&mut hardness, 0.0..=1.0)
                .text("Жесткость")
                .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)),
        );
        let _ = self.brush_base.set_hardness(hardness);
        let mut blend_colors = self.brush_base.blend_colors();
        if ui
            .checkbox(&mut blend_colors, "Смешивание цветов")
            .changed()
        {
            let _ = self.brush_base.set_blend_colors(blend_colors);
        }
        let mut strict_pixel_painting = self.brush_base.strict_pixel_painting();
        if ui
            .checkbox(&mut strict_pixel_painting, "Рисовать строго по пикселям")
            .changed()
        {
            let _ = self
                .brush_base
                .set_strict_pixel_painting(strict_pixel_painting);
        }

        ui.horizontal(|ui| {
            ui.label("Цвет");
            color_picker::color_edit_button_srgba(ui, &mut self.color, Alpha::OnlyBlend);
        });
        let mut alpha = self.color.a();
        if ui
            .add(
                WheelSlider::new(&mut alpha, 0..=255)
                    .text("Прозрачность")
                    .custom_formatter(|value, _| format!("{:.0}%", value / 255.0 * 100.0)),
            )
            .changed()
        {
            let [r, g, b, _] = self.color.to_srgba_unmultiplied();
            self.color = Color32::from_rgba_unmultiplied(r, g, b, alpha);
        }

        if self.mode == ZamazkaMode::Rect {
            ui.checkbox(&mut self.rect_erase, "Прямоуг.: стирать");
        }

        ui.separator();
        ui.label("Горячие клавиши");
        ui.label("ПКМ: взять цвет");
        ui.label("Shift + ЛКМ: временный ластик");
        ui.label("Ctrl + ЛКМ: прямоугольник");
        ui.label("Ctrl + Shift + ЛКМ: стереть прямоугольник");
        ui.label("Shift + колесо: размер кисти");
        ui.label("- / =: размер кисти");
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

    fn secondary_click(
        &mut self,
        canvas: &mut CanvasView,
        project: &ProjectData,
        point: StrokePoint,
    ) -> bool {
        if self.brush_base.should_ignore_drawing() {
            return false;
        }
        self.sample_color_with_base(canvas, project, point);
        true
    }

    fn draw_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        self.prefetch_base_under_pointer(ctx, canvas, project);
    }

    fn draw_cursor(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<egui::Pos2>,
    ) {
        if ui.ctx().is_popup_open() {
            ui.ctx()
                .output_mut(|out| out.cursor_icon = egui::CursorIcon::Default);
            return;
        }
        self.brush_base.draw_scratch_preview(ui, canvas);
        if self.brush_base.should_ignore_drawing() {
            return;
        }
        if self.mode != ZamazkaMode::Rect {
            self.brush_base
                .draw_circle_cursor(ui, canvas, pointer_scene_pos);
            self.draw_color_cross(ui, canvas, pointer_scene_pos);
        }

        if let (Some(start), Some(current)) = (self.rect_start, self.rect_current)
            && let Some(rect) = Self::rect_scene(start, current)
        {
            ui.painter().rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(1.0, Color32::from_gray(160)),
                egui::StrokeKind::Outside,
            );
        }
    }

    fn ensure_hover_overlay(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        if self.brush_base.should_ignore_drawing() || self.mode == ZamazkaMode::Rect {
            return;
        }
        let _ = BrushToolBase::ensure_overlay_under_point(canvas, point);
    }

    fn bubble_occluder(
        &self,
        canvas: &CanvasView,
        pointer_scene_pos: Option<egui::Pos2>,
    ) -> Option<CleaningCursorOccluder> {
        if self.brush_base.should_ignore_drawing() || self.mode == ZamazkaMode::Rect {
            return None;
        }
        self.brush_base.bubble_occluder(canvas, pointer_scene_pos)
    }

    fn stroke_begin(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        if self.brush_base.should_ignore_drawing() {
            return;
        }
        if point.modifiers.ctrl || self.mode == ZamazkaMode::Rect {
            self.rect_stroke_erase = point.modifiers.ctrl && point.modifiers.shift
                || self.mode == ZamazkaMode::Rect && self.rect_erase;
            self.rect_start = Some(point);
            self.rect_current = Some(point);
            return;
        }
        if self.mode == ZamazkaMode::Eyedropper {
            self.sample_color(canvas, point);
            return;
        }
        if self.brush_base.begin_scratch_stroke(canvas, point) {
            let painted = self.brush_base.paint_scratch_segment(
                canvas,
                point,
                point,
                self.color,
                self.effective_erase(point),
            );
            if !painted {
                let _ = self.brush_base.commit_scratch_stroke(canvas);
                self.touched_pages.insert(point.page_idx);
                self.paint_segment(canvas, point, point);
            }
        } else {
            self.touched_pages.insert(point.page_idx);
            self.paint_segment(canvas, point, point);
        }
    }

    fn stroke_update(&mut self, canvas: &mut CanvasView, from: StrokePoint, to: StrokePoint) {
        if self.brush_base.should_ignore_drawing() {
            return;
        }
        if self.rect_start.is_some() {
            self.rect_current = Some(to);
            return;
        }
        if self.mode == ZamazkaMode::Eyedropper {
            self.sample_color(canvas, to);
            return;
        }
        if self.brush_base.scratch_active() {
            if self.brush_base.paint_scratch_segment(
                canvas,
                from,
                to,
                self.color,
                self.effective_erase(from),
            ) {
                return;
            }
            if self.brush_base.commit_scratch_stroke(canvas).is_none() {
                self.brush_base.cancel_scratch_stroke();
            }
        }
        self.touched_pages.insert(to.page_idx);
        self.paint_segment(canvas, from, to);
    }

    fn stroke_end(&mut self, canvas: &mut CanvasView) {
        if self.rect_start.is_some() {
            self.commit_rect(canvas);
            self.rect_stroke_erase = false;
            self.rect_start = None;
            self.rect_current = None;
        }
        let _ = self.brush_base.commit_scratch_stroke(canvas);
        for idx in self.touched_pages.drain() {
            let _ = canvas.commit_overlay_page_to_model(idx);
        }
    }

    fn set_temporary_erase(&mut self, erase: bool) {
        self.temporary_erase = erase;
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
        self.brush_base.scratch_active()
    }
}
