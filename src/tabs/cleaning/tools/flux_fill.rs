/*
FILE HEADER (cleaning/tools/flux_fill.rs)
- Назначение: инструмент "AI редактирование области (FLUX.1 Fill)" вкладки cleaning
  на базе `RegionMaskInpaintToolBase`. Перерисовывает / удаляет содержимое под маской
  через Python AI backend (`inpaint.flux_fill`, стриминговый; `.unload`; `.status`).
- Режимы (`FluxMode`):
  - `ObjectRemoval` (по умолчанию): удаление объекта под маской, фон достраивается.
  - `Inpaint`: генерация по текстовому промпту.
- Модель: GGUF-квант (выбор из каталога `YarvixPA/FLUX.1-Fill-dev-GGUF`) + diffusers-
  компоненты — всё качается в `side_models/` бэкендом по требованию.
- Прогресс: один прогресс-бар на две фазы — `phase:"download"` (байты) и
  `phase:"generate"` (шаги). Кадры `progress` несут `phase/step/total/label`.
- UI: все параметры спрятаны в сворачиваемое меню (по умолчанию свёрнуто); прогресс-бар
  показывается над ним. Каталог квантов и отметка «скачано» подгружаются через `.status`.
- Хранение настроек: отдельный файл `flux_fill_inpaint_settings.json`
  (см. `config::flux_fill_inpaint_settings_path`).
*/
use super::base::{CleaningTool, RegionMaskInpaintToolBase, StrokePoint};
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

// Download (≈22 GB on first use) + diffusion may take a long time; allow a wide window.
const FLUX_BACKEND_CALL_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
const FLUX_STATUS_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// GGUF quant catalog (kept in sync with the backend `AVAILABLE_QUANTS`).
pub const FLUX_QUANTS: [&str; 9] = [
    "Q3_K_S", "Q4_0", "Q4_1", "Q4_K_S", "Q5_0", "Q5_1", "Q5_K_S", "Q6_K", "Q8_0",
];
const DEFAULT_QUANT: &str = "Q8_0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FluxMode {
    ObjectRemoval,
    Inpaint,
}

impl FluxMode {
    fn wire(self) -> &'static str {
        match self {
            FluxMode::ObjectRemoval => "object_removal",
            FluxMode::Inpaint => "inpaint",
        }
    }

    fn from_wire(value: &str) -> Self {
        match value.trim() {
            "inpaint" => FluxMode::Inpaint,
            _ => FluxMode::ObjectRemoval,
        }
    }
}

/// Persisted generation parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct FluxSettings {
    mode: String,
    quant: String,
    prompt: String,
    steps: u32,
    guidance: f32,
    seed: i64,
    max_seq: u32,
    max_side: u32,
    dilate: u32,
    feather: u32,
    seamless: bool,
    vae_tiling: bool,
    cpu_offload: bool,
    miopen_fast: bool,
}

impl Default for FluxSettings {
    fn default() -> Self {
        Self {
            mode: FluxMode::ObjectRemoval.wire().to_string(),
            quant: DEFAULT_QUANT.to_string(),
            prompt: String::new(),
            steps: 28,
            guidance: 30.0,
            seed: -1,
            max_seq: 512,
            max_side: 1536,
            dilate: 4,
            feather: 3,
            seamless: true,
            vae_tiling: true,
            cpu_offload: false,
            miopen_fast: true,
        }
    }
}

/// Immutable snapshot handed to the worker run closure.
#[derive(Debug, Clone)]
struct FluxRunConfig {
    settings: FluxSettings,
}

/// Live progress shared between the worker run thread and the editor UI.
#[derive(Default)]
struct FluxSharedProgress {
    active: bool,
    phase: String,
    step: u64,
    total: u64,
    label: String,
}

/// Quant catalog + download state pulled from the backend `.status` method.
#[derive(Default, Clone)]
struct FluxStatus {
    downloaded: Vec<String>,
    components_ready: bool,
}

pub struct FluxFillInpaintTool {
    inpaint_base: RegionMaskInpaintToolBase,
    settings: FluxSettings,
    unload_status: Option<String>,
    settings_rx: Option<Receiver<FluxSettings>>,
    settings_loaded: bool,
    dirty: bool,
    save_rx: Option<Receiver<()>>,
    progress: Arc<Mutex<FluxSharedProgress>>,
    status: Option<FluxStatus>,
    status_rx: Option<Receiver<Result<FluxStatus, String>>>,
}

impl Default for FluxFillInpaintTool {
    fn default() -> Self {
        let mut tool = Self {
            // Flux pads to multiples of 16 server-side, so no forced selection multiple.
            inpaint_base: RegionMaskInpaintToolBase::new("flux_fill_inpaint", None),
            settings: FluxSettings::default(),
            unload_status: None,
            settings_rx: None,
            settings_loaded: false,
            dirty: false,
            save_rx: None,
            progress: Arc::new(Mutex::new(FluxSharedProgress::default())),
            status: None,
            status_rx: None,
        };
        tool.request_settings_load();
        tool
    }
}

impl FluxFillInpaintTool {
    fn request_settings_load(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.settings_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(load_flux_settings());
        });
    }

    fn poll_settings_load(&mut self) {
        let Some(rx) = self.settings_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(settings) => {
                self.settings = settings;
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
        let settings = self.settings.clone();
        let (tx, rx) = mpsc::channel();
        self.save_rx = Some(rx);
        thread::spawn(move || {
            if let Err(err) = save_flux_settings(&settings) {
                crate::runtime_log::log_warn(format!(
                    "[cleaning] failed to save Flux Fill settings: {err}"
                ));
            }
            let _ = tx.send(());
        });
    }

    /// Kicks a one-shot background `.status` query (quant download state).
    fn request_status(&mut self) {
        if self.status_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.status_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(fetch_flux_status());
        });
    }

    fn poll_status(&mut self) {
        let Some(rx) = self.status_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(status)) => {
                self.status = Some(status);
                self.status_rx = None;
            }
            Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                self.status_rx = None;
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    fn run_flux(
        image: &egui::ColorImage,
        mask: &egui::ColorImage,
        cfg: &FluxRunConfig,
        progress: &Arc<Mutex<FluxSharedProgress>>,
    ) -> Result<egui::ColorImage, String> {
        if image.size != mask.size {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        }
        let (width, height) = (image.size[0], image.size[1]);
        if width == 0 || height == 0 {
            return Ok(image.clone());
        }
        if !mask.pixels.iter().any(|px| px.a() > 0) {
            return Ok(image.clone());
        }
        let mode = FluxMode::from_wire(&cfg.settings.mode);
        if mode == FluxMode::Inpaint && cfg.settings.prompt.trim().is_empty() {
            return Err(t!("cleaning.tools.flux.prompt_required_error").to_string());
        }

        let image_png = encode_color_image_png_rgba(image)?;
        let mask_png = encode_mask_png_luma(mask)?;
        let params = json!({
            "mode": mode.wire(),
            "quant": cfg.settings.quant,
            "prompt": cfg.settings.prompt,
            "steps": cfg.settings.steps,
            "guidance": cfg.settings.guidance,
            "seed": cfg.settings.seed,
            "max_seq": cfg.settings.max_seq,
            "max_side": cfg.settings.max_side,
            "dilate": cfg.settings.dilate,
            "feather": cfg.settings.feather,
            "seamless": cfg.settings.seamless,
            "vae_tiling": cfg.settings.vae_tiling,
            "cpu_offload": cfg.settings.cpu_offload,
            "miopen_fast": cfg.settings.miopen_fast,
        });
        let header = json!({
            "image_len": image_png.len(),
            "mask_len": mask_png.len(),
            "params": params,
        });
        let blob = concat_image_mask(&image_png, &mask_png);

        {
            let mut guard = lock_progress(progress);
            guard.active = true;
            guard.phase = "generate".to_string();
            guard.step = 0;
            guard.total = u64::from(cfg.settings.steps);
            guard.label = t!("cleaning.tools.flux.preparing_status").to_string();
        }
        let stream_result = flux_stream_call(header, &blob, |phase, step, total, label| {
            let mut guard = lock_progress(progress);
            guard.phase = phase;
            guard.step = step;
            guard.total = total;
            guard.label = label;
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
        let (out_w, out_h) = (out_rgba.width() as usize, out_rgba.height() as usize);
        if out_w != width || out_h != height {
            return Err(tf!("cleaning.inpaint.unexpected_size_error", out_w = out_w, out_h = out_h, width = width, height = height));
        }
        Ok(egui::ColorImage::from_rgba_unmultiplied(
            [out_w, out_h],
            out_rgba.as_raw(),
        ))
    }
}

impl CleaningTool for FluxFillInpaintTool {
    fn tool_id(&self) -> &'static str {
        "flux_fill_inpaint"
    }

    fn title(&self) -> &'static str {
        t!("cleaning.tools.flux.title")
    }

    fn pytorch_required(&self) -> bool {
        true
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.inpaint_base.cancel_selection();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.inpaint_base.draw_ui_hint(ui);
        ui.small(t!("cleaning.tools.flux.description_hint"));
        ui.small(t!("cleaning.tools.flux.autodownload_hint"));
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
        self.poll_status();

        let run_config = FluxRunConfig {
            settings: self.settings.clone(),
        };
        let run_progress = Arc::clone(&self.progress);
        let ui_progress = Arc::clone(&self.progress);

        let FluxFillInpaintTool {
            inpaint_base,
            settings,
            unload_status,
            status,
            ..
        } = self;
        let status_snapshot = status.clone();
        let mut changed = false;
        let mut want_status = false;

        inpaint_base.draw_overlay_ui_custom(
            ctx,
            canvas,
            project,
            t!("cleaning.tools.flux.title"),
            move |image, mask| Self::run_flux(image, mask, &run_config, &run_progress),
            |ui| {
                draw_flux_progress_ui(ui, &ui_progress);
                egui::CollapsingHeader::new(t!("cleaning.tools.flux.params_heading")).id_salt("cleaning.tools.flux.params_heading")
                    .id_salt("cleaning_flux_params_collapse")
                    .default_open(false)
                    .show(ui, |ui| {
                        changed |= draw_flux_params_ui(ui, settings, status_snapshot.as_ref());
                        if ui.small_button(t!("cleaning.tools.flux.refresh_download_button")).clicked() {
                            want_status = true;
                        }
                        if ui.small_button(t!("cleaning.tools.flux.unload_button")).clicked() {
                            *unload_status = match unload_flux() {
                                Ok(()) => Some(t!("cleaning.tools.flux.unload_requested_status").to_string()),
                                Err(err) => Some(tf!("cleaning.inpaint.unload_error", err = err)),
                            };
                        }
                        if let Some(s) = unload_status.as_ref() {
                            ui.small(s);
                        }
                    });
            },
        );

        if changed {
            self.dirty = true;
        }
        // Lazily fetch download state the first time the editor is shown, or on demand.
        if want_status || (self.status.is_none() && self.status_rx.is_none()) {
            self.request_status();
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

/// Draws the Flux parameter editor. Returns `true` if any value changed.
fn draw_flux_params_ui(
    ui: &mut egui::Ui,
    settings: &mut FluxSettings,
    status: Option<&FluxStatus>,
) -> bool {
    let mut changed = false;
    let mut mode = FluxMode::from_wire(&settings.mode);

    ui.horizontal(|ui| {
        ui.label(t!("cleaning.common.mode_label"));
        let mode_label = match mode {
            FluxMode::ObjectRemoval => t!("cleaning.tools.flux.mode_remove_object"),
            FluxMode::Inpaint => t!("cleaning.tools.flux.mode_prompt_repaint"),
        };
        WheelComboBox::from_id_salt("cleaning_flux_mode_picker")
            .selected_text(mode_label)
            .show_ui(ui, |ui| {
                changed |= ui
                    .selectable_value(&mut mode, FluxMode::ObjectRemoval, t!("cleaning.tools.flux.mode_remove_object"))
                    .changed();
                changed |= ui
                    .selectable_value(&mut mode, FluxMode::Inpaint, t!("cleaning.tools.flux.mode_prompt_repaint"))
                    .changed();
            });
    });
    if mode.wire() != settings.mode {
        settings.mode = mode.wire().to_string();
        changed = true;
    }

    ui.horizontal(|ui| {
        ui.label(t!("cleaning.tools.flux.quant_label"));
        let label = quant_label(&settings.quant, status);
        WheelComboBox::from_id_salt("cleaning_flux_quant_picker")
            .selected_text(label)
            .show_ui(ui, |ui| {
                for quant in FLUX_QUANTS {
                    let item = quant_label(quant, status);
                    changed |= ui
                        .selectable_value(&mut settings.quant, quant.to_string(), item)
                        .changed();
                }
            });
    });
    if let Some(s) = status {
        let comp = if s.components_ready {
            t!("cleaning.tools.flux.components_downloaded_status")
        } else {
            t!("cleaning.tools.flux.components_pending_status")
        };
        ui.small(comp);
    }

    let prompt_hint = match mode {
        FluxMode::ObjectRemoval => t!("cleaning.tools.flux.prompt_optional_label"),
        FluxMode::Inpaint => t!("cleaning.tools.flux.prompt_repaint_label"),
    };
    ui.label(prompt_hint);
    changed |= ui
        .add(egui::TextEdit::multiline(&mut settings.prompt).desired_rows(2))
        .changed();

    changed |= ui
        .add(WheelSlider::new(&mut settings.steps, 1..=100).text(t!("cleaning.common.steps_label")))
        .changed();
    changed |= ui
        .add(WheelSlider::new(&mut settings.guidance, 0.0..=60.0).text("Guidance"))
        .changed();
    changed |= ui
        .add(WheelSlider::new(&mut settings.dilate, 0..=100).text(t!("cleaning.common.mask_expand_label")))
        .changed();
    changed |= ui
        .add(WheelSlider::new(&mut settings.feather, 0..=100).text(t!("cleaning.tools.flux.edge_feather_label")))
        .changed();
    changed |= ui
        .checkbox(&mut settings.seamless, t!("cleaning.tools.flux.edge_stitch_label"))
        .changed();

    egui::CollapsingHeader::new(t!("cleaning.tools.flux.advanced_heading")).id_salt("cleaning.tools.flux.advanced_heading")
        .id_salt("cleaning_flux_advanced_collapse")
        .default_open(false)
        .show(ui, |ui| {
            changed |= ui
                .add(WheelSlider::new(&mut settings.max_seq, 64..=512).text("Max tokens"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut settings.max_side, 256..=4096).text(t!("cleaning.tools.flux.max_side_label")))
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
            changed |= ui
                .checkbox(&mut settings.vae_tiling, "VAE tiling/slicing")
                .changed();
            changed |= ui
                .checkbox(&mut settings.cpu_offload, t!("cleaning.tools.flux.cpu_offload_label"))
                .changed();
            changed |= ui
                .checkbox(&mut settings.miopen_fast, "MIOpen Fast (ROCm)")
                .changed();
        });

    changed
}

/// Builds a quant dropdown label with a "✓"/"скачать" hint from the status snapshot.
fn quant_label(quant: &str, status: Option<&FluxStatus>) -> String {
    match status {
        Some(s) if s.downloaded.iter().any(|q| q == quant) => format!("{quant} ✓"),
        Some(_) => tf!("cleaning.tools.flux.quant_download_label", quant = quant),
        None => quant.to_string(),
    }
}

fn lock_progress(progress: &Mutex<FluxSharedProgress>) -> MutexGuard<'_, FluxSharedProgress> {
    match progress.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

/// Draws the unified progress bar: download phase shows bytes %, generate shows steps.
fn draw_flux_progress_ui(ui: &mut egui::Ui, progress: &Mutex<FluxSharedProgress>) {
    let (active, phase, step, total, label) = {
        let g = lock_progress(progress);
        (g.active, g.phase.clone(), g.step, g.total, g.label.clone())
    };
    if !active {
        return;
    }
    let fraction = if total > 0 {
        (step as f32 / total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let text = if phase == "download" {
        let done_gb = step as f64 / 1e9;
        let total_gb = total as f64 / 1e9;
        tf!(
            "cleaning.tools.flux.download_progress_status",
            label = label,
            done = format!("{done_gb:.2}"),
            total = format!("{total_gb:.2}")
        )
    } else if total > 0 {
        tf!("cleaning.common.step_progress_status", step = step, total = total)
    } else {
        label.clone()
    };
    if total > 0 {
        ui.add(egui::ProgressBar::new(fraction).text(text));
    } else {
        ui.add(egui::ProgressBar::new(0.0).text(text));
    }
    ui.ctx().request_repaint();
}

fn concat_image_mask(image_png: &[u8], mask_png: &[u8]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(image_png.len() + mask_png.len());
    blob.extend_from_slice(image_png);
    blob.extend_from_slice(mask_png);
    blob
}

/// Streaming `inpaint.flux_fill` call. Each `progress` frame carries
/// `phase`/`step`/`total`/`label` in the header (no preview blob).
fn flux_stream_call<F>(
    header: Value,
    blob: &[u8],
    mut on_progress: F,
) -> Result<(Value, Vec<u8>), String>
where
    F: FnMut(String, u64, u64, String),
{
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call_streaming(
            backend_ipc::protocol::METHOD_INPAINT_FLUX_FILL,
            header,
            blob,
            |progress_header, _preview_blob| {
                let phase = progress_header
                    .get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or("generate")
                    .to_string();
                let step = progress_header
                    .get("step")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let total = progress_header
                    .get("total")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let label = progress_header
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                on_progress(phase, step, total, label);
            },
            FLUX_BACKEND_CALL_TIMEOUT,
        )
        .map_err(map_flux_call_error)
}

fn unload_flux() -> Result<(), String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call(
            backend_ipc::protocol::METHOD_INPAINT_FLUX_FILL_UNLOAD,
            json!({}),
            &[],
            FLUX_STATUS_CALL_TIMEOUT,
        )
        .map_err(map_flux_call_error)?;
    Ok(())
}

/// Queries the backend `.status` for the quant catalog + which are downloaded.
fn fetch_flux_status() -> Result<FluxStatus, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (header, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_INPAINT_FLUX_FILL_STATUS,
            json!({}),
            &[],
            FLUX_STATUS_CALL_TIMEOUT,
        )
        .map_err(map_flux_call_error)?;
    let downloaded = header
        .get("downloaded_quants")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let components_ready = header
        .get("components_ready")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(FluxStatus {
        downloaded,
        components_ready,
    })
}

fn map_flux_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => tf!("cleaning.inpaint.request_aborted_error", msg = msg),
        CallError::Transport(_) => ai_backend_offline_error().to_string(),
    }
}

fn load_flux_settings() -> FluxSettings {
    let path = config::flux_fill_inpaint_settings_path();
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return FluxSettings::default(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_flux_settings(settings: &FluxSettings) -> Result<(), String> {
    let path = config::flux_fill_inpaint_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    }
    let raw = serde_json::to_string_pretty(settings)
        .map_err(|err| tf!("cleaning.tools.flux.serialize_settings_error", err = err))?;
    fs::write(&path, raw).map_err(|err| tf!("cleaning.tools.flux.write_settings_error", err = err))
}

fn encode_color_image_png_rgba(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let (width, height) = (image.size[0], image.size[1]);
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
    let (width, height) = (mask.size[0], mask.size[1]);
    let width_u32 = u32::try_from(width).map_err(|_| t!("cleaning.png.mask_width_too_large_error").to_string())?;
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
        assert_eq!(FluxMode::from_wire("inpaint"), FluxMode::Inpaint);
        assert_eq!(FluxMode::from_wire("object_removal"), FluxMode::ObjectRemoval);
        assert_eq!(FluxMode::from_wire("bogus"), FluxMode::ObjectRemoval);
        assert_eq!(FluxMode::ObjectRemoval.wire(), "object_removal");
    }

    #[test]
    fn defaults_are_object_removal_q8() {
        let s = FluxSettings::default();
        assert_eq!(s.mode, "object_removal");
        assert_eq!(s.quant, "Q8_0");
        assert!(s.seamless);
        assert!(FLUX_QUANTS.contains(&s.quant.as_str()));
    }

    #[test]
    fn partial_json_uses_defaults() {
        let s: FluxSettings = serde_json::from_str("{}").expect("deserialize empty");
        assert_eq!(s.mode, "object_removal");
        assert_eq!(s.steps, 28);
    }

    #[test]
    fn blob_concat_orders_image_then_mask() {
        let image_png = b"IMAGE".to_vec();
        let mask_png = b"MASK".to_vec();
        let blob = concat_image_mask(&image_png, &mask_png);
        assert_eq!(blob.len(), image_png.len() + mask_png.len());
        assert_eq!(&blob[..image_png.len()], image_png.as_slice());
        assert_eq!(&blob[image_png.len()..], mask_png.as_slice());
    }

    #[test]
    fn quant_label_marks_downloaded() {
        let status = FluxStatus {
            downloaded: vec!["Q8_0".to_string()],
            components_ready: true,
        };
        assert_eq!(quant_label("Q8_0", Some(&status)), "Q8_0 ✓");
        assert_eq!(
            quant_label("Q4_0", Some(&status)),
            tf!("cleaning.tools.flux.quant_download_label", quant = "Q4_0")
        );
        assert_eq!(quant_label("Q8_0", None), "Q8_0");
    }
}
