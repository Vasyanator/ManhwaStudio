/*
FILE OVERVIEW: crates/ms-tab-translation/src/panels/text_detector.rs
Translation panel UI for text detection controls.

Main types:
- `TextDetectorAlgorithm`: detector backend mode selector (`Classic` /
  `PaddleOcr` / `Ai` / `Surya`).
- `TextDetectorPanelOptions`: editable UI options for detector run.
- `TextDetectorPanelActions`: one-frame UI actions returned to tab logic.
- `TextDetectorPanelView`: read-only per-frame snapshot the tab lends the panel.
- `TextDetectorPlanNoticeCache`: the current page's `PlanNotice`, recomputed only when the
  engine, its params or the page size change (`plan_detection` allocates its tile list).
- `DetectorUnavailable`: why the selected algorithm cannot run; produced only by
  `TextDetectorAlgorithm::availability`, the one owner of the "can detect" decision
  (panel button gate, panel hint and the tab's run-mode check all route through it).

Flow:
- `draw_text_detector_panel(ui, options, plan_notices, view)`: renders status, options, the
  current page's plan notice (`plan_notice_text`, planned through the cache after the option
  widgets ran) and action buttons. The Save button is drawn only while
  `view.storage_available` (false in a single-image session).
- `TextDetectorPanelOptions::run_mode()` -> `TextDetectorRunMode::plan_inputs()` ->
  `ms_text_detect::plan_detection` is the one chain both the notice and the worker use, so
  the notice announces exactly the resize / tiling the run will do.
- Algorithm selection uses frameless [`AiButton`] toggles (one per algorithm, wrapped
  ~3 per row) that self-gate on each algorithm's runtime capability and show a runtime
  marker; `Classic` has no runtime dependency and is a plain selectable.
- AI mode advanced params hold the CTD detection size (the plan's tile side); shared mask
  dilation lives in the common section with other detector-wide options; device selection
  is configured globally in `Настройки -> ИИ бэкенд`.
- Строка кнопок под чекбоксами: вход/выход из режима редактирования строк
  детектора и режима редактирования маски.
*/

use crate::text_detector::{
    TextDetectorAiCtdOptions, TextDetectorPaddleOcrOptions, TextDetectorRunMode, TextDetectorSuryaOptions,
};
use ms_models::page_view::{PageImageInfo, SourcePageLoadState};
use ms_text_detect::{DetectParams, EngineKind, PlanMode, PlanNotice, plan_detection};
use ms_widgets::{AiButton, AiRequirement, WheelSpinBox};

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum TextDetectorAlgorithm {
    #[default]
    Classic,
    PaddleOcr,
    Ai,
    Surya,
}

/// Selectable detector algorithms in display order (wrapped ~3 per row).
const DETECTOR_ALGORITHMS: [TextDetectorAlgorithm; 4] = [
    TextDetectorAlgorithm::Classic,
    TextDetectorAlgorithm::PaddleOcr,
    TextDetectorAlgorithm::Ai,
    TextDetectorAlgorithm::Surya,
];

impl TextDetectorAlgorithm {
    pub fn key(self) -> &'static str {
        match self {
            TextDetectorAlgorithm::Classic => "classic",
            TextDetectorAlgorithm::PaddleOcr => "paddleocr",
            TextDetectorAlgorithm::Ai => "ai",
            TextDetectorAlgorithm::Surya => "surya",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            TextDetectorAlgorithm::Classic => t!("translation.text_detector_panel.algo_classic"),
            TextDetectorAlgorithm::PaddleOcr => "PaddleOCR",
            TextDetectorAlgorithm::Ai => "ComicTextDetector",
            TextDetectorAlgorithm::Surya => "Surya",
        }
    }

    /// The one "can this algorithm run now" decision. `Classic` is a local heuristic
    /// and is always available; every other algorithm needs AI enabled (no `--no-ai`);
    /// the PyTorch detectors (`Ai` = ComicTextDetector, `Surya`) additionally fail only
    /// when the backend has REPORTED PyTorch missing (`Some(false)`) — an unknown
    /// capability (`None`) is permissive so a not-yet-probed backend does not lock them
    /// out. `ai_enabled` is checked first, so `--no-ai` always wins over `TorchMissing`.
    pub fn availability(
        self,
        ai_enabled: bool,
        torch_available: Option<bool>,
    ) -> Result<(), DetectorUnavailable> {
        let needs_torch = match self {
            TextDetectorAlgorithm::Classic => return Ok(()),
            TextDetectorAlgorithm::PaddleOcr => false,
            TextDetectorAlgorithm::Ai | TextDetectorAlgorithm::Surya => true,
        };
        if !ai_enabled {
            return Err(DetectorUnavailable::AiDisabled);
        }
        if needs_torch && torch_available == Some(false) {
            return Err(DetectorUnavailable::TorchMissing);
        }
        Ok(())
    }

    /// Localized "disabled by `--no-ai`" status for this algorithm. `Classic` is never
    /// unavailable (see [`Self::availability`]); its arm only keeps the match total and
    /// reuses the generic AI-detector text.
    fn ai_disabled_status(self) -> &'static str {
        match self {
            TextDetectorAlgorithm::PaddleOcr => t!("translation.text_detector.paddle_disabled_status"),
            TextDetectorAlgorithm::Classic | TextDetectorAlgorithm::Ai => {
                t!("translation.text_detector.ai_disabled_status")
            }
            TextDetectorAlgorithm::Surya => t!("translation.text_detector.surya_disabled_status"),
        }
    }
}

/// Why the selected detector algorithm cannot run; returned only by
/// [`TextDetectorAlgorithm::availability`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DetectorUnavailable {
    /// AI features are disabled for this session (`--no-ai`).
    AiDisabled,
    /// The algorithm runs on PyTorch and the backend reported PyTorch missing.
    TorchMissing,
}

impl DetectorUnavailable {
    /// Localized error text for a refused detection run (the tab's run-mode check):
    /// the algorithm's `--no-ai` text, or the shared "PyTorch is not installed" text.
    pub fn status_text(self, algorithm: TextDetectorAlgorithm) -> &'static str {
        match self {
            DetectorUnavailable::AiDisabled => algorithm.ai_disabled_status(),
            DetectorUnavailable::TorchMissing => t!("translation.common.pytorch_not_installed_status"),
        }
    }

    /// Localized hint shown under the panel's disabled detect buttons, or `None` for
    /// `Classic`. It names the real reason, the same text a refused run reports
    /// ([`Self::status_text`]): the algorithm's `--no-ai` text, or "PyTorch is not installed".
    ///
    /// `Classic` never yields a hint even when given a reason: the tab computes the
    /// availability BEFORE the panel's algorithm selector runs, so on the frame the user
    /// switches to `Classic` the stale reason meets the new algorithm, and no hint must show.
    fn panel_hint_text(self, algorithm: TextDetectorAlgorithm) -> Option<&'static str> {
        match algorithm {
            TextDetectorAlgorithm::Classic => None,
            TextDetectorAlgorithm::PaddleOcr | TextDetectorAlgorithm::Ai | TextDetectorAlgorithm::Surya => {
                Some(self.status_text(algorithm))
            }
        }
    }
}

/// Runtime capability an algorithm's selection button gates on, or `None` when it
/// has no local-runtime dependency (`Classic` is a pure local heuristic).
/// `PaddleOCR` runs on onnxruntime (native or backend); `ComicTextDetector` and
/// `Surya` run on PyTorch in the backend.
fn algorithm_requirement(algorithm: TextDetectorAlgorithm) -> Option<AiRequirement> {
    match algorithm {
        TextDetectorAlgorithm::Classic => None,
        TextDetectorAlgorithm::PaddleOcr => Some(AiRequirement::Onnx),
        TextDetectorAlgorithm::Ai => Some(AiRequirement::Torch),
        TextDetectorAlgorithm::Surya => Some(AiRequirement::Torch),
    }
}

/// Short runtime marker badge for an algorithm ("Torch"/"ONNX"), or `None` for the
/// dependency-free `Classic` algorithm.
fn algorithm_marker(algorithm: TextDetectorAlgorithm) -> Option<&'static str> {
    match algorithm {
        TextDetectorAlgorithm::Classic => None,
        TextDetectorAlgorithm::PaddleOcr => Some("ONNX"),
        TextDetectorAlgorithm::Ai => Some("Torch"),
        TextDetectorAlgorithm::Surya => Some("Torch"),
    }
}

/// Descriptive hover text for an algorithm (moved off the removed "Алгоритм:" label
/// onto each button).
fn algorithm_hover(algorithm: TextDetectorAlgorithm) -> &'static str {
    match algorithm {
        TextDetectorAlgorithm::Classic => t!("translation.text_detector_panel.algo_classic_hint"),
        TextDetectorAlgorithm::PaddleOcr => {
            t!("translation.text_detector_panel.algo_paddle_hint")
        }
        TextDetectorAlgorithm::Ai => {
            t!("translation.text_detector_panel.algo_ctd_hint")
        }
        TextDetectorAlgorithm::Surya => {
            t!("translation.text_detector_panel.algo_surya_hint")
        }
    }
}

/// Renders one detector-algorithm selection button. Runtime algorithms use a
/// frameless [`AiButton`] that self-gates on [`algorithm_requirement`] and shows a
/// runtime marker; `Classic` (no requirement) is a plain frameless selectable.
/// Returns `true` when the click selected this algorithm.
fn algorithm_select_button(
    ui: &mut egui::Ui,
    selected: &mut TextDetectorAlgorithm,
    algorithm: TextDetectorAlgorithm,
) -> bool {
    let is_selected = *selected == algorithm;
    let response = match algorithm_requirement(algorithm) {
        Some(requirement) => {
            let mut btn = AiButton::new(algorithm.title(), requirement)
                .selected(is_selected)
                .frame(false);
            if let Some(marker) = algorithm_marker(algorithm) {
                btn = btn.marker(marker);
            }
            btn.draw(ui).response
        }
        None => ui.selectable_label(is_selected, algorithm.title()),
    };
    let response = response.on_hover_text(algorithm_hover(algorithm));
    if response.clicked() {
        *selected = algorithm;
        return true;
    }
    false
}

#[derive(Debug, Clone)]
pub struct TextDetectorPanelOptions {
    pub algorithm: TextDetectorAlgorithm,
    pub draw_lines: bool,
    pub draw_mask: bool,
    pub block_expand_px: i32,
    pub mask_dilate_size: i32,
    pub merge_gap_px: i32,
    pub ai_detect_size: i32,
}

impl Default for TextDetectorPanelOptions {
    fn default() -> Self {
        Self {
            algorithm: TextDetectorAlgorithm::Classic,
            draw_lines: true,
            draw_mask: true,
            block_expand_px: 0,
            mask_dilate_size: 2,
            merge_gap_px: 5,
            ai_detect_size: 1280,
        }
    }
}

impl TextDetectorPanelOptions {
    /// The run mode these options select, WITHOUT the availability gate (the tab checks
    /// [`TextDetectorAlgorithm::availability`] before starting a run). The panel's plan notice
    /// and the tab's run both build their mode here, so they plan with the same inputs.
    /// Page-batch dilation is passed to the worker separately, so the CTD options carry 0.
    #[must_use]
    pub fn run_mode(&self) -> TextDetectorRunMode {
        match self.algorithm {
            TextDetectorAlgorithm::Classic => TextDetectorRunMode::Classic,
            TextDetectorAlgorithm::PaddleOcr => TextDetectorRunMode::PaddleOcr(TextDetectorPaddleOcrOptions::default()),
            TextDetectorAlgorithm::Ai => TextDetectorRunMode::AiCtd(TextDetectorAiCtdOptions {
                detect_size: self.ai_detect_size,
                mask_dilate_size: 0,
            }),
            TextDetectorAlgorithm::Surya => TextDetectorRunMode::Surya(TextDetectorSuryaOptions),
        }
    }
}

/// The source size `[w, h]` of a page whose dimensions are known (decoded, non-zero), or
/// `None` while it is loading, failed to decode or is absent from the page map.
#[must_use]
pub fn detector_page_size(info: Option<&PageImageInfo>) -> Option<[u32; 2]> {
    let info = info?;
    (info.load_state == SourcePageLoadState::Available && info.width_px > 0 && info.height_px > 0)
        .then_some([info.width_px, info.height_px])
}

/// Caches the current page's [`PlanNotice`] across frames. `plan_detection` is pure but
/// allocates its tile list, so the panel replans only when the engine, its params or the page
/// size change. Owned by the tab next to the panel options; holds no I/O state.
#[derive(Debug, Default)]
pub struct TextDetectorPlanNoticeCache {
    /// Inputs of the cached result; `None` until the first known page size.
    key: Option<(EngineKind, DetectParams, [u32; 2])>,
    /// The notice for `key`, or `None` when the plan refused the page.
    notice: Option<PlanNotice>,
}

impl TextDetectorPlanNoticeCache {
    /// The plan notice for `options` on a page of `page_size` (`None` = size unknown: no
    /// notice). A page the plan refuses (too large) also yields `None`; the refusal is logged
    /// once per input change, and the run reports it as a page error.
    pub fn notice(&mut self, options: &TextDetectorPanelOptions, page_size: Option<[u32; 2]>) -> Option<PlanNotice> {
        let size = page_size?;
        let (engine, params) = options.run_mode().plan_inputs();
        let key = (engine, params, size);
        if self.key != Some(key) {
            self.key = Some(key);
            self.notice = match plan_detection(engine, size, &params) {
                Ok(plan) => Some(plan.notice()),
                Err(err) => {
                    ms_log::runtime_log::log_info(format!(
                        "[text-detector] panel notice: no plan for {engine:?} on a {}x{} page: {err}",
                        size[0], size[1]
                    ));
                    None
                }
            };
        }
        self.notice
    }
}

/// The localized scale of a plan: one percentage, or width x height percentages when the
/// engine scales the axes differently (Surya squeezes pages like its library).
fn plan_scale_text(notice: &PlanNotice) -> String {
    let [x, y] = notice.scale_percent;
    if x == y {
        tf!("translation.text_detector_panel.plan_scale_label", percent = x)
    } else {
        tf!("translation.text_detector_panel.plan_scale_axes_label", x = x, y = y)
    }
}

/// The localized one-line summary of how the current page will be processed: full
/// resolution, resized (with the scale and scaled size), tiled (with the tile count, grid and
/// tile size), or both. Pure formatting of the plan's notice; computes nothing.
#[must_use]
pub fn plan_notice_text(notice: &PlanNotice) -> String {
    let [w, h] = notice.scaled_size;
    let [tile_w, tile_h] = notice.tile_size;
    match notice.mode {
        PlanMode::FullResolution => tf!("translation.text_detector_panel.plan_full_resolution_status", w = w, h = h),
        PlanMode::Resized => tf!(
            "translation.text_detector_panel.plan_resized_status",
            scale = plan_scale_text(notice),
            w = w,
            h = h
        ),
        PlanMode::Tiled => tf!(
            "translation.text_detector_panel.plan_tiled_status",
            tiles = tp!("translation.text_detector_panel.plan_tiles_label", notice.tiles),
            cols = notice.cols,
            rows = notice.rows,
            tile_w = tile_w,
            tile_h = tile_h
        ),
        PlanMode::ResizedAndTiled => tf!(
            "translation.text_detector_panel.plan_resized_tiled_status",
            scale = plan_scale_text(notice),
            w = w,
            h = h,
            tiles = tp!("translation.text_detector_panel.plan_tiles_label", notice.tiles),
            cols = notice.cols,
            rows = notice.rows,
            tile_w = tile_w,
            tile_h = tile_h
        ),
    }
}

/// The localized effective CTD detection size line (the requested size clamped and snapped
/// to a multiple of 64 by the plan), or `None` for engines without one.
#[must_use]
pub fn plan_ctd_size_text(notice: &PlanNotice) -> Option<String> {
    notice
        .ctd_detect_size
        .map(|size| tf!("translation.text_detector_panel.plan_effective_size_hint", size = size))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TextDetectorPanelActions {
    pub detect_current: bool,
    pub detect_all: bool,
    pub ocr_current: bool,
    pub ocr_all: bool,
    pub save_results: bool,
    pub clear_results: bool,
    pub toggle_edit_lines_mode: bool,
    pub toggle_edit_mask_mode: bool,
    pub options_changed: bool,
}

/// Read-only per-frame state the tab lends to [`draw_text_detector_panel`]. Every
/// field is a snapshot computed by `tab.rs`; the panel never reaches back into the tab.
#[derive(Debug, Clone, Copy)]
pub struct TextDetectorPanelView<'a> {
    /// Detector status line and its severity colour.
    pub status_text: &'a str,
    pub status_color: egui::Color32,
    /// `(done, total)` pages of the running detection, shown when `total > 0`.
    pub progress: Option<(usize, usize)>,
    pub detect_busy: bool,
    pub ocr_busy: bool,
    pub has_pages: bool,
    /// Result of [`TextDetectorAlgorithm::availability`] for the selected algorithm.
    pub availability: Result<(), DetectorUnavailable>,
    pub can_ocr_current: bool,
    pub can_ocr_all: bool,
    pub can_save: bool,
    /// Whether the chapter's detection storage exists for this session. `false` in a
    /// single-image session, whose scratch chapter is deleted on exit: the Save button is not
    /// drawn at all there.
    pub storage_available: bool,
    pub edit_lines_mode: bool,
    pub edit_mask_mode: bool,
    /// Size of the current page (`detector_page_size`), or `None` while unknown. The panel
    /// plans its notice from it AFTER the option widgets ran, so the frame of an algorithm or
    /// detect-size change already shows the new plan.
    pub page_size: Option<[u32; 2]>,
}

/// Draws the text-detector side panel: status, algorithm selector, display and
/// post-processing options, CTD advanced params, the current page's plan notice, and the
/// detect/save/clear/OCR buttons.
/// Edits `options` in place and returns this frame's user actions; starts no work itself.
/// `plan_notices` is the tab-owned notice cache, queried with the options as edited this frame.
pub fn draw_text_detector_panel(
    ui: &mut egui::Ui,
    options: &mut TextDetectorPanelOptions,
    plan_notices: &mut TextDetectorPlanNoticeCache,
    view: &TextDetectorPanelView<'_>,
) -> TextDetectorPanelActions {
    let TextDetectorPanelView {
        status_text,
        status_color,
        progress,
        detect_busy,
        ocr_busy,
        has_pages,
        availability,
        can_ocr_current,
        can_ocr_all,
        can_save,
        storage_available,
        edit_lines_mode,
        edit_mask_mode,
        page_size,
    } = *view;
    let mut actions = TextDetectorPanelActions::default();

    ui.heading(t!("translation.text_detector.title"));
    ui.colored_label(status_color, status_text);
    if let Some((done, total)) = progress
        && total > 0
    {
        ui.small(format!("{done} / {total}"));
    }
    ui.separator();

    // Algorithm selector: frameless self-gating toggle buttons (no "Алгоритм:"
    // label so ~3 fit per row). Each runtime algorithm shows its marker and disables
    // with its reason when unavailable; the currently-selected disabled algorithm's
    // action is additionally covered by the `availability` hint below.
    ui.horizontal_wrapped(|ui| {
        for algorithm in DETECTOR_ALGORITHMS {
            if algorithm_select_button(ui, &mut options.algorithm, algorithm) {
                actions.options_changed = true;
            }
        }
    });

    actions.options_changed |= ui
        .checkbox(&mut options.draw_lines, t!("translation.text_detector_panel.show_blocks_label"))
        .on_hover_text(t!("translation.text_detector_panel.show_blocks_hint"))
        .changed();
    actions.options_changed |= ui
    .checkbox(&mut options.draw_mask, t!("translation.text_detector_panel.show_mask_label"))
    .on_hover_text(t!("translation.text_detector_panel.show_mask_hint"))
    .changed();
    ui.horizontal(|ui| {
        let edit_lines_label = if edit_lines_mode {
            t!("translation.text_detector_panel.exit_edit_lines_button")
        } else {
            t!("translation.text_detector_panel.edit_lines_button")
        };
        if ui.button(edit_lines_label).clicked() {
            actions.toggle_edit_lines_mode = true;
        }
    });
    ui.horizontal(|ui| {
        let edit_mask_label = if edit_mask_mode {
            t!("translation.text_detector_panel.exit_edit_mask_button")
        } else {
            t!("translation.text_detector_panel.edit_mask_button")
        };
        if ui.button(edit_mask_label).clicked() {
            actions.toggle_edit_mask_mode = true;
        }
    });

    ui.horizontal(|ui| {
        ui.label(t!("translation.text_detector_panel.block_expand_label"))
        .on_hover_text(t!("translation.text_detector_panel.block_expand_hint"));
        let mut value = options.block_expand_px;
        if ui
            .add(WheelSpinBox::new(&mut value).range(0..=200).speed(0.2))
            .changed()
        {
            options.block_expand_px = value.clamp(0, 200);
            actions.options_changed = true;
        }
    });

    ui.horizontal(|ui| {
        ui.label(t!("translation.text_detector_panel.mask_expand_label"))
            .on_hover_text(t!("translation.text_detector_panel.mask_expand_hint"));
        let mut value = options.mask_dilate_size;
        if ui
            .add(WheelSpinBox::new(&mut value).range(0..=30).speed(0.2))
            .changed()
        {
            options.mask_dilate_size = value.clamp(0, 30);
            actions.options_changed = true;
        }
    });

    ui.horizontal(|ui| {
        ui.label(t!("translation.text_detector_panel.merge_distance_label"))
        .on_hover_text(t!("translation.text_detector_panel.merge_distance_hint"));
        let mut value = options.merge_gap_px;
        if ui
            .add(WheelSpinBox::new(&mut value).range(0..=200).speed(0.2))
            .changed()
        {
            options.merge_gap_px = value.clamp(0, 200);
            actions.options_changed = true;
        }
    });

    if options.algorithm == TextDetectorAlgorithm::Ai {
        ui.separator();
        // The i18n key as salt keeps the collapsed state stable across UI-language switches.
        egui::CollapsingHeader::new(t!("translation.text_detector_panel.advanced_heading"))
            .id_salt("translation.text_detector_panel.advanced_heading")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(t!("translation.text_detector_panel.detection_size_label"));
                    let mut value = options.ai_detect_size;
                    if ui
                        .add(WheelSpinBox::new(&mut value).range(896..=2048).speed(1.0))
                        .changed()
                    {
                        options.ai_detect_size = value.clamp(896, 2048);
                        actions.options_changed = true;
                    }
                });
            });
    }

    ui.separator();
    // What the run will do to the CURRENT page (the plan the worker uses), shown before the
    // detect buttons so the user sees the resize / tile count before starting. Planned here,
    // after the option widgets, so an option changed this frame is already reflected.
    if let Some(notice) = plan_notices.notice(options, page_size).as_ref() {
        ui.small(plan_notice_text(notice));
        if let Some(size_text) = plan_ctd_size_text(notice) {
            ui.small(size_text);
        }
    }
    let detect_enabled = has_pages && availability.is_ok() && !detect_busy && !ocr_busy;
    if ui
        .add_enabled(
            detect_enabled,
            egui::Button::new(t!("translation.text_detector_panel.detect_current_page_button")),
        )
        .clicked()
    {
        actions.detect_current = true;
    }
    if ui
        .add_enabled(detect_enabled, egui::Button::new(t!("translation.text_detector_panel.detect_all_button")))
        .clicked()
    {
        actions.detect_all = true;
    }
    if storage_available
        && ui
            .add_enabled(
                can_save && !detect_busy && !ocr_busy,
                egui::Button::new(t!("translation.text_detector_panel.save_selection_button")),
            )
            .clicked()
    {
        actions.save_results = true;
    }
    if ui.button(t!("translation.text_detector_panel.clear_results_button")).clicked() {
        actions.clear_results = true;
    }

    ui.separator();
    if ui
        .add_enabled(
            can_ocr_current && !detect_busy && !ocr_busy,
            egui::Button::new(t!("translation.text_detector_panel.recognize_current_page_button")),
        )
        .clicked()
    {
        actions.ocr_current = true;
    }
    if ui
        .add_enabled(
            can_ocr_all && !detect_busy && !ocr_busy,
            egui::Button::new(t!("translation.text_detector_panel.recognize_all_button")),
        )
        .clicked()
    {
        actions.ocr_all = true;
    }

    if detect_busy {
        ui.small(t!("translation.text_detector_panel.detecting_status"));
    }
    if ocr_busy {
        ui.small(t!("translation.text_detector_panel.recognizing_status"));
    }
    if let Some(hint) = availability.err().and_then(|reason| reason.panel_hint_text(options.algorithm)) {
        ui.small(hint);
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::{
        AiRequirement, DETECTOR_ALGORITHMS, DetectorUnavailable, TextDetectorAlgorithm, TextDetectorPanelOptions,
        TextDetectorPlanNoticeCache, algorithm_marker, algorithm_requirement, detector_page_size, plan_ctd_size_text,
        plan_notice_text,
    };
    use crate::text_detector::TextDetectorRunMode;
    use ms_models::page_view::{PageImageInfo, SourcePageLoadState};
    use ms_text_detect::{EngineKind, PlanMode, PlanNotice};

    const TORCH_STATES: [Option<bool>; 3] = [None, Some(true), Some(false)];

    /// Reference copy of the panel-arm `can_detect` match `availability` replaced.
    fn reference_can_detect(
        algorithm: TextDetectorAlgorithm,
        ai_enabled: bool,
        torch_available: Option<bool>,
    ) -> bool {
        match algorithm {
            TextDetectorAlgorithm::Classic => true,
            TextDetectorAlgorithm::PaddleOcr => ai_enabled,
            TextDetectorAlgorithm::Ai | TextDetectorAlgorithm::Surya => {
                ai_enabled && !matches!(torch_available, Some(false))
            }
        }
    }

    /// Reference copy of the run-mode error match `availability` replaced:
    /// `None` = the run is allowed, `Some(text)` = the refusal text.
    fn reference_run_error(
        algorithm: TextDetectorAlgorithm,
        ai_enabled: bool,
        torch_available: Option<bool>,
    ) -> Option<&'static str> {
        match algorithm {
            TextDetectorAlgorithm::Classic => None,
            TextDetectorAlgorithm::PaddleOcr => {
                (!ai_enabled).then(|| t!("translation.text_detector.paddle_disabled_status"))
            }
            TextDetectorAlgorithm::Ai => {
                if !ai_enabled {
                    return Some(t!("translation.text_detector.ai_disabled_status"));
                }
                matches!(torch_available, Some(false))
                    .then(|| t!("translation.common.pytorch_not_installed_status"))
            }
            TextDetectorAlgorithm::Surya => {
                if !ai_enabled {
                    return Some(t!("translation.text_detector.surya_disabled_status"));
                }
                matches!(torch_available, Some(false))
                    .then(|| t!("translation.common.pytorch_not_installed_status"))
            }
        }
    }

    /// The one-frame stale case: a reason computed for the previous algorithm meets a
    /// fresh switch to `Classic`; the panel must show no hint (the `Classic` arm was `None`).
    #[test]
    fn classic_never_shows_a_panel_hint() {
        for reason in [DetectorUnavailable::AiDisabled, DetectorUnavailable::TorchMissing] {
            assert_eq!(reason.panel_hint_text(TextDetectorAlgorithm::Classic), None, "{reason:?}");
        }
    }

    #[test]
    fn availability_matches_the_three_replaced_decisions() {
        for algorithm in DETECTOR_ALGORITHMS {
            for ai_enabled in [false, true] {
                for torch_available in TORCH_STATES {
                    let case = format!("{algorithm:?} ai={ai_enabled} torch={torch_available:?}");
                    let availability = algorithm.availability(ai_enabled, torch_available);
                    assert_eq!(
                        availability.is_ok(),
                        reference_can_detect(algorithm, ai_enabled, torch_available),
                        "can_detect: {case}"
                    );
                    assert_eq!(
                        availability.err().map(|reason| reason.status_text(algorithm)),
                        reference_run_error(algorithm, ai_enabled, torch_available),
                        "run error: {case}"
                    );
                    // The panel hint names the same reason a refused run reports
                    // (PyTorch missing included); `Classic` never reaches `Err`.
                    if let Err(reason) = availability {
                        assert_eq!(
                            reason.panel_hint_text(algorithm),
                            reference_run_error(algorithm, ai_enabled, torch_available),
                            "panel hint: {case}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn availability_reasons() {
        assert_eq!(TextDetectorAlgorithm::Classic.availability(false, Some(false)), Ok(()));
        assert_eq!(
            TextDetectorAlgorithm::PaddleOcr.availability(true, Some(false)),
            Ok(())
        );
        assert_eq!(
            TextDetectorAlgorithm::Ai.availability(false, Some(false)),
            Err(DetectorUnavailable::AiDisabled)
        );
        assert_eq!(
            TextDetectorAlgorithm::Surya.availability(true, Some(false)),
            Err(DetectorUnavailable::TorchMissing)
        );
        assert_eq!(TextDetectorAlgorithm::Surya.availability(true, None), Ok(()));
    }

    /// Q7c: a PyTorch detector with PyTorch missing names that reason, not "AI disabled".
    #[test]
    fn torch_missing_hint_names_pytorch() {
        for algorithm in [TextDetectorAlgorithm::Ai, TextDetectorAlgorithm::Surya] {
            assert_eq!(
                DetectorUnavailable::TorchMissing.panel_hint_text(algorithm),
                Some(t!("translation.common.pytorch_not_installed_status")),
                "{algorithm:?}"
            );
        }
    }

    /// Installs the embedded English catalog for tests that assert formatted text, serialized
    /// on the process-global locale lock (the active catalog is one `ArcSwap`).
    fn english_locale() -> std::sync::MutexGuard<'static, ()> {
        let guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");
        guard
    }

    fn notice(mode: PlanMode, scale_percent: [u32; 2], grid: [u32; 2], ctd: Option<u32>) -> PlanNotice {
        PlanNotice {
            mode,
            scale_percent,
            cols: grid[0],
            rows: grid[1],
            tiles: grid[0] * grid[1],
            tile_size: [1280, 1280],
            scaled_size: [800, 12_000],
            ctd_detect_size: ctd,
        }
    }

    #[test]
    fn plan_notice_text_per_mode() {
        let _guard = english_locale();
        assert_eq!(
            plan_notice_text(&notice(PlanMode::FullResolution, [100, 100], [1, 1], None)),
            "Current page: full resolution (800×12000), one pass."
        );
        assert_eq!(
            plan_notice_text(&notice(PlanMode::Resized, [57, 57], [1, 1], None)),
            "Current page: scaled to 57% (800×12000), one pass."
        );
        assert_eq!(
            plan_notice_text(&notice(PlanMode::Tiled, [100, 100], [1, 21], None)),
            "Current page: full resolution, 21 tiles (grid 1×21, tile 1280×1280)."
        );
        assert_eq!(
            plan_notice_text(&notice(PlanMode::ResizedAndTiled, [64, 64], [2, 1], None)),
            "Current page: scaled to 64% (800×12000), 2 tiles (grid 2×1, tile 1280×1280)."
        );
        // Surya squeezes the axes differently: both percentages are shown.
        assert_eq!(
            plan_notice_text(&notice(PlanMode::Resized, [150, 92], [1, 1], None)),
            "Current page: scaled to 150% × 92% (800×12000), one pass."
        );
    }

    #[test]
    fn ctd_size_line_only_for_ctd() {
        let _guard = english_locale();
        assert_eq!(plan_ctd_size_text(&notice(PlanMode::Tiled, [100, 100], [1, 3], None)), None);
        assert_eq!(
            plan_ctd_size_text(&notice(PlanMode::Tiled, [100, 100], [1, 3], Some(960))).as_deref(),
            Some("CTD detection size: 960 px")
        );
    }

    #[test]
    fn page_size_known_only_when_decoded_and_non_empty() {
        let info = |w, h, load_state| PageImageInfo { width_px: w, height_px: h, load_state };
        assert_eq!(detector_page_size(None), None);
        assert_eq!(detector_page_size(Some(&info(0, 0, SourcePageLoadState::Loading))), None);
        assert_eq!(detector_page_size(Some(&info(800, 1200, SourcePageLoadState::Loading))), None);
        assert_eq!(detector_page_size(Some(&info(800, 1200, SourcePageLoadState::Failed))), None);
        assert_eq!(detector_page_size(Some(&info(0, 1200, SourcePageLoadState::Available))), None);
        assert_eq!(detector_page_size(Some(&info(800, 1200, SourcePageLoadState::Available))), Some([800, 1200]));
    }

    /// The options build the run mode the tab starts; CTD carries the detect size and no
    /// region dilation (page batches pass theirs separately).
    #[test]
    fn options_build_the_run_mode() {
        let mut options = TextDetectorPanelOptions { ai_detect_size: 1000, ..TextDetectorPanelOptions::default() };
        for (algorithm, engine) in [
            (TextDetectorAlgorithm::Classic, EngineKind::Classic),
            (TextDetectorAlgorithm::PaddleOcr, EngineKind::Paddle),
            (TextDetectorAlgorithm::Ai, EngineKind::Ctd),
            (TextDetectorAlgorithm::Surya, EngineKind::Surya),
        ] {
            options.algorithm = algorithm;
            assert_eq!(options.run_mode().plan_inputs().0, engine, "{algorithm:?}");
        }
        options.algorithm = TextDetectorAlgorithm::Ai;
        let TextDetectorRunMode::AiCtd(ctd) = options.run_mode() else {
            panic!("CTD algorithm must build an AiCtd mode");
        };
        assert_eq!((ctd.detect_size, ctd.mask_dilate_size), (1000, 0));
    }

    /// The cache yields exactly what `plan_detection` says for the current inputs, follows
    /// engine, param and page-size changes, and has no notice for an unknown size.
    #[test]
    fn notice_cache_follows_inputs() {
        let mut cache = TextDetectorPlanNoticeCache::default();
        let mut options = TextDetectorPanelOptions { algorithm: TextDetectorAlgorithm::Ai, ..TextDetectorPanelOptions::default() };
        assert_eq!(cache.notice(&options, None), None);
        let expected = |options: &TextDetectorPanelOptions, size: [u32; 2]| {
            let (engine, params) = options.run_mode().plan_inputs();
            ms_text_detect::plan_detection(engine, size, &params).map(|plan| plan.notice()).ok()
        };
        let tall = [800, 16_000];
        let first = cache.notice(&options, Some(tall));
        assert_eq!(first, expected(&options, tall));
        let first = first.expect("a 800x16000 page has a CTD plan");
        assert!(first.tiles > 1, "a tall page is tiled: {first:?}");
        assert_eq!(first.ctd_detect_size, Some(1280));
        // Detect size change: 1000 is snapped down to 960.
        options.ai_detect_size = 1000;
        let snapped = cache.notice(&options, Some(tall)).expect("plan");
        assert_eq!(snapped.ctd_detect_size, Some(960));
        // Page change.
        assert_eq!(cache.notice(&options, Some([800, 1200])), expected(&options, [800, 1200]));
        // Engine change.
        options.algorithm = TextDetectorAlgorithm::Classic;
        let classic = cache.notice(&options, Some([2000, 3200])).expect("classic plan");
        assert_eq!(classic.mode, PlanMode::Resized);
        assert_eq!(classic.scale_percent, [50, 50]);
        assert_eq!(classic.ctd_detect_size, None);
        // A page above the pixel limit has no plan, hence no notice.
        assert_eq!(cache.notice(&options, Some([u32::MAX, u32::MAX])), None);
    }

    #[test]
    fn algorithm_requirements_match_runtime() {
        assert_eq!(algorithm_requirement(TextDetectorAlgorithm::Classic), None);
        assert_eq!(
            algorithm_requirement(TextDetectorAlgorithm::PaddleOcr),
            Some(AiRequirement::Onnx)
        );
        assert_eq!(
            algorithm_requirement(TextDetectorAlgorithm::Ai),
            Some(AiRequirement::Torch)
        );
        assert_eq!(
            algorithm_requirement(TextDetectorAlgorithm::Surya),
            Some(AiRequirement::Torch)
        );
    }

    #[test]
    fn algorithm_markers_present_only_for_runtime_algorithms() {
        assert_eq!(algorithm_marker(TextDetectorAlgorithm::Classic), None);
        assert_eq!(
            algorithm_marker(TextDetectorAlgorithm::PaddleOcr),
            Some("ONNX")
        );
        assert_eq!(algorithm_marker(TextDetectorAlgorithm::Ai), Some("Torch"));
        assert_eq!(algorithm_marker(TextDetectorAlgorithm::Surya), Some("Torch"));
    }

    #[test]
    fn every_algorithm_is_listed_once() {
        assert_eq!(DETECTOR_ALGORITHMS.len(), 4);
        for algorithm in [
            TextDetectorAlgorithm::Classic,
            TextDetectorAlgorithm::PaddleOcr,
            TextDetectorAlgorithm::Ai,
            TextDetectorAlgorithm::Surya,
        ] {
            assert_eq!(
                DETECTOR_ALGORITHMS
                    .iter()
                    .filter(|&&a| a == algorithm)
                    .count(),
                1
            );
        }
    }
}
