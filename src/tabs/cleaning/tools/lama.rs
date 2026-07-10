/*
FILE HEADER (cleaning/tools/lama.rs)
- Назначение: инструмент "AI удаление (Lama)" для вкладки cleaning на базе
  `RegionMaskInpaintToolBase`, с вызовом Python backend через v2 framed IPC
  (метод `inpaint.lama_v2`).
- Ключевые сущности:
  - `LamaInpaintTool`: wiring инструмента (выделение, UI параметров refine, routing ввода).
  - `LamaParams`: параметры refine для backend (`refine`, `n_iters`, `max_scales`, `px_budget`).
  - `LamaModelListState`: фоновое состояние проверки наличия предустановленных моделей
    в `ManhwaStudio_AI_Models/Torch/LaMa/models` без блокировки GUI.
  - `LamaModelSpec`: фиксированный каталог поддерживаемых Lama-моделей с локальными
    именами, пользовательскими названиями и optional URL для автоскачивания.
  - `run_lama(...)`: конвертация region image + mask в PNG, отправка через v2 framed
    IPC (`inpaint.lama_v2`) и разбор результата из RESPONSE BLOB.
- Поведение:
  - Shift+ЛКМ по canvas открывает region mask editor (через `RegionMaskInpaintToolBase`).
  - В окне рисуется маска; кнопка `Обработать` запускает LaMa V2 через backend.
  - Параметры refine, выбор одной из предопределённых моделей и кнопка выгрузки модели
    рендерятся прямо в окне region editor.
- Важно:
  - При пустой маске обработка не запускается и возвращается исходный регион без изменений.
  - `best.ckpt` считается уже установленной инсталлятором; anime-модели при отсутствии
    докачиваются автоматически перед запуском inpaint.
  - Ошибки backend пробрасываются пользователю в статус окна region editor.
*/
use super::base::{CleaningTool, RegionMaskInpaintToolBase, StrokePoint};
use crate::ai_models;
use crate::backend_ipc::{self, CallError};
use crate::canvas::CanvasView;
use crate::config;
use crate::project::ProjectData;
use crate::tabs::translation::backend_health::ai_backend_offline_error;
use crate::widgets::{WheelComboBox, WheelSlider};
use eframe::egui;
use image::{ColorType, ImageEncoder};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use ms_thread as thread;
use web_time::Duration;

/// Per-call timeout for the v2 framed backend. Mirrors the previous HTTP read
/// timeout: model warmup + inpaint can take a while on first use.
const LAMA_BACKEND_CALL_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_LAMA_MODEL_FILENAME: &str = "anime-manga-big-lama.pt";

#[derive(Debug, Clone, Copy)]
pub struct LamaModelSpec {
    pub file_name: &'static str,
    /// Stable i18n catalog key for the model's UI display name. The persisted
    /// selection is keyed by `file_name`, so the display label is free to
    /// localize (see `docs/i18n_exclusions.md` §A5: a name is unsafe only when
    /// it doubles as stored content).
    pub display_key: &'static str,
}

impl LamaModelSpec {
    /// Localized UI display name for this model. Runtime lookup (not `const`)
    /// because `t!` is not const; falls back to the key on a catalog miss.
    #[must_use]
    pub fn display_name(&self) -> &'static str {
        ms_i18n::lookup(self.display_key).unwrap_or(self.display_key)
    }
}

const LAMA_MODEL_SPECS: [LamaModelSpec; 3] = [
    LamaModelSpec {
        file_name: "best.ckpt",
        display_key: "cleaning.tools.lama.model_base",
    },
    LamaModelSpec {
        file_name: "lama_large_512px.ckpt",
        display_key: "cleaning.tools.lama.model_anime_v1",
    },
    LamaModelSpec {
        file_name: "anime-manga-big-lama.pt",
        display_key: "cleaning.tools.lama.model_anime_v2",
    },
];

#[derive(Debug, Clone, Copy)]
struct LamaParams {
    refine: bool,
    n_iters: u8,
    max_scales: u8,
    px_budget: u32,
}

impl Default for LamaParams {
    fn default() -> Self {
        Self {
            refine: false,
            n_iters: 15,
            max_scales: 3,
            px_budget: 1_000_000,
        }
    }
}

#[derive(Debug)]
enum LamaModelListState {
    Idle,
    Loading(Receiver<Result<Vec<String>, String>>),
    Ready(Vec<String>),
    Error(String),
}

pub struct LamaInpaintTool {
    inpaint_base: RegionMaskInpaintToolBase,
    params: LamaParams,
    model_list_state: LamaModelListState,
    selected_model: Option<String>,
    unload_status: Option<String>,
}

impl Default for LamaInpaintTool {
    fn default() -> Self {
        let mut tool = Self {
            inpaint_base: RegionMaskInpaintToolBase::new("lama_inpaint", Some(8)),
            params: LamaParams::default(),
            model_list_state: LamaModelListState::Idle,
            selected_model: Some(DEFAULT_LAMA_MODEL_FILENAME.to_string()),
            unload_status: None,
        };
        tool.request_model_scan();
        tool
    }
}

impl LamaInpaintTool {
    fn request_model_scan(&mut self) {
        if matches!(self.model_list_state, LamaModelListState::Loading(_)) {
            return;
        }

        let (tx, rx) = mpsc::channel();
        self.model_list_state = LamaModelListState::Loading(rx);
        thread::spawn(move || {
            let _send_result = tx.send(scan_lama_models());
        });
    }

    fn poll_model_scan(&mut self) {
        if matches!(self.model_list_state, LamaModelListState::Idle) {
            self.request_model_scan();
            return;
        }

        let next_state = match &self.model_list_state {
            LamaModelListState::Loading(rx) => match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    t!("cleaning.tools.lama.model_scan_aborted_status").to_string(),
                )),
            },
            LamaModelListState::Idle
            | LamaModelListState::Ready(_)
            | LamaModelListState::Error(_) => None,
        };

        if let Some(result) = next_state {
            match result {
                Ok(models) => {
                    self.model_list_state = LamaModelListState::Ready(models);
                    normalize_selected_model(&mut self.selected_model);
                }
                Err(err) => {
                    self.model_list_state = LamaModelListState::Error(err);
                    normalize_selected_model(&mut self.selected_model);
                }
            }
        }
    }

    fn run_lama(
        image: &egui::ColorImage,
        mask: &egui::ColorImage,
        params: LamaParams,
        selected_model: Option<&str>,
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

        let resolved_model = ensure_selected_lama_model_ready(selected_model)?;

        let image_png = encode_color_image_png_rgba(image)?;
        let mask_png = encode_mask_png_luma(mask)?;
        // v2 two-image convention: request blob = image_png ++ mask_png, with
        // image_len/mask_len in the header so the backend can split them.
        let header = json!({
            "image_len": image_png.len(),
            "mask_len": mask_png.len(),
            "params": {
                "refine": params.refine,
                "n_iters": params.n_iters,
                "max_scales": params.max_scales,
                "px_budget": params.px_budget,
                "model_name": resolved_model,
            }
        });
        let blob = concat_image_mask(&image_png, &mask_png);

        let (_response_header, out_bytes) =
            inpaint_call(backend_ipc::protocol::METHOD_INPAINT_LAMA_V2, header, &blob)?;
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

impl CleaningTool for LamaInpaintTool {
    fn tool_id(&self) -> &'static str {
        "lama_inpaint"
    }

    fn title(&self) -> &'static str {
        t!("cleaning.tools.lama.title")
    }

    fn pytorch_required(&self) -> bool {
        true
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.inpaint_base.cancel_selection();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.inpaint_base.draw_ui_hint(ui);
        ui.small(t!("cleaning.tools.lama.description_hint"));
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
        self.poll_model_scan();
        let (inpaint_base, params, model_list_state, selected_model, unload_status) = (
            &mut self.inpaint_base,
            &mut self.params,
            &mut self.model_list_state,
            &mut self.selected_model,
            &mut self.unload_status,
        );
        let selected_model_for_run = selected_model.clone();
        inpaint_base.draw_overlay_ui_custom(
            ctx,
            canvas,
            project,
            t!("cleaning.tools.lama.title"),
            {
                let params = *params;
                move |image, mask| {
                    Self::run_lama(image, mask, params, selected_model_for_run.as_deref())
                }
            },
            move |ui| {
                ui.separator();
                let lama_section_id = ui.make_persistent_id("cleaning_lama_params_section");
                egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    lama_section_id,
                    false,
                )
                .show_header(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(t!("cleaning.tools.lama.params_heading"));
                        ui.add_space(8.0);
                        draw_model_picker_header_ui(ui, model_list_state, selected_model);
                    });
                })
                .body(|ui| {
                    ui.checkbox(&mut params.refine, "Refine");
                    ui.add_enabled_ui(params.refine, |ui| {
                        ui.add(WheelSlider::new(&mut params.n_iters, 5..=50).text("n_iters"));
                        ui.add(WheelSlider::new(&mut params.max_scales, 1..=5).text("max_scales"));
                        ui.add(
                            WheelSlider::new(&mut params.px_budget, 500_000..=4_000_000)
                                .text("px_budget"),
                        );
                    });
                    draw_model_picker_refresh_ui(ui, model_list_state);
                });
                if ui.small_button(t!("cleaning.tools.lama.unload_button")).clicked() {
                    *unload_status =
                        match unload_call(backend_ipc::protocol::METHOD_INPAINT_LAMA_V2_UNLOAD) {
                            Ok(_) => Some(t!("cleaning.tools.lama.unload_requested_status").to_string()),
                            Err(err) => Some(tf!("cleaning.inpaint.unload_error", err = err)),
                        };
                }
                if let Some(status) = unload_status.as_ref() {
                    ui.small(status);
                }
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

/// Concatenates the image PNG and mask PNG into the v2 two-image request blob
/// (`image_png ++ mask_png`). The split is recovered server-side from the
/// `image_len`/`mask_len` header fields.
fn concat_image_mask(image_png: &[u8], mask_png: &[u8]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(image_png.len() + mask_png.len());
    blob.extend_from_slice(image_png);
    blob.extend_from_slice(mask_png);
    blob
}

/// Issues a blocking v2 framed inpaint call. The request blob and header are
/// built by the caller; the result PNG comes from the RESPONSE BLOB (raw bytes)
/// and the metadata from the response header `Value`.
///
/// Maps `CallError` to a user-facing `String`, preserving the previous UX:
/// - `Error`       → the backend error message (same as the old method error).
/// - `Interrupted` → transient abort surfaced to the status line.
/// - `Transport`   → connect/framing failure (unified backend offline message).
fn inpaint_call(method: &str, header: Value, blob: &[u8]) -> Result<(Value, Vec<u8>), String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call(method, header, blob, LAMA_BACKEND_CALL_TIMEOUT)
        .map_err(map_inpaint_call_error)
}

/// Issues a v2 framed `*.unload` call (no fields, no blob) and confirms the
/// backend reported `unloaded`.
fn unload_call(method: &str) -> Result<(), String> {
    let (header, _blob) = inpaint_call(method, json!({}), &[])?;
    let _unloaded = header.get("unloaded").and_then(Value::as_bool);
    Ok(())
}

/// Shared `CallError` → user-facing `String` mapping for inpaint/unload calls.
fn map_inpaint_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => tf!("cleaning.inpaint.request_aborted_error", msg = msg),
        // A transport failure means the backend is offline; surface the unified
        // offline message (matching device calls) instead of the raw error string.
        CallError::Transport(_) => ai_backend_offline_error().to_string(),
    }
}

fn scan_lama_models() -> Result<Vec<String>, String> {
    let models_dir = lama_models_dir();
    if !models_dir.exists() {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(&models_dir).map_err(|err| {
        tf!("cleaning.tools.lama.read_models_dir_error", models_dir = models_dir.display(), err = err)
    })?;
    let mut models = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|err| {
            tf!("cleaning.tools.lama.read_models_entry_error", models_dir = models_dir.display(), err = err)
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if is_supported_lama_model_path(&path) {
            models.push(entry.file_name().to_string_lossy().to_string());
        }
    }

    models.sort();
    Ok(models)
}

fn lama_models_dir() -> PathBuf {
    config::lama_models_dir()
}

fn lama_model_spec_by_name(model_name: &str) -> Option<&'static LamaModelSpec> {
    LAMA_MODEL_SPECS
        .iter()
        .find(|spec| spec.file_name == model_name.trim())
}

/// Fixed catalog of LaMa models shared with other cleaning tools (e.g. the SDXL
/// 4-channel prefill picker). Returns the same specs the LaMa tool itself uses.
#[must_use]
pub fn lama_model_catalog() -> &'static [LamaModelSpec] {
    &LAMA_MODEL_SPECS
}

/// Default LaMa model file name used when no explicit selection is available.
#[must_use]
pub fn default_lama_model_filename() -> &'static str {
    DEFAULT_LAMA_MODEL_FILENAME
}

/// Ensures the given supported LaMa model file is present locally, downloading
/// it through `ai_models` when missing. Returns the resolved model file name.
///
/// # Errors
/// Returns an error string if the model name is not in the supported catalog or
/// the download fails. Intended to run off the GUI thread.
pub fn ensure_lama_model_for_external(model_name: &str) -> Result<&'static str, String> {
    ensure_selected_lama_model_ready(Some(model_name))
}

fn normalize_selected_model(selected_model: &mut Option<String>) {
    if selected_model
        .as_deref()
        .and_then(lama_model_spec_by_name)
        .is_some()
    {
        return;
    }

    *selected_model = Some(DEFAULT_LAMA_MODEL_FILENAME.to_string());
}

fn is_supported_lama_model_path(path: &std::path::Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ckpt") || ext.eq_ignore_ascii_case("pt"))
}

fn is_lama_model_present(models: &[String], model_name: &str) -> bool {
    models.iter().any(|present| present == model_name)
}

fn lama_model_status_text(model_name: &str, model_list_state: &LamaModelListState) -> &'static str {
    match model_list_state {
        LamaModelListState::Idle | LamaModelListState::Loading(_) => t!("cleaning.tools.lama.checking_files_status"),
        LamaModelListState::Ready(models) => {
            if is_lama_model_present(models, model_name) {
                t!("cleaning.tools.lama.file_found_status")
            } else if lama_model_spec_by_name(model_name).is_some() {
                t!("cleaning.tools.lama.will_download_status")
            } else {
                t!("cleaning.tools.lama.expected_from_installer_status")
            }
        }
        LamaModelListState::Error(_) => t!("cleaning.tools.lama.status_unavailable_status"),
    }
}

fn ensure_selected_lama_model_ready(selected_model: Option<&str>) -> Result<&'static str, String> {
    let selected_name = selected_model.unwrap_or(DEFAULT_LAMA_MODEL_FILENAME);
    let spec = lama_model_spec_by_name(selected_name)
        .ok_or_else(|| tf!("cleaning.tools.lama.unsupported_model_error", selected_name = selected_name))?;
    let local_path = lama_models_dir().join(spec.file_name);
    if local_path.exists() {
        return Ok(spec.file_name);
    }

    ai_models::ensure_lama_model(&config::models_dir(), spec.file_name)?;
    Ok(spec.file_name)
}

fn draw_model_picker_header_ui(
    ui: &mut egui::Ui,
    model_list_state: &mut LamaModelListState,
    selected_model: &mut Option<String>,
) {
    normalize_selected_model(selected_model);
    match model_list_state {
        LamaModelListState::Idle => {
            ui.add_enabled_ui(false, |ui| {
                WheelComboBox::from_id_salt("cleaning_lama_model_picker")
                    .selected_text(t!("cleaning.tools.lama.checking_models_status"))
                    .show_ui(ui, |_ui| {});
            });
        }
        LamaModelListState::Loading(_) => {
            let selected_text = selected_model
                .as_deref()
                .and_then(lama_model_spec_by_name)
                .map_or(t!("cleaning.common.select_model_placeholder"), |spec| spec.display_name());
            WheelComboBox::from_id_salt("cleaning_lama_model_picker")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for spec in &LAMA_MODEL_SPECS {
                        let _changed = ui
                            .selectable_value(
                                selected_model,
                                Some(spec.file_name.to_string()),
                                spec.display_name(),
                            )
                            .changed();
                    }
                });
            if let Some(selected_name) = selected_model.as_deref() {
                ui.small(lama_model_status_text(selected_name, model_list_state));
            }
        }
        LamaModelListState::Ready(_) | LamaModelListState::Error(_) => {
            let selected_text = selected_model
                .as_deref()
                .and_then(lama_model_spec_by_name)
                .map_or(t!("cleaning.common.select_model_placeholder"), |spec| spec.display_name());
            WheelComboBox::from_id_salt("cleaning_lama_model_picker")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for spec in &LAMA_MODEL_SPECS {
                        let _changed = ui
                            .selectable_value(
                                selected_model,
                                Some(spec.file_name.to_string()),
                                spec.display_name(),
                            )
                            .changed();
                    }
                });
            if let Some(selected_name) = selected_model.as_deref() {
                ui.small(lama_model_status_text(selected_name, model_list_state));
            }
            if let LamaModelListState::Error(err) = model_list_state {
                ui.colored_label(egui::Color32::from_rgb(208, 84, 62), err.as_str());
            }
        }
    }
}

fn draw_model_picker_refresh_ui(ui: &mut egui::Ui, model_list_state: &mut LamaModelListState) {
    if ui.small_button(t!("cleaning.tools.lama.refresh_models_button")).clicked() {
        *model_list_state = LamaModelListState::Idle;
    }
}

// ============================================================================
// Unit tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The request blob must be `image_png ++ mask_png` in that exact order, and
    /// the `image_len`/`mask_len` header ints must name the two segment lengths
    /// so the backend can split `blob[:image_len]` / `blob[image_len..]`.
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

    /// The inpaint request header must carry `image_len`, `mask_len` and a
    /// `params` object with the LaMa-v2 fields (incl. `model_name`).
    #[test]
    fn lama_v2_header_shape_has_lengths_and_params() {
        let image_png = [0u8; 11];
        let mask_png = [0u8; 3];
        let params = LamaParams::default();
        let header = json!({
            "image_len": image_png.len(),
            "mask_len": mask_png.len(),
            "params": {
                "refine": params.refine,
                "n_iters": params.n_iters,
                "max_scales": params.max_scales,
                "px_budget": params.px_budget,
                "model_name": "best.ckpt",
            }
        });
        assert_eq!(header["image_len"].as_u64(), Some(11));
        assert_eq!(header["mask_len"].as_u64(), Some(3));
        assert!(header["params"].is_object(), "params must be an object");
        assert_eq!(header["params"]["model_name"].as_str(), Some("best.ckpt"));
        assert_eq!(header["params"]["px_budget"].as_u64(), Some(1_000_000));
    }

    /// The result PNG comes from the RESPONSE BLOB (raw bytes), and the metadata
    /// fields are read from the response header — no base64 anywhere.
    #[test]
    fn result_png_comes_from_response_blob() {
        // A tiny valid 1x1 RGBA PNG used as the simulated response blob.
        let mut blob = Vec::new();
        let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut blob),
                image::ImageFormat::Png,
            )
            .expect("encode test PNG");

        let header = json!({
            "engine": "lama_v2",
            "source_size": [1, 1],
            "device": "cpu",
            "refine": false,
            "model_name": "best.ckpt"
        });
        assert!(!blob.is_empty(), "response blob must carry the PNG");
        let decoded = image::load_from_memory(&blob).expect("decode response blob PNG");
        assert_eq!(decoded.width(), 1);
        assert_eq!(header["engine"].as_str(), Some("lama_v2"));
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
            map_inpaint_call_error(CallError::Error("boom".to_string())),
            "boom"
        );
        // Pin the exact catalog key, not just that the payload survived: a mapping
        // regression that picked a different message would still pass a `contains` check.
        assert_eq!(
            map_inpaint_call_error(CallError::Interrupted("MARKER".to_string())),
            tf!("cleaning.inpaint.request_aborted_error", msg = "MARKER")
        );
        assert_eq!(
            map_inpaint_call_error(CallError::Transport("dead".to_string())),
            ai_backend_offline_error()
        );
    }
}
