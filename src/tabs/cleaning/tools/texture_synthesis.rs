/*
FILE HEADER (cleaning/tools/texture_synthesis.rs)
- Назначение: инструмент inpaint под маской на базе библиотеки `texture-synthesis`.
- Ключевые сущности:
  - `TextureSynthesisInpaintTool`: wiring инструмента (выделение, UI, routing ввода).
  - `TextureSynthesisParams`: параметры сессии `texture-synthesis`.
  - `run_with_params(...)`: конвертация region-image/маски и запуск inpaint-сессии.
- Поведение:
  - Shift+ЛКМ по canvas открывает region mask editor (через `RegionMaskInpaintToolBase`).
  - В окне можно переключать режим рисования между маской удаления и маской примера
    (для области, откуда можно брать текстуру).
  - Кнопка `Обработать` запускает texture-synthesis inpaint внутри выделенного региона.
  - Параметры синтеза настраиваются прямо в окне region editor в сворачиваемом блоке.
- Важно:
  - Маска для `texture-synthesis` строится как: белый = сохранить пиксель, чёрный = дорисовать.
  - Маска примера (зелёная) ограничивает зону сэмплинга; если она пустая, используется
    старый fallback: всё вне дыры доступно для сэмплинга.
*/
use super::base::{CleaningTool, RegionEditToolBase, RegionMaskInpaintToolBase, StrokePoint};
use crate::canvas::CanvasView;
use crate::project::ProjectData;
use crate::widgets::WheelSlider;
use eframe::egui;
use texture_synthesis as ts;

#[derive(Debug, Clone, Copy)]
struct TextureSynthesisParams {
    seed: u64,
    nearest_neighbors: u32,
    random_sample_locations: u64,
    backtrack_stages: u32,
    backtrack_percent: f32,
    cauchy_dispersion: f32,
    max_thread_count: usize,
}

impl Default for TextureSynthesisParams {
    fn default() -> Self {
        Self {
            seed: 211,
            nearest_neighbors: 50,
            random_sample_locations: 50,
            backtrack_stages: 5,
            backtrack_percent: 0.5,
            cauchy_dispersion: 1.0,
            max_thread_count: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .max(1),
        }
    }
}

pub struct TextureSynthesisInpaintTool {
    inpaint_base: RegionMaskInpaintToolBase,
    params: TextureSynthesisParams,
}

impl Default for TextureSynthesisInpaintTool {
    fn default() -> Self {
        Self {
            inpaint_base: RegionMaskInpaintToolBase::new("texture_synthesis_inpaint", Some(8)),
            params: TextureSynthesisParams::default(),
        }
    }
}

impl TextureSynthesisInpaintTool {
    fn run_with_params(
        image: &egui::ColorImage,
        mask: &egui::ColorImage,
        sample_mask: Option<&egui::ColorImage>,
        params: TextureSynthesisParams,
    ) -> Result<egui::ColorImage, String> {
        if image.size != mask.size {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        }
        if sample_mask.as_ref().is_some_and(|m| m.size != image.size) {
            return Err(t!("cleaning.tools.texture_synthesis.sample_mask_mismatch_error").to_string());
        }
        let width = image.size[0];
        let height = image.size[1];
        if width == 0 || height == 0 {
            return Ok(image.clone());
        }
        if !mask.pixels.iter().any(|px| px.a() > 0) {
            return Ok(image.clone());
        }

        let (src_rgba, inpaint_mask_rgba, sample_method_rgba) =
            build_ts_inputs(image, mask, sample_mask)?;
        let dims = ts::Dims::new(
            u32::try_from(width).map_err(|_| t!("cleaning.tools.texture_synthesis.region_width_too_large_error").to_string())?,
            u32::try_from(height).map_err(|_| t!("cleaning.tools.texture_synthesis.region_height_too_large_error").to_string())?,
        );

        let inpaint_mask_dyn = ts::image::DynamicImage::ImageRgba8(inpaint_mask_rgba.clone());
        let sample_method_dyn = ts::image::DynamicImage::ImageRgba8(sample_method_rgba.clone());
        let example_dyn = ts::image::DynamicImage::ImageRgba8(src_rgba.clone());

        let session = ts::Session::builder()
            .seed(params.seed)
            .nearest_neighbors(params.nearest_neighbors.max(1))
            .random_sample_locations(params.random_sample_locations.max(1))
            .backtrack_stages(params.backtrack_stages.max(1))
            .backtrack_percent(params.backtrack_percent.clamp(0.0, 1.0))
            .cauchy_dispersion(params.cauchy_dispersion.clamp(0.0, 1.0))
            .max_thread_count(params.max_thread_count.max(1))
            .inpaint_example(
                inpaint_mask_dyn,
                ts::Example::builder(example_dyn).set_sample_method(sample_method_dyn),
                dims,
            )
            .build()
            .map_err(|err| format!("texture-synthesis build error: {err}"))?;

        let generated = session.run(None);
        let out = generated.as_ref();
        let out_w = out.width() as usize;
        let out_h = out.height() as usize;
        if out_w == 0 || out_h == 0 {
            return Err(t!("cleaning.tools.texture_synthesis.empty_result_error").to_string());
        }
        if out_w != width || out_h != height {
            return Err(tf!("cleaning.tools.texture_synthesis.unexpected_size_error", out_w = out_w, out_h = out_h, width = width, height = height));
        }

        Ok(egui::ColorImage::from_rgba_unmultiplied(
            [out_w, out_h],
            out.as_raw(),
        ))
    }
}

impl CleaningTool for TextureSynthesisInpaintTool {
    fn tool_id(&self) -> &'static str {
        "texture_synthesis_inpaint"
    }

    fn title(&self) -> &'static str {
        t!("cleaning.tools.texture_synthesis.title")
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.inpaint_base.cancel_selection();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.inpaint_base.draw_ui_hint(ui);
        ui.small(t!("cleaning.tools.texture_synthesis.description_hint"));
        ui.small(t!("cleaning.tools.texture_synthesis.mask_legend_hint"));
        ui.small(t!("cleaning.tools.texture_synthesis.params_in_window_hint"));
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
        let (inpaint_base, params) = (&mut self.inpaint_base, &mut self.params);
        inpaint_base.draw_overlay_ui_custom_with_sample_mask(
            ctx,
            canvas,
            project,
            "Texture Synthesis Inpaint",
            {
                let params = *params;
                move |image, mask, sample_mask| {
                    Self::run_with_params(image, mask, sample_mask, params)
                }
            },
            move |ui| {
                RegionEditToolBase::draw_region_editor_collapsible_section(
                    ui,
                    "texture_synthesis_params",
                    t!("cleaning.tools.texture_synthesis.params_heading"),
                    false,
                    |ui| {
                        ui.add(WheelSlider::new(&mut params.seed, 0..=u64::MAX).text(t!("cleaning.tools.texture_synthesis.seed_label")))
                            .on_hover_text(
                                t!("cleaning.tools.texture_synthesis.seed_hint"),
                            );
                        ui.add(
                            WheelSlider::new(&mut params.nearest_neighbors, 1..=128)
                                .text(t!("cleaning.tools.texture_synthesis.neighbors_label")),
                        )
                        .on_hover_text(
                            t!("cleaning.tools.texture_synthesis.neighbors_hint"),
                        );
                        ui.add(
                            WheelSlider::new(&mut params.random_sample_locations, 1..=256)
                                .text(t!("cleaning.tools.texture_synthesis.random_samples_label")),
                        )
                        .on_hover_text(
                            t!("cleaning.tools.texture_synthesis.random_samples_hint"),
                        );
                        ui.add(
                            WheelSlider::new(&mut params.backtrack_stages, 1..=10)
                                .text(t!("cleaning.tools.texture_synthesis.backtrack_stages_label")),
                        )
                        .on_hover_text(
                            t!("cleaning.tools.texture_synthesis.backtrack_stages_hint"),
                        );
                        ui.add(
                            WheelSlider::new(&mut params.backtrack_percent, 0.0..=1.0)
                                .text(t!("cleaning.tools.texture_synthesis.backtrack_fraction_label")),
                        )
                        .on_hover_text(
                            t!("cleaning.tools.texture_synthesis.backtrack_fraction_hint"),
                        );
                        ui.add(
                            WheelSlider::new(&mut params.cauchy_dispersion, 0.0..=1.0)
                                .text(t!("cleaning.tools.texture_synthesis.cauchy_dispersion_label")),
                        )
                        .on_hover_text(
                            t!("cleaning.tools.texture_synthesis.cauchy_dispersion_hint"),
                        );
                        ui.add(
                            WheelSlider::new(&mut params.max_thread_count, 1..=128)
                                .text(t!("cleaning.tools.texture_synthesis.max_threads_label")),
                        )
                        .on_hover_text(
                            t!("cleaning.tools.texture_synthesis.max_threads_hint"),
                        );
                    },
                );
            },
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

fn build_ts_inputs(
    image: &egui::ColorImage,
    mask: &egui::ColorImage,
    sample_mask: Option<&egui::ColorImage>,
) -> Result<
    (
        ts::image::RgbaImage,
        ts::image::RgbaImage,
        ts::image::RgbaImage,
    ),
    String,
> {
    let width = image.size[0];
    let height = image.size[1];
    if image.size != mask.size {
        return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
    }
    if sample_mask.as_ref().is_some_and(|m| m.size != image.size) {
        return Err(t!("cleaning.tools.texture_synthesis.sample_mask_mismatch_error").to_string());
    }

    let w_u32 = u32::try_from(width).map_err(|_| t!("cleaning.tools.texture_synthesis.region_width_too_large_error").to_string())?;
    let h_u32 = u32::try_from(height).map_err(|_| t!("cleaning.tools.texture_synthesis.region_height_too_large_error").to_string())?;

    let mut src_raw = Vec::<u8>::with_capacity(width.saturating_mul(height).saturating_mul(4));
    let mut inpaint_mask_raw =
        Vec::<u8>::with_capacity(width.saturating_mul(height).saturating_mul(4));
    let mut sample_method_raw =
        Vec::<u8>::with_capacity(width.saturating_mul(height).saturating_mul(4));

    let mut fill_count = 0usize;
    let has_custom_sample_mask = sample_mask
        .as_ref()
        .is_some_and(|m| m.pixels.iter().any(|px| px.a() > 0));
    let mut allowed_sample_count = 0usize;

    for idx in 0..image.pixels.len() {
        let src = image.pixels[idx];
        let m = mask.pixels[idx];
        let [r, g, b, a] = src.to_srgba_unmultiplied();
        src_raw.extend_from_slice(&[r, g, b, a]);

        let fill = m.a() > 0;
        if fill {
            fill_count = fill_count.saturating_add(1);
            inpaint_mask_raw.extend_from_slice(&[0, 0, 0, 255]);
        } else {
            inpaint_mask_raw.extend_from_slice(&[255, 255, 255, 255]);
        }

        let allow_sample = if has_custom_sample_mask {
            let is_sample = sample_mask
                .as_ref()
                .and_then(|m| m.pixels.get(idx))
                .is_some_and(|px| px.a() > 0);
            is_sample && !fill
        } else {
            !fill
        };
        if allow_sample {
            allowed_sample_count = allowed_sample_count.saturating_add(1);
            sample_method_raw.extend_from_slice(&[255, 255, 255, 255]);
        } else {
            sample_method_raw.extend_from_slice(&[0, 0, 0, 255]);
        }
    }
    if fill_count >= width.saturating_mul(height) {
        return Err(
            t!("cleaning.tools.texture_synthesis.mask_covers_all_error")
                .to_string(),
        );
    }
    if has_custom_sample_mask && allowed_sample_count == 0 {
        return Err(
            t!("cleaning.tools.texture_synthesis.sample_mask_empty_error")
                .to_string(),
        );
    }

    let src_img = ts::image::RgbaImage::from_raw(w_u32, h_u32, src_raw)
        .ok_or_else(|| t!("cleaning.tools.texture_synthesis.build_source_error").to_string())?;
    let inpaint_mask_img = ts::image::RgbaImage::from_raw(w_u32, h_u32, inpaint_mask_raw)
        .ok_or_else(|| t!("cleaning.tools.texture_synthesis.build_mask_error").to_string())?;
    let sample_method_img = ts::image::RgbaImage::from_raw(w_u32, h_u32, sample_method_raw)
        .ok_or_else(|| t!("cleaning.tools.texture_synthesis.build_sample_mask_error").to_string())?;

    Ok((src_img, inpaint_mask_img, sample_method_img))
}
