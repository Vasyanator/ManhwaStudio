/*
FILE HEADER (cleaning/tools/region_edit_test.rs)
- Назначение: тестовый инструмент поверх `RegionMaskInpaintToolBase` для проверки mask-inpaint пайплайна.
- Ключевые сущности:
  - `RegionPaintTestTool`: состояние инструмента (база region inpaint + routing ввода).
  - `run(...)`: тестовая обработка; заливает красным пиксели под маской.
- Поведение:
  - Shift+ЛКМ по canvas создаёт выделение (через `RegionMaskInpaintToolBase`).
  - В окне: ЛКМ рисует маску, ПКМ/Shift+ЛКМ стирает маску, `Обработать` запускает `run`.
  - `run` в тесте не делает ML: просто красит область под маской в красный цвет.
*/
use super::base::{CleaningTool, RegionMaskInpaintToolBase, StrokePoint};
use crate::canvas::CanvasView;
use crate::project::ProjectData;
use eframe::egui;
use egui::Color32;

pub struct RegionPaintTestTool {
    inpaint_base: RegionMaskInpaintToolBase,
}

impl Default for RegionPaintTestTool {
    fn default() -> Self {
        Self {
            inpaint_base: RegionMaskInpaintToolBase::new("region_inpaint_test", Some(8)),
        }
    }
}

impl RegionPaintTestTool {
    fn run(image: &egui::ColorImage, mask: &egui::ColorImage) -> Result<egui::ColorImage, String> {
        if image.size != mask.size {
            return Err("Размер изображения и маски не совпадает.".to_string());
        }
        let mut out = image.clone();
        for (dst, m) in out.pixels.iter_mut().zip(mask.pixels.iter()) {
            if m.a() > 0 {
                *dst = Color32::from_rgba_unmultiplied(255, 0, 0, 255);
            }
        }
        Ok(out)
    }
}

impl CleaningTool for RegionPaintTestTool {
    fn tool_id(&self) -> &'static str {
        "region_paint_test"
    }

    fn title(&self) -> &'static str {
        "Тест: mask inpaint"
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.inpaint_base.cancel_selection();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.inpaint_base.draw_ui_hint(ui);
        ui.small("Тестовый run: «Обработать» заполняет красным пиксели под маской.");
        ui.small("Zoom в окне: Ctrl/Z + колесо, Ctrl/Z + ЛКМ drag, Ctrl/Z + -/=, Ctrl/Z + 0.");
    }

    fn on_key_event(&mut self, ctx: &egui::Context) -> bool {
        self.inpaint_base.on_key_event(ctx)
    }

    fn on_wheel_event(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        self.inpaint_base.on_wheel_event(delta_y, modifiers)
    }

    fn set_space_pan_active(&mut self, active: bool) {
        self.inpaint_base.set_space_pan_active(active);
    }

    fn set_ai_backend_available(&mut self, available: bool) {
        self.inpaint_base.set_ai_backend_available(available);
    }

    fn set_ai_backend_torch_available(&mut self, available: bool) {
        self.inpaint_base.set_ai_backend_torch_available(available);
    }

    fn wants_primary_stroke(&self, point: StrokePoint) -> bool {
        self.inpaint_base.wants_primary_stroke(point)
    }

    fn stroke_begin(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        self.inpaint_base.begin_selection(canvas, point);
    }

    fn stroke_update(&mut self, canvas: &mut CanvasView, _from: StrokePoint, to: StrokePoint) {
        self.inpaint_base.update_selection(canvas, to);
    }

    fn stroke_end(&mut self, canvas: &mut CanvasView) {
        self.inpaint_base.end_selection(canvas);
    }

    fn draw_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        self.inpaint_base.draw_overlay_ui(
            ctx,
            canvas,
            project,
            "Тестовый mask-inpaint редактор",
            Self::run,
        );
    }

    fn draw_cursor(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<egui::Pos2>,
    ) {
        self.inpaint_base.draw_cursor(ui, canvas, pointer_scene_pos);
    }

    fn captures_canvas_pointer(&self, pointer_pos: egui::Pos2) -> bool {
        self.inpaint_base.editor_window_contains(pointer_pos)
    }

    fn block_canvas_zoom(&self) -> bool {
        self.inpaint_base.has_open_editor()
    }
}
