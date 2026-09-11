/*
File: cleaning/tools/mask_generation.rs

Purpose:
The host-neutral core of «Сгенерировать маску»: the catalog of backend mask sources, the
availability rules that decide which of them may run right now, the worker call that asks the
Python backend for a mask, and the conversion of its answer into a binary mask in REGION pixel
coordinates. It owns no editor state and draws no layout of its own, so every mask-carrying
tool can offer the same generation without a copy of any of it.

Two hosts consume it and neither may fork it:
- `base.rs::RegionMaskInpaintToolBase`, the detached region-editor window of the classic
  mask-inpaint tools;
- `ai_editor/`, the «ИИ-редактор области» host, which writes the answer into the selected
  layer of its `RegionFrame`'s mask stack.

Main responsibilities:
- name the four sources and say what each of them needs (Torch, a running backend)
- hold the parameters a host persists nowhere and re-reads every frame (`MaskGenerationState`)
- keep the watermark catalog's ✓/«скачать» marks fresh with at most one query in flight
- run the detection on a worker thread and hand back a 0/255 alpha buffer
- draw the two reusable controls: the source picker and the source's own parameters

Key structures:
- `MaskSource`: the four backend sources, with `label()` and `requires_torch()`
- `MaskGenerationParams`: source, mask dilation, watermark model — one `Copy` bundle
- `MaskGenerationState`: the params plus the watermark catalog state and the live progress
- `GeneratedMask`: a validated 0/255 alpha buffer with its size
- `MaskGenerationPoll`: what a host learns from one poll of a running job
- `WatermarkProgress`, `WatermarkModelSpec`, `WatermarkStatus`: the watermark source's extras,
  shared with the standalone `watermark_removal.rs` tool

Key functions:
- `spawn_mask_generation` / `poll_mask_generation`: the whole worker lifecycle
- `generate_mask`: the blocking detection, called only from the worker
- `draw_source_picker` / `draw_source_params` / `generate_button_hover_text`: the shared UI
- `detect_watermark_mask`, `spawn_watermark_status_query`: the streaming watermark calls

Notes:
NOTHING here may be called on the GUI thread except the `draw_*` helpers, `poll_*` and
`spawn_*`: every other function performs a blocking IPC round trip.
*/
use ms_backend_ipc::{self as backend_ipc, CallError};
use ms_tab_translation::backend_health::ai_backend_offline_error;
use ms_tab_translation::text_detector::{
    TextDetectorAiCtdOptions, TextDetectorPaddleOcrOptions, detect_ai_ctd_mask_for_image,
    detect_paddle_mask_for_image, detect_surya_mask_for_image, encode_color_image_png_rgba,
    parse_mask_alpha_from_blob,
};
use ms_widgets::{WheelComboBox, WheelSlider};
use eframe::egui;
use egui::Color32;
use ms_thread as thread;
use serde_json::{Value, json};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use web_time::Duration;

/// Per-message timeout of the streaming `watermark.detect` call. `wait_streaming`
/// restarts it on every frame received, so this bounds the gap BETWEEN frames,
/// not the total duration of a first run that downloads code and weights.
const WATERMARK_DETECT_CALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Timeout of the one-shot `watermark.status` catalog query.
const WATERMARK_STATUS_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Long side the backend downscales the region to before running the detector.
/// The mask branch has to see the whole watermark at once, so the pass is not
/// tiled (`dev-docs/watermark_removal_plan.md` §3.3).
pub(super) const WATERMARK_DETECT_DOWNSCALE_TO: u32 = 512;
/// Binarization threshold the backend applies to the predicted soft mask.
const WATERMARK_DETECT_THRESHOLD: f32 = 0.5;
/// Default watermark model: best PSNR / mask F1 of the three (plan §7.2).
pub(super) const DEFAULT_WATERMARK_MODEL: &str = "slbr";
/// Inclusive range the mask-dilation control offers, in region pixels.
const MASK_DILATE_RANGE: std::ops::RangeInclusive<i32> = 0..=30;

/// One entry of the watermark-detector catalog.
///
/// `id` is the WIRE value sent as `params.model` and the persisted selection
/// identity, so it stays a literal; only `display_key` (an i18n catalog key
/// resolved at render time) is localized — same split as `LamaModelSpec`,
/// see `dev-docs/i18n_exclusions.md` §A5.
///
/// Shared with the standalone `watermark_removal.rs` tool, which selects from the
/// same catalog — the catalog must never be duplicated per tool.
#[derive(Debug, Clone, Copy)]
pub(super) struct WatermarkModelSpec {
    pub(super) id: &'static str,
    display_key: &'static str,
}

impl WatermarkModelSpec {
    /// Localized display name of the model; falls back to the catalog key when
    /// the key is missing from the active locale.
    pub(super) fn display_name(self) -> &'static str {
        ms_i18n::lookup(self.display_key).unwrap_or(self.display_key)
    }
}

/// Fixed watermark-detector catalog, mirroring the backend's supported models.
pub(super) const WATERMARK_MODEL_SPECS: [WatermarkModelSpec; 3] = [
    WatermarkModelSpec {
        id: "slbr",
        display_key: "cleaning.tools.watermark.model_slbr",
    },
    WatermarkModelSpec {
        id: "wdnet",
        display_key: "cleaning.tools.watermark.model_wdnet",
    },
    WatermarkModelSpec {
        id: "splitnet",
        display_key: "cleaning.tools.watermark.model_splitnet",
    },
];

/// Snapshot of the backend `watermark.status` response: which catalog models
/// already have their weights and their network code on disk.
#[derive(Debug, Default, Clone)]
pub(super) struct WatermarkStatus {
    downloaded_models: Vec<String>,
    code_ready_models: Vec<String>,
}

impl WatermarkStatus {
    /// True when running `model_id` needs no download: both the weights and the
    /// (runtime-fetched) network code are present on the backend side.
    pub(super) fn is_ready(&self, model_id: &str) -> bool {
        self.downloaded_models.iter().any(|id| id == model_id)
            && self.code_ready_models.iter().any(|id| id == model_id)
    }
}

/// Live progress of a streaming `watermark.*` call, shared between the worker
/// that runs it and the editor UI that renders the progress bar.
///
/// Written by the mask-generation worker of this module and by the standalone
/// `watermark_removal.rs` tool, which streams `watermark.remove` with the same
/// two-phase (`download` bytes / `generate` steps) contract.
#[derive(Debug, Default)]
pub(super) struct WatermarkProgress {
    pub(super) active: bool,
    /// `"download"` (bytes) or `"generate"` (steps), from the `progress` frame.
    pub(super) phase: String,
    pub(super) step: u64,
    pub(super) total: u64,
    pub(super) label: String,
}

/// One backend source a mask can be generated from.
///
/// The three text detectors reach the backend through the translation module's typed helpers;
/// the watermark source streams `watermark.detect`. What the resulting mask MEANS is the
/// consuming tool's business — this enum only says where the pixels come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MaskSource {
    ComicTextDetector,
    PaddleOcr,
    Surya,
    Watermark,
}

/// Every source, in the order the picker offers them: the cheapest first, the watermark
/// detector — which may download weights on its first run — last.
pub(super) const MASK_SOURCES: [MaskSource; 4] = [
    MaskSource::PaddleOcr,
    MaskSource::Surya,
    MaskSource::ComicTextDetector,
    MaskSource::Watermark,
];

impl MaskSource {
    /// UI label of the source. Detector names are brand names and stay literal
    /// (`dev-docs/i18n_exclusions.md`); the watermark source is a description and
    /// is localized.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ComicTextDetector => "ComicTextDetector",
            Self::PaddleOcr => "PaddleOCR",
            Self::Surya => "Surya",
            Self::Watermark => t!("cleaning.mask_editor.source.watermark"),
        }
    }

    /// True when the source runs a Torch model in the Python backend and must
    /// therefore be disabled while the backend reports no Torch.
    pub(super) fn requires_torch(self) -> bool {
        match self {
            Self::PaddleOcr => false,
            Self::ComicTextDetector | Self::Surya | Self::Watermark => true,
        }
    }

    /// Whether this source could run right now: the backend answers, and Torch is present
    /// when the source needs it.
    ///
    /// This is the ONLY availability rule; a host that disables the button for a reason of
    /// its own (a busy worker, a locked frame) adds that reason beside this one and never
    /// instead of it.
    #[must_use]
    pub(super) fn is_available(self, backend_available: bool, torch_available: bool) -> bool {
        backend_available && (torch_available || !self.requires_torch())
    }
}

/// Everything a generation run needs beyond the pixels themselves.
#[derive(Debug, Clone, Copy)]
pub(super) struct MaskGenerationParams {
    pub(super) source: MaskSource,
    /// Dilation applied to the detected mask, in region pixels.
    pub(super) dilate_size: i32,
    /// Wire id of the selected watermark model; only used by the watermark source.
    pub(super) watermark_model: &'static str,
}

impl Default for MaskGenerationParams {
    fn default() -> Self {
        Self {
            source: MaskSource::ComicTextDetector,
            dilate_size: 7,
            watermark_model: DEFAULT_WATERMARK_MODEL,
        }
    }
}

/// Mask-generation state of one host: the shared parameters plus the watermark-source extras
/// (catalog download state and streaming progress).
///
/// The running job is deliberately NOT part of it: a host owns the receiver, because what a
/// finished job may still be written into is the host's own state and its lifetime is the
/// host's to decide.
#[derive(Debug)]
pub(super) struct MaskGenerationState {
    pub(super) params: MaskGenerationParams,
    watermark_status: Option<WatermarkStatus>,
    watermark_status_rx: Option<Receiver<Result<WatermarkStatus, String>>>,
    /// Arms exactly ONE `watermark.status` query: set initially and re-armed
    /// after a run that may have downloaded code or weights, cleared when the
    /// query is spawned. Without it a failing query would be retried on every
    /// frame, spawning a thread per frame.
    watermark_status_wanted: bool,
    pub(super) watermark_progress: Arc<Mutex<WatermarkProgress>>,
}

impl Default for MaskGenerationState {
    fn default() -> Self {
        Self {
            params: MaskGenerationParams::default(),
            watermark_status: None,
            watermark_status_rx: None,
            watermark_status_wanted: true,
            watermark_progress: Arc::new(Mutex::new(WatermarkProgress::default())),
        }
    }
}

impl MaskGenerationState {
    /// Re-arms the catalog query. Call it when starting a run that may download code or
    /// weights, so the ✓/«скачать» marks are refreshed once the run ends.
    pub(super) fn rearm_watermark_catalog(&mut self) {
        self.watermark_status_wanted = true;
    }

    /// Drains a finished catalog query and issues a new one when exactly one is due.
    ///
    /// Call once per frame from the host's UI pass. `busy` is the host's own "a generation is
    /// running" answer: the backend is never asked mid-download. At most one query is ever in
    /// flight, and a failed one is not retried until something re-arms it — otherwise a
    /// backend that refuses the method would cost one worker thread per rendered frame.
    pub(super) fn refresh_watermark_catalog(&mut self, backend_available: bool, busy: bool) {
        poll_watermark_status(&mut self.watermark_status, &mut self.watermark_status_rx);
        if self.watermark_status_wanted
            && self.params.source == MaskSource::Watermark
            && backend_available
            && !busy
            && self.watermark_status_rx.is_none()
        {
            self.watermark_status_wanted = false;
            self.watermark_status_rx = Some(spawn_watermark_status_query());
        }
    }
}

/// A validated detection answer: one byte per pixel, `0` (kept) or `255` (masked), row-major.
///
/// The buffer is binarized and its length is checked against `size` at construction, which is
/// what lets a consumer write it straight into an L8 mask layer.
#[derive(Clone)]
pub(super) struct GeneratedMask {
    size: [usize; 2],
    alpha: Vec<u8>,
}

// Hand-written: the payload is a megapixel buffer, and a derived `Debug` would dump all of it
// into a log line or a test failure. The geometry and the set-pixel count are what identify it.
impl std::fmt::Debug for GeneratedMask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedMask")
            .field("size", &self.size)
            .field("set_px", &self.alpha.iter().filter(|value| **value != 0).count())
            .finish()
    }
}

impl GeneratedMask {
    /// `[width, height]` of the mask in region pixels.
    #[must_use]
    pub(super) fn size(&self) -> [usize; 2] {
        self.size
    }

    /// The 0/255 alpha buffer, `width * height` bytes, row-major.
    #[must_use]
    pub(super) fn alpha(&self) -> &[u8] {
        &self.alpha
    }

    /// Builds a mask from parts without a backend round trip.
    ///
    /// TEST ONLY, and deliberately unchecked: a consumer's tests must be able to hand their
    /// host a mask of a DELIBERATELY wrong shape to prove the refusal, which the production
    /// constructor ([`generate_mask`]) makes impossible by validating first.
    #[cfg(test)]
    #[must_use]
    pub(super) fn from_parts_for_test(size: [usize; 2], alpha: Vec<u8>) -> Self {
        Self { size, alpha }
    }

    /// The same mask as an opaque-white-over-transparent `ColorImage`, which is how the
    /// detached region editors of `base.rs` store their mask.
    #[must_use]
    pub(super) fn to_color_image(&self) -> egui::ColorImage {
        let mut mask = egui::ColorImage::filled(self.size, Color32::TRANSPARENT);
        for (dst, alpha) in mask.pixels.iter_mut().zip(&self.alpha) {
            if *alpha != 0 {
                *dst = Color32::from_rgba_unmultiplied(255, 255, 255, 255);
            }
        }
        mask
    }
}

/// What one poll of a mask-generation job tells its host.
///
/// `Failed` already carries the FULL localized sentence to show the user: a backend failure
/// and a dead worker read differently, and folding them into one message at the call site is
/// how the two hosts drifted apart before this module existed.
#[derive(Debug)]
pub(super) enum MaskGenerationPoll {
    /// No job is in flight.
    Idle,
    /// A job is running; the host should keep repainting.
    Running,
    Done(GeneratedMask),
    Failed(String),
}

/// Starts a detection on a worker thread and returns its result channel.
///
/// `image` is moved into the worker: the detection encodes it to PNG, which must not happen on
/// the GUI thread. `progress` is written only by the watermark source.
#[must_use]
pub(super) fn spawn_mask_generation(
    image: egui::ColorImage,
    params: MaskGenerationParams,
    progress: Arc<Mutex<WatermarkProgress>>,
) -> Receiver<Result<GeneratedMask, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = generate_mask(&image, params, &progress);
        // The receiver is gone when the host abandoned the job; there is nothing to report.
        let _ = tx.send(result);
    });
    rx
}

/// Polls a running job, clearing `rx` on any terminal answer so the caller cannot poll a
/// finished job twice.
pub(super) fn poll_mask_generation(
    rx: &mut Option<Receiver<Result<GeneratedMask, String>>>,
) -> MaskGenerationPoll {
    let Some(receiver) = rx.as_ref() else {
        return MaskGenerationPoll::Idle;
    };
    match receiver.try_recv() {
        Ok(Ok(mask)) => {
            *rx = None;
            MaskGenerationPoll::Done(mask)
        }
        Ok(Err(err)) => {
            *rx = None;
            MaskGenerationPoll::Failed(tf!("cleaning.mask_editor.mask_gen_error", err = err))
        }
        Err(TryRecvError::Empty) => MaskGenerationPoll::Running,
        Err(TryRecvError::Disconnected) => {
            *rx = None;
            MaskGenerationPoll::Failed(
                t!("cleaning.mask_editor.mask_gen_thread_crashed_error").to_string(),
            )
        }
    }
}

/// Runs the selected mask source on `image` and returns a binary mask in REGION coordinates.
///
/// Blocking: it performs a backend round trip and must only ever be called from a worker
/// thread. `watermark_progress` is written only by the watermark source, whose first call
/// downloads network code and weights on the backend side.
///
/// # Errors
/// Returns a user-facing message when the backend fails, when the returned mask does not match
/// the region size, or when its buffer length is wrong.
pub(super) fn generate_mask(
    image: &egui::ColorImage,
    params: MaskGenerationParams,
    watermark_progress: &Arc<Mutex<WatermarkProgress>>,
) -> Result<GeneratedMask, String> {
    let method_label = params.source.label();
    let (mask_size, mask_alpha) = match params.source {
        MaskSource::ComicTextDetector => {
            let options = TextDetectorAiCtdOptions {
                mask_dilate_size: params.dilate_size,
                ..TextDetectorAiCtdOptions::default()
            };
            detect_ai_ctd_mask_for_image(image, &options)?
        }
        MaskSource::PaddleOcr => {
            let options = TextDetectorPaddleOcrOptions {
                mask_dilate_size: params.dilate_size,
            };
            detect_paddle_mask_for_image(image, &options)?
        }
        MaskSource::Surya => detect_surya_mask_for_image(image, params.dilate_size)?,
        MaskSource::Watermark => detect_watermark_mask(image, params, watermark_progress)?,
    };
    let mask_w = usize::try_from(mask_size[0])
        .map_err(|_| tf!("cleaning.mask_editor.mask_width_too_large_error", method_label = method_label))?;
    let mask_h = usize::try_from(mask_size[1])
        .map_err(|_| tf!("cleaning.mask_editor.mask_height_too_large_error", method_label = method_label))?;
    if [mask_w, mask_h] != image.size {
        return Err(tf!("cleaning.mask_editor.mask_size_error", method_label = method_label, mask_w = mask_w, mask_h = mask_h, image = image.size[0], image_2 = image.size[1]));
    }
    let expected_len = mask_w.saturating_mul(mask_h);
    if expected_len != mask_alpha.len() {
        return Err(tf!("cleaning.mask_editor.mask_length_error", method_label = method_label, actual_len = mask_alpha.len(), expected_len = expected_len));
    }
    // Binarize here rather than at each consumer: an L8 mask layer's invariant is that it
    // holds only 0 and 255, and a soft edge from a detector would break the set-pixel counter
    // that the region frame derives its lock from.
    let alpha = mask_alpha
        .into_iter()
        .map(|value| if value == 0 { 0u8 } else { 255u8 })
        .collect();
    Ok(GeneratedMask {
        size: [mask_w, mask_h],
        alpha,
    })
}

impl MaskGenerationState {
    /// Draws the source picker: a `WheelComboBox` over [`MASK_SOURCES`], with the Torch-gated
    /// entries disabled and explaining themselves on hover.
    ///
    /// `id_salt` must be stable across frames and unique per host instance — the labels are
    /// localized, so the popup's open/closed state would otherwise be lost on a language switch
    /// (`egui-docs/05-ids-and-i18n.md` §2).
    pub(super) fn draw_source_picker(
        &mut self,
        ui: &mut egui::Ui,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
        torch_available: bool,
    ) {
        let source = &mut self.params.source;
        WheelComboBox::from_id_salt(id_salt)
            .selected_text(source.label())
            .show_ui(ui, |ui| {
                for entry in MASK_SOURCES {
                    draw_mask_source_entry(ui, source, entry, torch_available);
                }
            });
    }

    /// Draws the parameters of the selected source: the shared mask dilation, and the model
    /// picker of the watermark source.
    pub(super) fn draw_source_params(&mut self, ui: &mut egui::Ui) {
        // Split borrow: the model picker writes the selection while reading the catalog
        // snapshot beside it, and both live in this struct.
        let Self {
            params,
            watermark_status,
            ..
        } = self;
        ui.add(
            WheelSlider::new(&mut params.dilate_size, MASK_DILATE_RANGE)
                .text(t!("cleaning.common.mask_expand_label")),
        );
        if params.source == MaskSource::Watermark {
            draw_watermark_model_picker_ui(ui, &mut params.watermark_model, watermark_status.as_ref());
        }
    }

    /// Draws the progress bar of a running watermark detection; a no-op for every other source
    /// and while the watermark source is idle.
    pub(super) fn draw_progress(&self, ui: &mut egui::Ui) {
        draw_watermark_progress_ui(ui, &self.watermark_progress);
    }
}

/// Draws one entry of the mask-source dropdown and applies a click to `selected`.
///
/// Torch-backed sources are disabled while the backend reports no Torch and
/// explain that on hover, matching the tab-level AI gating.
fn draw_mask_source_entry(
    ui: &mut egui::Ui,
    selected: &mut MaskSource,
    source: MaskSource,
    torch_available: bool,
) {
    let enabled = torch_available || !source.requires_torch();
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(source.label()).selected(*selected == source),
    );
    let response = if enabled {
        response
    } else {
        response.on_disabled_hover_text(
            egui::RichText::new(t!("cleaning.common.pytorch_not_installed_status"))
                .color(Color32::from_rgb(240, 102, 102)),
        )
    };
    if response.clicked() {
        *selected = source;
    }
}

/// Hover text of the «Сгенерировать маску» button: names the blocking condition
/// (backend offline, Torch missing) or describes what the selected source does.
pub(super) fn generate_button_hover_text(
    source: MaskSource,
    backend_available: bool,
    torch_available: bool,
) -> String {
    if !backend_available {
        return t!("cleaning.mask_editor.backend_unavailable_status").to_string();
    }
    if source.requires_torch() && !torch_available {
        return t!("cleaning.common.pytorch_not_installed_status").to_string();
    }
    match source {
        MaskSource::Watermark => t!("cleaning.tools.watermark.send_region_hint").to_string(),
        MaskSource::ComicTextDetector | MaskSource::PaddleOcr | MaskSource::Surya => {
            t!("cleaning.mask_editor.send_region_hint").to_string()
        }
    }
}

/// Model row of the watermark source: a `WheelComboBox` over the fixed catalog
/// with the ✓/«скачать» hint from `status`, plus the first-run download notice.
pub(super) fn draw_watermark_model_picker_ui(
    ui: &mut egui::Ui,
    selected_model: &mut &'static str,
    status: Option<&WatermarkStatus>,
) {
    ui.horizontal(|ui| {
        ui.label(t!("cleaning.tools.watermark.model_label"));
        let selected_text = watermark_model_label(watermark_model_spec(selected_model), status);
        WheelComboBox::from_id_salt("cleaning_watermark_model_picker")
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                for spec in WATERMARK_MODEL_SPECS {
                    let label = watermark_model_label(spec, status);
                    let _changed = ui.selectable_value(selected_model, spec.id, label).changed();
                }
            });
    });
    ui.small(t!("cleaning.tools.watermark.download_hint"));
}

/// Dropdown label of a watermark model: `✓` when the backend already has its code
/// and weights, a «скачать» hint when it does not, and the plain (localized) name
/// while no status snapshot has arrived yet.
fn watermark_model_label(spec: WatermarkModelSpec, status: Option<&WatermarkStatus>) -> String {
    let name = spec.display_name();
    match status {
        Some(status) if status.is_ready(spec.id) => format!("{name} ✓"),
        Some(_) => tf!("cleaning.tools.watermark.model_download_label", model = name),
        None => name.to_string(),
    }
}

/// Catalog entry for a wire model id, falling back to the default model when the
/// id is unknown (a stored selection from a newer catalog must not break the UI).
pub(super) fn watermark_model_spec(model_id: &str) -> WatermarkModelSpec {
    WATERMARK_MODEL_SPECS
        .iter()
        .copied()
        .find(|spec| spec.id == model_id)
        .unwrap_or(WATERMARK_MODEL_SPECS[0])
}

/// Draws the progress bar of a running watermark detection; a no-op while idle.
/// The download phase counts bytes, the generate phase counts steps.
pub(super) fn draw_watermark_progress_ui(ui: &mut egui::Ui, progress: &Mutex<WatermarkProgress>) {
    let (active, phase, step, total, label) = {
        let guard = lock_watermark_progress(progress);
        (
            guard.active,
            guard.phase.clone(),
            guard.step,
            guard.total,
            guard.label.clone(),
        )
    };
    if !active {
        return;
    }
    // Byte and step counters are far below 2^53, so the f64 conversion used for
    // the fraction and the MiB readout is exact.
    let fraction = if total > 0 {
        (step as f64 / total as f64).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let text = if phase == "download" {
        let done_mib = step as f64 / (1024.0 * 1024.0);
        let total_mib = total as f64 / (1024.0 * 1024.0);
        tf!(
            "cleaning.tools.watermark.download_progress_status",
            label = label,
            done = format!("{done_mib:.1}"),
            total = format!("{total_mib:.1}")
        )
    } else if total > 0 {
        tf!("cleaning.common.step_progress_status", step = step, total = total)
    } else {
        label
    };
    ui.add(egui::ProgressBar::new(fraction).text(text));
    ui.ctx().request_repaint();
}

/// Locks the shared watermark progress, recovering from a poisoned mutex: the
/// payload is display-only state, so a panicked writer must not kill the editor.
pub(super) fn lock_watermark_progress(
    progress: &Mutex<WatermarkProgress>,
) -> MutexGuard<'_, WatermarkProgress> {
    match progress.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

/// Polls the background `watermark.status` query. A failed query only clears the
/// receiver: the catalog then shows plain model names instead of ✓/«скачать».
pub(super) fn poll_watermark_status(
    status: &mut Option<WatermarkStatus>,
    status_rx: &mut Option<Receiver<Result<WatermarkStatus, String>>>,
) {
    let Some(rx) = status_rx.as_ref() else {
        return;
    };
    match rx.try_recv() {
        Ok(Ok(value)) => {
            *status = Some(value);
            *status_rx = None;
        }
        Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
            *status_rx = None;
        }
        Err(TryRecvError::Empty) => {}
    }
}

/// Spawns the `watermark.status` query on a worker thread (it is a blocking IPC
/// call and must never run on the GUI thread) and returns its result channel.
pub(super) fn spawn_watermark_status_query() -> Receiver<Result<WatermarkStatus, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(fetch_watermark_status());
    });
    rx
}

/// Queries `watermark.status` for the catalog download state.
///
/// # Errors
/// Returns the backend error message, or the unified offline message when the
/// backend cannot be reached.
fn fetch_watermark_status() -> Result<WatermarkStatus, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (header, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_WATERMARK_STATUS,
            json!({}),
            &[],
            WATERMARK_STATUS_CALL_TIMEOUT,
        )
        .map_err(map_watermark_call_error)?;
    Ok(WatermarkStatus {
        downloaded_models: watermark_status_string_list(&header, "downloaded_models"),
        code_ready_models: watermark_status_string_list(&header, "code_ready_models"),
    })
}

/// Reads a string array field of a response header, tolerating a missing field
/// (older backend) and skipping non-string entries.
fn watermark_status_string_list(header: &Value, field: &str) -> Vec<String> {
    header
        .get(field)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// Request header of a `watermark.detect` call. `model` is the wire id of the
/// catalog entry; `dilate_px` reuses the editor's shared mask-expansion control,
/// while the detection scale and threshold stay at the backend defaults (their
/// controls belong to the dedicated watermark tool).
fn watermark_detect_header(params: MaskGenerationParams) -> Value {
    json!({
        "params": {
            "model": params.watermark_model,
            "downscale_to": WATERMARK_DETECT_DOWNSCALE_TO,
            "threshold": WATERMARK_DETECT_THRESHOLD,
            "dilate_px": params.dilate_size.clamp(0, 30),
        }
    })
}

/// Streams a `watermark.detect` call for `image` and returns the predicted mask
/// as `([w, h], alpha)` in region pixel coordinates (alpha is 0/255).
///
/// The response blob is an L8 PNG at the input resolution. `progress` is updated
/// from the `progress` frames and is always cleared before returning, including
/// on failure, so the editor never keeps a stuck progress bar.
///
/// # Errors
/// Returns the backend error message, the abort notice for an interrupted call,
/// or the unified offline message when the transport fails.
fn detect_watermark_mask(
    image: &egui::ColorImage,
    params: MaskGenerationParams,
    progress: &Arc<Mutex<WatermarkProgress>>,
) -> Result<([u32; 2], Vec<u8>), String> {
    if image.size[0] == 0 || image.size[1] == 0 {
        return Ok(([0, 0], Vec::new()));
    }
    let image_png = encode_color_image_png_rgba(image)?;
    let header = watermark_detect_header(params);

    {
        let mut guard = lock_watermark_progress(progress);
        guard.active = true;
        guard.phase = "generate".to_string();
        guard.step = 0;
        guard.total = 0;
        guard.label = t!("cleaning.tools.watermark.preparing_status").to_string();
    }
    let stream_result =
        watermark_detect_stream_call(header, &image_png, |phase, step, total, label| {
            let mut guard = lock_watermark_progress(progress);
            guard.phase = phase;
            guard.step = step;
            guard.total = total;
            guard.label = label;
        });
    {
        let mut guard = lock_watermark_progress(progress);
        guard.active = false;
    }

    let (_response_header, mask_blob) = stream_result?;
    if mask_blob.is_empty() {
        return Err(t!("cleaning.tools.watermark.no_mask_result_error").to_string());
    }
    parse_mask_alpha_from_blob(&mask_blob)
}

/// Issues the streaming `watermark.detect` request. Each `progress` frame carries
/// `phase`/`step`/`total`/`label` in its header and no blob.
fn watermark_detect_stream_call<F>(
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
            backend_ipc::protocol::METHOD_WATERMARK_DETECT,
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
            WATERMARK_DETECT_CALL_TIMEOUT,
        )
        .map_err(map_watermark_call_error)
}

/// `CallError` → user-facing message for watermark calls, mirroring the inpaint
/// tools: backend errors verbatim, an abort notice for an interrupt, and the
/// unified offline message for a transport failure.
pub(super) fn map_watermark_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => tf!("cleaning.inpaint.request_aborted_error", msg = msg),
        CallError::Transport(_) => ai_backend_offline_error().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only PaddleOCR runs without Torch; every other mask source is gated on it.
    #[test]
    fn mask_source_torch_requirements() {
        assert!(!MaskSource::PaddleOcr.requires_torch());
        assert!(MaskSource::Surya.requires_torch());
        assert!(MaskSource::ComicTextDetector.requires_torch());
        assert!(MaskSource::Watermark.requires_torch());
    }

    /// The catalog offers every source exactly once, so no host can present a partial list.
    #[test]
    fn the_source_catalog_is_complete_and_free_of_duplicates() {
        assert_eq!(MASK_SOURCES.len(), 4);
        for source in MASK_SOURCES {
            assert_eq!(
                MASK_SOURCES.iter().filter(|other| **other == source).count(),
                1,
                "{source:?} appears more than once in the catalog"
            );
        }
        assert!(MASK_SOURCES.contains(&MaskSource::PaddleOcr));
        assert!(MASK_SOURCES.contains(&MaskSource::Surya));
        assert!(MASK_SOURCES.contains(&MaskSource::ComicTextDetector));
        assert!(MASK_SOURCES.contains(&MaskSource::Watermark));
    }

    /// Availability needs the backend in every case and Torch only where the source does.
    #[test]
    fn availability_needs_the_backend_and_torch_where_required() {
        assert!(!MaskSource::PaddleOcr.is_available(false, true));
        assert!(MaskSource::PaddleOcr.is_available(true, false));
        assert!(!MaskSource::Surya.is_available(true, false));
        assert!(MaskSource::Surya.is_available(true, true));
        assert!(!MaskSource::Watermark.is_available(false, true));
    }

    /// The generate-button tooltip names the blocking condition first (backend,
    /// then Torch) and otherwise describes the selected source.
    #[test]
    fn generate_hover_text_names_the_blocker() {
        assert_eq!(
            generate_button_hover_text(MaskSource::Watermark, false, true),
            t!("cleaning.mask_editor.backend_unavailable_status")
        );
        assert_eq!(
            generate_button_hover_text(MaskSource::Watermark, true, false),
            t!("cleaning.common.pytorch_not_installed_status")
        );
        assert_eq!(
            generate_button_hover_text(MaskSource::Watermark, true, true),
            t!("cleaning.tools.watermark.send_region_hint")
        );
        assert_eq!(
            generate_button_hover_text(MaskSource::PaddleOcr, true, false),
            t!("cleaning.mask_editor.send_region_hint")
        );
    }

    /// The default parameters name a source that exists and a model in the catalog.
    #[test]
    fn default_params_are_in_the_catalogs() {
        let params = MaskGenerationParams::default();
        assert!(MASK_SOURCES.contains(&params.source));
        assert_eq!(params.watermark_model, DEFAULT_WATERMARK_MODEL);
        assert_eq!(watermark_model_spec(params.watermark_model).id, DEFAULT_WATERMARK_MODEL);
        assert!(MASK_DILATE_RANGE.contains(&params.dilate_size));
    }

    /// A generated mask keeps its NON-SQUARE geometry: the buffer is exactly `w * h` bytes,
    /// row-major, and the `ColorImage` conversion puts every set pixel back at its own index.
    /// A width/height swap would pass a length check and corrupt every row, so the test pins
    /// one pixel whose row and column differ.
    #[test]
    fn a_generated_mask_keeps_its_shape_and_stride() {
        let (w, h) = (5usize, 3usize);
        let mut alpha = vec![0u8; w * h];
        // Row 2, column 1 — distinct indices, so a transposed buffer lands elsewhere.
        alpha[2 * w + 1] = 200;
        let mask = GeneratedMask {
            size: [w, h],
            alpha: alpha.iter().map(|v| if *v == 0 { 0 } else { 255 }).collect(),
        };
        assert_eq!(mask.size(), [w, h]);
        assert_eq!(mask.alpha().len(), w * h);
        assert_eq!(mask.alpha()[2 * w + 1], 255);
        let image = mask.to_color_image();
        assert_eq!(image.size, [w, h]);
        assert_eq!(image.pixels.len(), w * h);
        assert_eq!(image.pixels[2 * w + 1].a(), 255);
        assert_eq!(image.pixels[2 * w + 2].a(), 0);
    }

    /// A poll of an empty slot is `Idle`, and a dropped worker becomes the crashed-thread
    /// sentence rather than an error wrapped as a backend failure.
    #[test]
    fn polling_reports_idle_and_a_dead_worker() {
        let mut rx: Option<Receiver<Result<GeneratedMask, String>>> = None;
        assert!(matches!(poll_mask_generation(&mut rx), MaskGenerationPoll::Idle));

        let (tx, receiver) = mpsc::channel::<Result<GeneratedMask, String>>();
        drop(tx);
        let mut rx = Some(receiver);
        match poll_mask_generation(&mut rx) {
            MaskGenerationPoll::Failed(text) => {
                assert_eq!(text, t!("cleaning.mask_editor.mask_gen_thread_crashed_error"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(rx.is_none(), "a terminal poll must clear the receiver");
    }

    /// A backend failure reaches the host as the localized generation-error sentence, and the
    /// receiver is cleared so the job cannot be polled twice.
    #[test]
    fn a_backend_failure_becomes_the_generation_error_sentence() {
        let (tx, receiver) = mpsc::channel::<Result<GeneratedMask, String>>();
        tx.send(Err("boom".to_string())).expect("the receiver is alive");
        let mut rx = Some(receiver);
        match poll_mask_generation(&mut rx) {
            MaskGenerationPoll::Failed(text) => {
                assert_eq!(text, tf!("cleaning.mask_editor.mask_gen_error", err = "boom"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(rx.is_none());
        assert!(matches!(poll_mask_generation(&mut rx), MaskGenerationPoll::Idle));
    }

    /// `watermark.status` fields are optional and may hold non-string entries;
    /// neither may break the catalog labels.
    #[test]
    fn watermark_status_list_tolerates_missing_and_garbage() {
        let header = json!({ "downloaded_models": ["slbr", 7, null, "wdnet"] });
        assert_eq!(
            watermark_status_string_list(&header, "downloaded_models"),
            vec!["slbr".to_string(), "wdnet".to_string()]
        );
        assert!(watermark_status_string_list(&header, "code_ready_models").is_empty());
    }

    /// Watermark call errors keep the backend message verbatim, label an
    /// interrupt, and collapse a transport failure into the offline message.
    #[test]
    fn watermark_call_error_mapping_preserves_messages() {
        assert_eq!(
            map_watermark_call_error(CallError::Error("boom".to_string())),
            "boom"
        );
        assert_eq!(
            map_watermark_call_error(CallError::Interrupted("MARKER".to_string())),
            tf!("cleaning.inpaint.request_aborted_error", msg = "MARKER")
        );
        assert_eq!(
            map_watermark_call_error(CallError::Transport("dead".to_string())),
            ai_backend_offline_error()
        );
    }

    /// Watermark catalog ids are WIRE values sent as `params.model` and are the persisted
    /// selection identity: they must stay these exact literals, the default selection must be
    /// one of them, and an id from a newer build must fall back to the default rather than
    /// yielding an empty label.
    #[test]
    fn watermark_catalog_ids_are_wire_literals() {
        let ids: Vec<&str> = WATERMARK_MODEL_SPECS.iter().map(|spec| spec.id).collect();
        assert_eq!(ids, vec!["slbr", "wdnet", "splitnet"]);
        assert_eq!(DEFAULT_WATERMARK_MODEL, "slbr");
        assert_eq!(watermark_model_spec("wdnet").id, "wdnet");
        assert_eq!(watermark_model_spec("nope").id, DEFAULT_WATERMARK_MODEL);
    }

    /// The dropdown label marks a ready model with ✓, offers the download hint
    /// when either the weights or the network code are still missing, and stays
    /// plain while no status snapshot has arrived.
    #[test]
    fn watermark_model_label_marks_downloaded() {
        let spec = watermark_model_spec("slbr");
        let name = spec.display_name();
        let ready = WatermarkStatus {
            downloaded_models: vec!["slbr".to_string()],
            code_ready_models: vec!["slbr".to_string()],
        };
        assert_eq!(
            watermark_model_label(spec, Some(&ready)),
            format!("{name} ✓")
        );
        // Weights present but code missing still means a download on the next run.
        let code_missing = WatermarkStatus {
            downloaded_models: vec!["slbr".to_string()],
            code_ready_models: Vec::new(),
        };
        assert_eq!(
            watermark_model_label(spec, Some(&code_missing)),
            tf!("cleaning.tools.watermark.model_download_label", model = name)
        );
        assert_eq!(watermark_model_label(spec, None), name.to_string());
    }

    /// The detect header carries the wire model id plus the fixed detection
    /// parameters, with the editor's shared dilation control clamped to the
    /// slider range the backend expects.
    #[test]
    fn watermark_detect_header_shape() {
        let params = MaskGenerationParams {
            source: MaskSource::Watermark,
            dilate_size: 99,
            watermark_model: "splitnet",
        };
        let header = watermark_detect_header(params);
        assert!(header["params"].is_object(), "params must be an object");
        assert_eq!(header["params"]["model"].as_str(), Some("splitnet"));
        assert_eq!(header["params"]["downscale_to"].as_u64(), Some(512));
        assert_eq!(header["params"]["dilate_px"].as_i64(), Some(30));
        let threshold = header["params"]["threshold"].as_f64().unwrap_or(-1.0);
        assert!((threshold - 0.5).abs() < 1e-6, "threshold was {threshold}");
    }
}
