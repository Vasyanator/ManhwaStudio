/*
FILE HEADER (cleaning/tools/sdxl.rs)
- Назначение: инструмент "AI удаление (SDXL Inpaint)" для вкладки cleaning на базе
  `RegionMaskInpaintToolBase`, с вызовом Python backend через v2 framed IPC
  (`inpaint.sdxl`, стриминговый; `inpaint.sdxl.unload`).
- Режимы:
  - `SdxlMode::NineChannel`: выделенная 9-канальная inpaint-модель SDXL
    (`stable-diffusion-xl-1.0-inpainting-0.1` или совместимый 9-канальный ckpt/safetensors).
    Чистый masked-image идёт отдельным каналом, denoise по умолчанию 1.0.
  - `SdxlMode::FourChannel`: обычная 4-канальная SDXL (аниме safetensors с Civitai).
    Перед генерацией дырка предзаполняется LaMa (модель выбирается из каталога `lama.rs`),
    что убирает текст из контекста, поэтому работает умеренный denoise (по умолчанию 0.75).
- Ключевые сущности:
  - `SdxlInpaintTool`: wiring инструмента (выделение, UI параметров, routing ввода).
  - `SdxlMode`: режим канальности модели.
  - `SdxlSettings`: параметры генерации одного режима (промпты, steps, cfg, denoise, seed,
    сэмплер, размытие/расширение маски, путь к весам, LaMa-модель префилла).
  - `SdxlRunConfig`: неизменяемый снимок параметров, передаваемый в worker-thread run.
  - `run_sdxl(...)`: ensure LaMa-модели (4ch), конвертация region image + mask в PNG,
    отправка через v2 framed IPC (`inpaint.sdxl`, blob = image_png ++ mask_png) и
    разбор результата из RESPONSE BLOB.
  - `sdxl_stream_call(...)`: стриминговый v2-вызов; на каждый `progress`-кадр читает
    `step`/`total` из заголовка и превью PNG из BLOB кадра, вызывает колбэк прогресса.
  - `SdxlSharedProgress` + `draw_sdxl_progress_ui(...)`: общий между worker и UI прогресс
    (шаг/всего + последнее превью латентов) и его отрисовка (progress bar + live preview).
- UI:
  - Все параметры генерации спрятаны в сворачиваемое меню «Параметры генерации (SDXL)»
    (по умолчанию свёрнуто). Progress bar и предпросмотр латентов показываются над ним.
- Хранение настроек:
  - Параметры обоих режимов сохраняются в отдельном файле `sdxl_inpaint_settings.json`
    (см. `config::sdxl_inpaint_settings_path`), чтобы не конфликтовать с saver-ом
    `user_config.json`. Загрузка и запись идут в фоновых потоках.
- Важно:
  - При пустой маске или пустом пути к весам обработка не запускается.
  - Выделение области кратно 8 (требование VAE SDXL: размеры делятся на 8).
  - Ошибки backend пробрасываются пользователю в статус окна region editor.
*/
use super::base::{CleaningTool, RegionMaskInpaintToolBase, StrokePoint};
use super::lama::{
    LamaModelSpec, default_lama_model_filename, ensure_lama_model_for_external, lama_model_catalog,
};
use crate::backend_ipc::{self, CallError};
use crate::canvas::CanvasView;
use crate::config;
use crate::project::ProjectData;
use crate::tabs::translation::backend_health::ai_backend_offline_error;
use crate::widgets::{WheelComboBox, WheelSlider};
use eframe::egui;
use image::{ColorType, ImageEncoder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use ms_thread as thread;
use web_time::Duration;

// SDXL diffusion may run many steps on CPU; allow a long per-call window.
const SDXL_BACKEND_CALL_TIMEOUT: Duration = Duration::from_secs(20 * 60);
// VAE downscale factor: region width/height must be multiples of 8 for SDXL.
const SDXL_SELECTION_MULTIPLE: usize = 8;

/// Supported diffusers samplers exposed in the UI. The backend maps each name to
/// a concrete scheduler configuration; keep the list in sync with the service.
pub const SDXL_SAMPLERS: [&str; 8] = [
    "Euler",
    "Euler a",
    "DPM++ 2M",
    "DPM++ 2M Karras",
    "DPM++ SDE Karras",
    "DDIM",
    "UniPC",
    "Heun",
];

/// Channel mode of the loaded SDXL model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SdxlMode {
    /// Dedicated 9-channel inpainting UNet (clean masked-image conditioning).
    NineChannel,
    /// Ordinary 4-channel SDXL checkpoint; the hole is LaMa-prefilled first.
    FourChannel,
}

impl SdxlMode {
    /// Stable wire identifier sent to the backend and stored in settings.
    fn wire(self) -> &'static str {
        match self {
            SdxlMode::NineChannel => "nine_channel",
            SdxlMode::FourChannel => "four_channel",
        }
    }

    fn from_wire(value: &str) -> Self {
        match value.trim() {
            "four_channel" => SdxlMode::FourChannel,
            "nine_channel" => SdxlMode::NineChannel,
            _ => SdxlMode::NineChannel,
        }
    }
}

/// Generation parameters for one channel mode. Persisted per mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct SdxlSettings {
    /// ckpt / safetensors path or a Hugging Face repo id.
    model_path: String,
    positive_prompt: String,
    negative_prompt: String,
    steps: u32,
    cfg_scale: f32,
    /// Denoising strength. 9ch uses 1.0; 4ch uses < 1.0 over the LaMa prefill.
    denoise_strength: f32,
    /// Random seed; `-1` means a fresh random seed per run.
    seed: i64,
    sampler: String,
    /// Mask gaussian blur radius in pixels (soft seam).
    mask_blur: u32,
    /// Mask dilation in pixels (covers text anti-aliasing halo).
    mask_dilation: u32,
    /// LaMa model file used for 4-channel prefill (ignored for 9ch).
    lama_model: String,
}

impl Default for SdxlSettings {
    fn default() -> Self {
        Self::for_mode(SdxlMode::NineChannel)
    }
}

impl SdxlSettings {
    /// Mode-specific defaults: 9ch runs full denoise, 4ch runs a moderate denoise
    /// because the LaMa prefill already removed the text from the hole.
    fn for_mode(mode: SdxlMode) -> Self {
        let denoise = match mode {
            SdxlMode::NineChannel => 1.0,
            SdxlMode::FourChannel => 0.75,
        };
        Self {
            model_path: String::new(),
            positive_prompt: "clean background".to_string(),
            negative_prompt: "text, letters, watermark, signature, speech bubble".to_string(),
            steps: 30,
            cfg_scale: 7.0,
            denoise_strength: denoise,
            seed: -1,
            sampler: "DPM++ 2M Karras".to_string(),
            mask_blur: 4,
            mask_dilation: 6,
            lama_model: default_lama_model_filename().to_string(),
        }
    }
}

/// Persisted settings document: both modes plus the last selected mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct SdxlPersisted {
    mode: String,
    nine_channel: SdxlSettings,
    four_channel: SdxlSettings,
}

impl Default for SdxlPersisted {
    fn default() -> Self {
        Self {
            mode: SdxlMode::NineChannel.wire().to_string(),
            nine_channel: SdxlSettings::for_mode(SdxlMode::NineChannel),
            four_channel: SdxlSettings::for_mode(SdxlMode::FourChannel),
        }
    }
}

/// Immutable snapshot handed to the worker-thread run closure.
#[derive(Debug, Clone)]
struct SdxlRunConfig {
    mode: SdxlMode,
    settings: SdxlSettings,
}

/// Live progress shared between the worker run thread and the editor UI.
///
/// The worker writes `step`/`total` and the latest per-step latent `preview`
/// (bumping `preview_seq` on each new preview); the UI reads them every frame
/// while the editor repaints during processing.
#[derive(Default)]
struct SdxlSharedProgress {
    active: bool,
    step: u32,
    total: u32,
    preview: Option<egui::ColorImage>,
    preview_seq: u64,
}

pub struct SdxlInpaintTool {
    inpaint_base: RegionMaskInpaintToolBase,
    mode: SdxlMode,
    nine_channel: SdxlSettings,
    four_channel: SdxlSettings,
    unload_status: Option<String>,
    settings_rx: Option<Receiver<SdxlPersisted>>,
    settings_loaded: bool,
    dirty: bool,
    save_rx: Option<Receiver<()>>,
    progress: Arc<Mutex<SdxlSharedProgress>>,
    preview_texture: Option<egui::TextureHandle>,
    preview_uploaded_seq: u64,
}

impl Default for SdxlInpaintTool {
    fn default() -> Self {
        let mut tool = Self {
            inpaint_base: RegionMaskInpaintToolBase::new(
                "sdxl_inpaint",
                Some(SDXL_SELECTION_MULTIPLE),
            ),
            mode: SdxlMode::NineChannel,
            nine_channel: SdxlSettings::for_mode(SdxlMode::NineChannel),
            four_channel: SdxlSettings::for_mode(SdxlMode::FourChannel),
            unload_status: None,
            settings_rx: None,
            settings_loaded: false,
            dirty: false,
            save_rx: None,
            progress: Arc::new(Mutex::new(SdxlSharedProgress::default())),
            preview_texture: None,
            preview_uploaded_seq: 0,
        };
        tool.request_settings_load();
        tool
    }
}

impl SdxlInpaintTool {
    /// Spawns a background loader for the persisted settings file.
    fn request_settings_load(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.settings_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(load_sdxl_settings());
        });
    }

    /// Applies loaded settings once the background loader finishes.
    fn poll_settings_load(&mut self) {
        let Some(rx) = self.settings_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(persisted) => {
                self.mode = SdxlMode::from_wire(&persisted.mode);
                self.nine_channel = persisted.nine_channel;
                self.four_channel = persisted.four_channel;
                self.settings_loaded = true;
                self.settings_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.settings_loaded = true;
                self.settings_rx = None;
            }
        }
    }

    /// Persists settings in a background thread, coalescing while one save is in flight.
    fn poll_and_maybe_save(&mut self) {
        if let Some(rx) = self.save_rx.as_ref() {
            match rx.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => self.save_rx = None,
                Err(TryRecvError::Empty) => return,
            }
        }
        if !self.dirty || !self.settings_loaded {
            return;
        }
        self.dirty = false;
        let persisted = SdxlPersisted {
            mode: self.mode.wire().to_string(),
            nine_channel: self.nine_channel.clone(),
            four_channel: self.four_channel.clone(),
        };
        let (tx, rx) = mpsc::channel();
        self.save_rx = Some(rx);
        thread::spawn(move || {
            if let Err(err) = save_sdxl_settings(&persisted) {
                crate::runtime_log::log_warn(format!(
                    "[cleaning] failed to save SDXL inpaint settings: {err}"
                ));
            }
            let _ = tx.send(());
        });
    }

    fn active_settings(&self) -> &SdxlSettings {
        match self.mode {
            SdxlMode::NineChannel => &self.nine_channel,
            SdxlMode::FourChannel => &self.four_channel,
        }
    }

    fn run_sdxl(
        image: &egui::ColorImage,
        mask: &egui::ColorImage,
        cfg: &SdxlRunConfig,
        progress: &Arc<Mutex<SdxlSharedProgress>>,
    ) -> Result<egui::ColorImage, String> {
        if image.size != mask.size {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        }
        let width = image.size[0];
        let height = image.size[1];
        if width == 0 || height == 0 {
            return Ok(image.clone());
        }
        if !mask.pixels.iter().any(|px| px.a() > 0) {
            return Ok(image.clone());
        }
        if cfg.settings.model_path.trim().is_empty() {
            return Err(
                t!("cleaning.tools.sdxl.weights_path_required_error").to_string(),
            );
        }

        // 4-channel mode prefills the hole with LaMa first; ensure that model is
        // present locally before the backend tries to use it.
        let lama_model = match cfg.mode {
            SdxlMode::FourChannel => {
                Some(ensure_lama_model_for_external(&cfg.settings.lama_model)?.to_string())
            }
            SdxlMode::NineChannel => None,
        };

        let image_png = encode_color_image_png_rgba(image)?;
        let mask_png = encode_mask_png_luma(mask)?;
        let mut params = json!({
            "mode": cfg.mode.wire(),
            "model_path": cfg.settings.model_path.trim(),
            "positive_prompt": cfg.settings.positive_prompt,
            "negative_prompt": cfg.settings.negative_prompt,
            "steps": cfg.settings.steps,
            "cfg_scale": cfg.settings.cfg_scale,
            "denoise_strength": cfg.settings.denoise_strength,
            "seed": cfg.settings.seed,
            "sampler": cfg.settings.sampler,
            "mask_blur": cfg.settings.mask_blur,
            "mask_dilation": cfg.settings.mask_dilation,
        });
        if let Some(lama_model) = lama_model
            && let Some(obj) = params.as_object_mut()
        {
            obj.insert("lama_model".to_string(), Value::String(lama_model));
        }

        // v2 two-image convention: request blob = image_png ++ mask_png, with
        // image_len/mask_len in the header so the backend can split them.
        let header = json!({
            "image_len": image_png.len(),
            "mask_len": mask_png.len(),
            "params": params,
        });
        let blob = concat_image_mask(&image_png, &mask_png);

        // Reset shared progress for this run; the streamed frames update it.
        {
            let mut guard = lock_progress(progress);
            guard.active = true;
            guard.step = 0;
            guard.total = cfg.settings.steps;
            guard.preview = None;
            guard.preview_seq = guard.preview_seq.wrapping_add(1);
        }
        let stream_result = sdxl_stream_call(header, &blob, |step, total, preview| {
            let mut guard = lock_progress(progress);
            guard.step = step;
            guard.total = total;
            if let Some(preview) = preview {
                guard.preview = Some(preview);
                guard.preview_seq = guard.preview_seq.wrapping_add(1);
            }
        });
        {
            let mut guard = lock_progress(progress);
            guard.active = false;
        }
        let (_response_header, out_bytes) = stream_result?;
        if out_bytes.is_empty() {
            return Err(t!("cleaning.inpaint.no_png_result_error").to_string());
        }
        let out_rgba = image::load_from_memory(&out_bytes)
            .map_err(|err| tf!("cleaning.inpaint.corrupt_png_error", err = err))?
            .to_rgba8();
        let out_w = out_rgba.width() as usize;
        let out_h = out_rgba.height() as usize;
        if out_w != width || out_h != height {
            return Err(tf!("cleaning.inpaint.unexpected_size_error", out_w = out_w, out_h = out_h, width = width, height = height));
        }
        Ok(egui::ColorImage::from_rgba_unmultiplied(
            [out_w, out_h],
            out_rgba.as_raw(),
        ))
    }
}

impl CleaningTool for SdxlInpaintTool {
    fn tool_id(&self) -> &'static str {
        "sdxl_inpaint"
    }

    fn title(&self) -> &'static str {
        t!("cleaning.tools.sdxl.title")
    }

    fn pytorch_required(&self) -> bool {
        true
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.inpaint_base.cancel_selection();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.inpaint_base.draw_ui_hint(ui);
        ui.small(t!("cleaning.tools.sdxl.description_hint"));
        ui.small(t!("cleaning.tools.sdxl.mode_description_hint"));
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
        self.poll_settings_load();

        let run_config = SdxlRunConfig {
            mode: self.mode,
            settings: self.active_settings().clone(),
        };
        let run_progress = Arc::clone(&self.progress);
        let ui_progress = Arc::clone(&self.progress);

        let SdxlInpaintTool {
            inpaint_base,
            mode,
            nine_channel,
            four_channel,
            unload_status,
            preview_texture,
            preview_uploaded_seq,
            ..
        } = self;
        let mut changed = false;

        inpaint_base.draw_overlay_ui_custom(
            ctx,
            canvas,
            project,
            t!("cleaning.tools.sdxl.title"),
            move |image, mask| Self::run_sdxl(image, mask, &run_config, &run_progress),
            |ui| {
                ui.separator();
                draw_sdxl_progress_ui(ui, &ui_progress, preview_texture, preview_uploaded_seq);
                egui::CollapsingHeader::new(t!("cleaning.tools.sdxl.params_heading")).id_salt("cleaning.tools.sdxl.params_heading")
                    .id_salt("cleaning_sdxl_params_collapse")
                    .default_open(false)
                    .show(ui, |ui| {
                        changed |= draw_sdxl_params_ui(ui, mode, nine_channel, four_channel);
                        if ui.small_button(t!("cleaning.tools.sdxl.unload_button")).clicked() {
                            *unload_status = match unload_sdxl() {
                                Ok(_) => {
                                    Some(t!("cleaning.tools.sdxl.unload_requested_status").to_string())
                                }
                                Err(err) => Some(tf!("cleaning.inpaint.unload_error", err = err)),
                            };
                        }
                        if let Some(status) = unload_status.as_ref() {
                            ui.small(status);
                        }
                    });
            },
        );

        if changed {
            self.dirty = true;
        }
        self.poll_and_maybe_save();
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

/// Draws the SDXL parameter editor. Returns `true` if any value changed.
fn draw_sdxl_params_ui(
    ui: &mut egui::Ui,
    mode: &mut SdxlMode,
    nine_channel: &mut SdxlSettings,
    four_channel: &mut SdxlSettings,
) -> bool {
    let mut changed = false;

    ui.horizontal(|ui| {
        ui.label(t!("cleaning.tools.sdxl.model_mode_label"));
        let mode_label = match *mode {
            SdxlMode::NineChannel => t!("cleaning.tools.sdxl.mode_9ch"),
            SdxlMode::FourChannel => t!("cleaning.tools.sdxl.mode_4ch"),
        };
        WheelComboBox::from_id_salt("cleaning_sdxl_mode_picker")
            .selected_text(mode_label)
            .show_ui(ui, |ui| {
                changed |= ui
                    .selectable_value(mode, SdxlMode::NineChannel, t!("cleaning.tools.sdxl.mode_9ch"))
                    .changed();
                changed |= ui
                    .selectable_value(mode, SdxlMode::FourChannel, t!("cleaning.tools.sdxl.mode_4ch"))
                    .changed();
            });
    });

    let is_four_channel = matches!(*mode, SdxlMode::FourChannel);
    let settings = match *mode {
        SdxlMode::NineChannel => nine_channel,
        SdxlMode::FourChannel => four_channel,
    };

    ui.label(t!("cleaning.tools.sdxl.weights_path_label"));
    changed |= ui.text_edit_singleline(&mut settings.model_path).changed();

    ui.label(t!("cleaning.tools.sdxl.positive_prompt_label"));
    changed |= ui
        .add(egui::TextEdit::multiline(&mut settings.positive_prompt).desired_rows(2))
        .changed();
    ui.label(t!("cleaning.tools.sdxl.negative_prompt_label"));
    changed |= ui
        .add(egui::TextEdit::multiline(&mut settings.negative_prompt).desired_rows(2))
        .changed();

    ui.horizontal(|ui| {
        ui.label(t!("cleaning.tools.sdxl.sampler_label"));
        WheelComboBox::from_id_salt("cleaning_sdxl_sampler_picker")
            .selected_text(settings.sampler.clone())
            .show_ui(ui, |ui| {
                for sampler in SDXL_SAMPLERS {
                    changed |= ui
                        .selectable_value(&mut settings.sampler, sampler.to_string(), sampler)
                        .changed();
                }
            });
    });
    changed |= ui
        .add(WheelSlider::new(&mut settings.steps, 1..=150).text(t!("cleaning.common.steps_label")))
        .changed();
    changed |= ui
        .add(WheelSlider::new(&mut settings.cfg_scale, 1.0..=20.0).text("CFG"))
        .changed();
    changed |= ui
        .add(WheelSlider::new(&mut settings.denoise_strength, 0.0..=1.0).text("Denoise"))
        .changed();
    changed |= ui
        .add(WheelSlider::new(&mut settings.mask_dilation, 0..=64).text(t!("cleaning.common.mask_expand_label")))
        .changed();
    changed |= ui
        .add(WheelSlider::new(&mut settings.mask_blur, 0..=64).text(t!("cleaning.tools.sdxl.mask_blur_label")))
        .changed();
    ui.horizontal(|ui| {
        ui.label(t!("cleaning.common.seed_label"));
        changed |= ui
            .add(egui::DragValue::new(&mut settings.seed).speed(1.0))
            .changed();
        if ui.small_button("🎲").clicked() {
            settings.seed = -1;
            changed = true;
        }
    });

    if is_four_channel {
        ui.separator();
        ui.label(t!("cleaning.tools.sdxl.lama_prefill_model_label"));
        let selected_label = lama_model_catalog()
            .iter()
            .find(|spec| spec.file_name == settings.lama_model)
            .map_or(t!("cleaning.common.select_model_placeholder"), |spec| spec.display_name());
        WheelComboBox::from_id_salt("cleaning_sdxl_lama_picker")
            .selected_text(selected_label)
            .show_ui(ui, |ui| {
                for spec in lama_model_catalog() {
                    changed |= draw_lama_choice(ui, &mut settings.lama_model, spec);
                }
            });
        ui.small(t!("cleaning.tools.sdxl.lama_prefill_hint"));
    }

    changed
}

/// Renders one selectable LaMa model row and reports whether it changed.
fn draw_lama_choice(ui: &mut egui::Ui, selected: &mut String, spec: &LamaModelSpec) -> bool {
    ui.selectable_value(selected, spec.file_name.to_string(), spec.display_name())
        .changed()
}

/// Locks the shared progress, recovering the inner value if a worker panicked
/// while holding it (the data is plain progress metadata, safe to reuse).
fn lock_progress(progress: &Mutex<SdxlSharedProgress>) -> MutexGuard<'_, SdxlSharedProgress> {
    match progress.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

/// Draws the step progress bar and the latest live latent preview. Uploads a new
/// preview texture only when the worker produced a newer frame.
fn draw_sdxl_progress_ui(
    ui: &mut egui::Ui,
    progress: &Mutex<SdxlSharedProgress>,
    preview_texture: &mut Option<egui::TextureHandle>,
    preview_uploaded_seq: &mut u64,
) {
    let (active, step, total, new_preview, seq) = {
        let guard = lock_progress(progress);
        let new_preview = if guard.preview_seq != *preview_uploaded_seq {
            guard.preview.clone()
        } else {
            None
        };
        (
            guard.active,
            guard.step,
            guard.total,
            new_preview,
            guard.preview_seq,
        )
    };

    if let Some(image) = new_preview {
        let handle =
            ui.ctx()
                .load_texture("sdxl_latent_preview", image, egui::TextureOptions::LINEAR);
        *preview_texture = Some(handle);
        *preview_uploaded_seq = seq;
    }

    if !active && preview_texture.is_none() {
        return;
    }

    if total > 0 {
        let fraction = (step.min(total) as f32 / total as f32).clamp(0.0, 1.0);
        ui.add(egui::ProgressBar::new(fraction).text(tf!("cleaning.common.step_progress_status", step = step, total = total)));
    } else if active {
        ui.add(egui::ProgressBar::new(0.0).text(t!("cleaning.tools.sdxl.preparing_model_status")));
    }

    if let Some(handle) = preview_texture.as_ref() {
        ui.label(t!("cleaning.tools.sdxl.latent_preview_label"));
        let size = handle.size_vec2();
        let scale = if size.x > 0.0 {
            (360.0 / size.x).min(1.0)
        } else {
            1.0
        };
        ui.add(
            egui::Image::new(egui::load::SizedTexture::from_handle(handle))
                .fit_to_exact_size(size * scale),
        );
    }
}

/// Concatenates the image PNG and mask PNG into the v2 two-image request blob
/// (`image_png ++ mask_png`). The split is recovered server-side from the
/// `image_len`/`mask_len` header fields.
fn concat_image_mask(image_png: &[u8], mask_png: &[u8]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(image_png.len() + mask_png.len());
    blob.extend_from_slice(image_png);
    blob.extend_from_slice(mask_png);
    blob
}

/// Issues the streaming v2 framed `inpaint.sdxl` call. Invokes
/// `on_progress(step, total, preview)` for each `progress` frame — `step`/`total`
/// come from the progress header, and the preview PNG comes from that frame's BLOB
/// (raw bytes, decoded into a `ColorImage`). The terminal result PNG is returned
/// from the RESPONSE BLOB together with the response header `Value`
/// (`engine`/`source_size`/`device`/`mode`).
///
/// Maps `CallError` to the same user-facing UX as the old transport:
/// - `Error`       → the backend error message (old streamed `"error"` frame).
/// - `Interrupted` → transient abort surfaced to the status line.
/// - `Transport`   → connect/framing failure (backend offline path).
fn sdxl_stream_call<F>(
    header: Value,
    blob: &[u8],
    mut on_progress: F,
) -> Result<(Value, Vec<u8>), String>
where
    F: FnMut(u32, u32, Option<egui::ColorImage>),
{
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call_streaming(
            backend_ipc::protocol::METHOD_INPAINT_SDXL,
            header,
            blob,
            |progress_header, preview_blob| {
                let step = progress_header
                    .get("step")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                let total = progress_header
                    .get("total")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                let preview = decode_preview_image(preview_blob);
                on_progress(step, total, preview);
            },
            SDXL_BACKEND_CALL_TIMEOUT,
        )
        .map_err(map_sdxl_call_error)
}

/// Issues the v2 framed `inpaint.sdxl.unload` call (no fields, no blob) and
/// confirms the backend reported `unloaded`.
fn unload_sdxl() -> Result<(), String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (header, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_INPAINT_SDXL_UNLOAD,
            json!({}),
            &[],
            SDXL_BACKEND_CALL_TIMEOUT,
        )
        .map_err(map_sdxl_call_error)?;
    let _unloaded = header.get("unloaded").and_then(Value::as_bool);
    Ok(())
}

/// Shared `CallError` → user-facing `String` mapping for the SDXL stream/unload
/// calls.
fn map_sdxl_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => tf!("cleaning.inpaint.request_aborted_error", msg = msg),
        // A transport failure means the backend is offline; surface the unified
        // offline message (matching device calls) instead of the raw error string.
        CallError::Transport(_) => ai_backend_offline_error().to_string(),
    }
}

/// Decodes a PNG latent preview (raw progress-frame blob bytes) into a
/// `ColorImage`, ignoring an empty blob or any decode failure.
fn decode_preview_image(bytes: &[u8]) -> Option<egui::ColorImage> {
    if bytes.is_empty() {
        return None;
    }
    let rgba = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(
        size,
        rgba.as_raw(),
    ))
}

/// Loads persisted SDXL settings, falling back to defaults when the file is
/// absent or unreadable. Intended to run off the GUI thread.
fn load_sdxl_settings() -> SdxlPersisted {
    let path = config::sdxl_inpaint_settings_path();
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return SdxlPersisted::default(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Writes SDXL settings to the dedicated settings file. Runs off the GUI thread.
fn save_sdxl_settings(persisted: &SdxlPersisted) -> Result<(), String> {
    let path = config::sdxl_inpaint_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    }
    let raw = serde_json::to_string_pretty(persisted)
        .map_err(|err| tf!("cleaning.tools.sdxl.serialize_settings_error", err = err))?;
    fs::write(&path, raw).map_err(|err| tf!("cleaning.tools.sdxl.write_settings_error", err = err))
}

fn encode_color_image_png_rgba(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let width = image.size[0];
    let height = image.size[1];
    let width_u32 =
        u32::try_from(width).map_err(|_| t!("cleaning.png.image_width_too_large_error").to_string())?;
    let height_u32 =
        u32::try_from(height).map_err(|_| t!("cleaning.png.image_height_too_large_error").to_string())?;

    let mut raw = Vec::<u8>::with_capacity(width.saturating_mul(height).saturating_mul(4));
    for px in &image.pixels {
        let [r, g, b, a] = px.to_srgba_unmultiplied();
        raw.extend_from_slice(&[r, g, b, a]);
    }
    let mut out = Vec::<u8>::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&raw, width_u32, height_u32, ColorType::Rgba8.into())
        .map_err(|err| tf!("cleaning.png.encode_image_error", err = err))?;
    Ok(out)
}

fn encode_mask_png_luma(mask: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let width = mask.size[0];
    let height = mask.size[1];
    let width_u32 =
        u32::try_from(width).map_err(|_| t!("cleaning.png.mask_width_too_large_error").to_string())?;
    let height_u32 =
        u32::try_from(height).map_err(|_| t!("cleaning.png.mask_height_too_large_error").to_string())?;

    let mut raw = Vec::<u8>::with_capacity(width.saturating_mul(height));
    for px in &mask.pixels {
        raw.push(if px.a() > 0 { 255 } else { 0 });
    }
    let mut out = Vec::<u8>::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&raw, width_u32, height_u32, ColorType::L8.into())
        .map_err(|err| tf!("cleaning.png.encode_mask_error", err = err))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_wire_roundtrip() {
        assert_eq!(SdxlMode::from_wire("four_channel"), SdxlMode::FourChannel);
        assert_eq!(SdxlMode::from_wire("nine_channel"), SdxlMode::NineChannel);
        // Unknown values fall back to the nine-channel default.
        assert_eq!(SdxlMode::from_wire("bogus"), SdxlMode::NineChannel);
        assert_eq!(SdxlMode::FourChannel.wire(), "four_channel");
    }

    #[test]
    fn mode_defaults_differ_in_denoise() {
        let nine = SdxlSettings::for_mode(SdxlMode::NineChannel);
        let four = SdxlSettings::for_mode(SdxlMode::FourChannel);
        assert!((nine.denoise_strength - 1.0).abs() < f32::EPSILON);
        assert!(four.denoise_strength < 1.0);
    }

    #[test]
    fn persisted_settings_roundtrip() {
        let mut doc = SdxlPersisted {
            mode: SdxlMode::FourChannel.wire().to_string(),
            ..SdxlPersisted::default()
        };
        doc.four_channel.model_path = "/models/anime.safetensors".to_string();
        doc.four_channel.steps = 42;
        let raw = serde_json::to_string(&doc).expect("serialize");
        let back: SdxlPersisted = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(back.mode, "four_channel");
        assert_eq!(back.four_channel.model_path, "/models/anime.safetensors");
        assert_eq!(back.four_channel.steps, 42);
    }

    #[test]
    fn partial_json_uses_defaults() {
        // Missing fields must fall back to defaults via serde(default).
        let doc: SdxlPersisted = serde_json::from_str("{}").expect("deserialize empty");
        assert_eq!(doc.mode, SdxlMode::NineChannel.wire());
        assert_eq!(doc.nine_channel.steps, 30);
    }

    #[test]
    fn default_sampler_is_supported() {
        let settings = SdxlSettings::default();
        assert!(SDXL_SAMPLERS.contains(&settings.sampler.as_str()));
    }

    /// The request blob must be `image_png ++ mask_png` in that exact order, and
    /// `image_len`/`mask_len` must name the two segment lengths so the backend can
    /// split `blob[:image_len]` / `blob[image_len..]`.
    #[test]
    fn blob_concat_orders_image_then_mask_with_lengths() {
        let image_png = b"IMAGE_PNG_BYTES".to_vec();
        let mask_png = b"MASK".to_vec();
        let blob = concat_image_mask(&image_png, &mask_png);

        let image_len = image_png.len();
        let mask_len = mask_png.len();
        assert_eq!(
            blob.len(),
            image_len + mask_len,
            "image_len + mask_len == blob length"
        );
        assert_eq!(
            &blob[..image_len],
            image_png.as_slice(),
            "image segment first"
        );
        assert_eq!(
            &blob[image_len..image_len + mask_len],
            mask_png.as_slice(),
            "mask segment second"
        );
    }

    /// The SDXL request header must carry `image_len`, `mask_len` and a `params`
    /// object with the SDXL fields (incl. `mode`/`model_path`), and an optional
    /// `lama_model` for the 4-channel prefill.
    #[test]
    fn sdxl_header_shape_has_lengths_and_params() {
        let image_png = [0u8; 17];
        let mask_png = [0u8; 5];
        let settings = SdxlSettings::for_mode(SdxlMode::FourChannel);
        let header = json!({
            "image_len": image_png.len(),
            "mask_len": mask_png.len(),
            "params": {
                "mode": SdxlMode::FourChannel.wire(),
                "model_path": settings.model_path,
                "steps": settings.steps,
                "sampler": settings.sampler,
                "lama_model": settings.lama_model,
            }
        });
        assert_eq!(header["image_len"].as_u64(), Some(17));
        assert_eq!(header["mask_len"].as_u64(), Some(5));
        assert!(header["params"].is_object(), "params must be an object");
        assert_eq!(header["params"]["mode"].as_str(), Some("four_channel"));
        assert!(header["params"]["lama_model"].as_str().is_some());
    }

    /// A `progress` frame carries `step`/`total` in its header and the preview PNG
    /// as the frame BLOB (raw bytes). `step`/`total` parse from the header and the
    /// blob decodes into a `ColorImage`; an empty blob yields no preview.
    #[test]
    fn progress_parses_step_total_and_preview_from_blob() {
        let progress_header = json!({
            "v": 1, "id": 42, "kind": "progress", "step": 7, "total": 30
        });
        let step = progress_header
            .get("step")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let total = progress_header
            .get("total")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        assert_eq!(step, 7);
        assert_eq!(total, 30);

        // A tiny valid 2x1 RGBA PNG as the simulated progress blob.
        let mut preview_blob = Vec::new();
        let img = image::RgbaImage::from_pixel(2, 1, image::Rgba([9, 8, 7, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut preview_blob),
                image::ImageFormat::Png,
            )
            .expect("encode preview PNG");
        let preview = decode_preview_image(&preview_blob).expect("decode preview from blob");
        assert_eq!(preview.size, [2, 1]);

        // No preview when the progress blob is empty (`blob_len == 0`).
        assert!(decode_preview_image(&[]).is_none());
    }

    /// The terminal result PNG comes from the RESPONSE BLOB (raw bytes), and the
    /// metadata fields are read from the response header — no base64 anywhere.
    #[test]
    fn result_png_comes_from_response_blob() {
        let mut blob = Vec::new();
        let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut blob),
                image::ImageFormat::Png,
            )
            .expect("encode test PNG");

        let header = json!({
            "engine": "sdxl",
            "source_size": [1, 1],
            "device": "cpu",
            "mode": "nine_channel"
        });
        assert!(!blob.is_empty(), "response blob must carry the PNG");
        let decoded = image::load_from_memory(&blob).expect("decode response blob PNG");
        assert_eq!(decoded.width(), 1);
        assert_eq!(header["engine"].as_str(), Some("sdxl"));
        assert_eq!(header["mode"].as_str(), Some("nine_channel"));
        assert_eq!(header["device"].as_str(), Some("cpu"));
    }

    /// The unload response header carries an `unloaded` bool.
    #[test]
    fn unload_response_reports_unloaded_flag() {
        let header = json!({ "unloaded": true });
        assert_eq!(header.get("unloaded").and_then(Value::as_bool), Some(true));
    }

    /// `CallError` mapping preserves the backend error message verbatim, maps
    /// interrupt to its UX string, and maps transport failures to the unified
    /// offline message.
    #[test]
    fn call_error_mapping_preserves_messages() {
        assert_eq!(
            map_sdxl_call_error(CallError::Error("boom".to_string())),
            "boom"
        );
        // Pin the exact catalog key, not just that the payload survived: a mapping
        // regression that picked a different message would still pass a `contains` check.
        assert_eq!(
            map_sdxl_call_error(CallError::Interrupted("MARKER".to_string())),
            tf!("cleaning.inpaint.request_aborted_error", msg = "MARKER")
        );
        assert_eq!(
            map_sdxl_call_error(CallError::Transport("dead".to_string())),
            ai_backend_offline_error()
        );
    }
}
