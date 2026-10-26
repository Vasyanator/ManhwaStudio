/*
File: crates/ms-tab-translation/src/panels/ocr_model_download.rs

Purpose:
The download / verify block of an app-managed external model bundle (`ocr_model_download` bundles:
Baberu OCR, the selected PaddleOCR-VL variant, the EasyOCR model of the selected languages,
the two Surya OCR checkpoints, the Surya text-detection checkpoint; the Cleaning tab draws it
for its SDXL configs): install status, the outcome of the last unsuccessful download, the
"Download (size)" / "Resume download" button, a "Verify files" button once the bundle is
installed (hashes every file against its pin: the repair path for a damaged file the stat-only
probe cannot see; it ends with an intact / N-files-removed notice), a progress bar with Cancel
while a download, verify or automatic adoption (files found on disk without markers) runs
(its text also shows the automatic retry countdown after a network failure, or that found
files are being checked), the outcome of a refused or cancelled adoption, and — while ANOTHER
bundle's job runs — a line naming it with its progress over the disabled button.
It only draws and returns a `ModelDownloadAction`; the caller hands the action to
`OcrModelDownloadController`.

Key items:
- `ModelDownloadAction`: what the user asked for this frame.
- `download_block_view()`: pure snapshot -> (lines, progress, button) mapping, unit-tested.
- `draw_ocr_model_download()`: renders that view (`pub`: the Cleaning tab draws it too).
- `load_blocked_reason()`: why a Load / Run button is disabled for the bundle's state.
- `external_model_display_name()`: the user-facing name of every catalog spec.

Notes:
- Download / resume captions come from `ocr_model_download::offered_download_label`, the
  same owner the "model is not downloaded" errors use, so an error names exactly the
  button drawn here.
*/

use crate::ocr_model_download::{
    DownloadNotice, JobKind, OcrModelDownloadState, OcrModelPanelSnapshot, OtherDownload, download_button_label, format_bytes,
    offered_download_label,
};
use ms_sysprobe::ai_models::easyocr::{EasyOcrScript, easyocr_spec};
use ms_sysprobe::ai_models::external::{ExternalDownloadPhase, ExternalDownloadProgress, ExternalModelSpec};
use ms_config::{Flux2Variant, Flux2WeightAxis, Flux2WeightFormat};
use ms_sysprobe::ai_models::external_catalog::{
    BABERU_OCR, FLUX2_KLEIN_4B_BASE, PaddleVlVariant, SDXL_CONFIG_BASE, SDXL_CONFIG_INPAINT, SDXL_INPAINT_FP16,
    SURYA_DETECTION_2025_05_07, SURYA_RECOGNITION_2025_09_23, flux2_klein_spec,
};

/// The user's request from the download block in this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelDownloadAction {
    /// Nothing clicked.
    #[default]
    None,
    /// Start (or resume) downloading the shown model.
    Download,
    /// Verify the installed files of the shown model against their pins.
    Verify,
    /// Cancel the running download.
    Cancel,
}

/// Colour role of one text line of the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTone {
    Weak,
    Success,
    Warning,
    Error,
}

/// The block's button: caption, the action its click returns, and whether it is enabled.
#[derive(Debug, Clone, PartialEq)]
struct BlockButton {
    caption: String,
    action: ModelDownloadAction,
    enabled: bool,
}

/// Everything the block shows for one snapshot; built by the pure [`download_block_view`].
#[derive(Debug, Clone, PartialEq)]
struct DownloadBlockView {
    /// Text lines (status, last outcome, other running download) and their colour roles.
    lines: Vec<(StatusTone, String)>,
    /// Progress bar fraction (0..=1) and its text.
    progress: Option<(f32, String)>,
    /// The button, if the state offers one.
    button: Option<BlockButton>,
}

/// Display name of an external model spec for user-facing text: the engine's product name
/// for Baberu OCR, the localized variant label for a PaddleOCR-VL variant, "EasyOCR" with the
/// recognizer file of its script, localized names for the Surya checkpoints, the SDXL
/// downloads and the FLUX.2 klein 4B package parts (a GGUF part names its quant token); the
/// spec id for a spec this table does not know (logged once per call site would be noise: the
/// id is the diagnostic itself).
#[must_use]
pub fn external_model_display_name(spec: &ExternalModelSpec) -> String {
    if let Some(name) = flux2_klein_display_name(spec) {
        return name;
    }
    if spec.id == BABERU_OCR.id {
        // Product name, not translated (the engine selector shows the same literal).
        return "Baberu OCR".to_owned();
    }
    if let Some(variant) = PaddleVlVariant::ALL.into_iter().find(|variant| variant.spec().id == spec.id) {
        return crate::panels::ocr::paddle_vl_variant_label(variant).to_owned();
    }
    if let Some(script) = EasyOcrScript::ALL.into_iter().find(|script| easyocr_spec(*script).id == spec.id) {
        return tf!("translation.ocr_model.easyocr_model_name", file = script.recognizer_file());
    }
    let key = match spec.id {
        id if id == SURYA_DETECTION_2025_05_07.id => "translation.ocr_model.surya_detection_name",
        id if id == SURYA_RECOGNITION_2025_09_23.id => "translation.ocr_model.surya_recognition_name",
        id if id == SDXL_CONFIG_BASE.id => "translation.ocr_model.sdxl_config_base_name",
        id if id == SDXL_CONFIG_INPAINT.id => "translation.ocr_model.sdxl_config_inpaint_name",
        id if id == SDXL_INPAINT_FP16.id => "translation.ocr_model.sdxl_inpaint_fp16_name",
        other => return other.to_owned(),
    };
    // The key is chosen at runtime from the table above, so `resolve_key` replaces `t!`.
    ms_i18n::resolve_key(key).to_string()
}

/// The display name of a FLUX.2 klein 4B package spec, `None` for any other spec. The
/// weight specs are found through the catalog's own lookup over the offered formats, so a
/// new format is named without touching this function.
fn flux2_klein_display_name(spec: &ExternalModelSpec) -> Option<String> {
    if spec.id == FLUX2_KLEIN_4B_BASE.id {
        return Some(t!("translation.ocr_model.flux2_klein_4b_base_name").to_string());
    }
    let variant = Flux2Variant::Klein4B;
    let (axis, format) = Flux2WeightAxis::all().into_iter().find_map(|axis| {
        Flux2WeightFormat::offered_for(variant, axis)
            .iter()
            .find(|format| flux2_klein_spec(variant, axis, **format).is_some_and(|candidate| candidate.id == spec.id))
            .map(|format| (axis, *format))
    })?;
    let name = match (axis, format.quant_token()) {
        (Flux2WeightAxis::Transformer, None) => t!("translation.ocr_model.flux2_klein_4b_transformer_bf16_name").to_string(),
        (Flux2WeightAxis::TextEncoder, None) => t!("translation.ocr_model.flux2_klein_4b_text_encoder_bf16_name").to_string(),
        // The quant token (`Q8_0`, …) is a technical literal shown as is (i18n exclusion).
        (Flux2WeightAxis::Transformer, Some(quant)) => tf!("translation.ocr_model.flux2_klein_4b_transformer_gguf_name", quant = quant),
        (Flux2WeightAxis::TextEncoder, Some(quant)) => tf!("translation.ocr_model.flux2_klein_4b_text_encoder_gguf_name", quant = quant),
    };
    Some(name)
}

/// Display name of a bundle: its specs' names joined with " + ".
fn bundle_display_name(specs: &[&'static ExternalModelSpec]) -> String {
    specs.iter().map(|spec| external_model_display_name(spec)).collect::<Vec<_>>().join(" + ")
}

/// The line naming the other bundle's running download / verify with its byte progress.
fn other_download_line(other: &OtherDownload) -> String {
    let name = bundle_display_name(&other.specs);
    let done = format_bytes(other.progress.total_done);
    let total = format_bytes(other.progress.total_bytes);
    match other.kind {
        JobKind::Download => tf!("translation.ocr_model.other_download_status", name = name, done = done, total = total),
        JobKind::Verify => tf!("translation.ocr_model.other_verify_status", name = name, done = done, total = total),
        JobKind::Adopt => tf!("translation.ocr_model.other_adopt_status", name = name, done = done, total = total),
    }
}

/// Maps a bundle's download snapshot to what the block shows. Pure (reads only the active
/// i18n catalog), so the mapping is unit-tested without a `Ui`.
fn download_block_view(snapshot: &OcrModelPanelSnapshot) -> DownloadBlockView {
    let mut lines = Vec::new();
    let status = match &snapshot.state {
        OcrModelDownloadState::Unknown | OcrModelDownloadState::Checking => {
            Some((StatusTone::Weak, t!("translation.ocr_model.checking_status").to_string()))
        }
        OcrModelDownloadState::Missing => Some((StatusTone::Warning, t!("translation.ocr_model.not_downloaded_status").to_string())),
        // A notice (cancelled / failed) already says why the download is unfinished.
        OcrModelDownloadState::Partial { .. } if snapshot.notice.is_some() => None,
        OcrModelDownloadState::Partial { .. } => {
            Some((StatusTone::Warning, t!("translation.ocr_model.interrupted_status").to_string()))
        }
        OcrModelDownloadState::Installed => Some((StatusTone::Success, t!("translation.ocr_model.installed_status").to_string())),
        OcrModelDownloadState::Downloading(_) | OcrModelDownloadState::Verifying(_) => None,
        OcrModelDownloadState::Unavailable(reason) => Some((StatusTone::Error, reason.clone())),
    };
    lines.extend(status);
    let offered = offered_download_label(snapshot);
    match &snapshot.notice {
        Some(DownloadNotice::Cancelled) => lines.push((StatusTone::Weak, t!("translation.ocr_model.cancelled_status").to_string())),
        Some(DownloadNotice::Failed(err)) => lines.push((StatusTone::Error, tf!("translation.ocr_model.failed_error", err = err))),
        Some(DownloadNotice::VerifyIntact) => lines.push((StatusTone::Weak, t!("translation.ocr_model.verify_intact_status").to_string())),
        Some(DownloadNotice::VerifyRemoved { files }) => {
            // Names the button the re-probed state offers (normally "Download (size)").
            let button = offered.clone().unwrap_or_else(|| download_button_label(snapshot.missing_bytes));
            lines.push((StatusTone::Warning, tf!("translation.ocr_model.verify_removed_status", count = files, button = button)));
        }
        Some(DownloadNotice::VerifyCancelled) => {
            lines.push((StatusTone::Weak, t!("translation.ocr_model.verify_cancelled_status").to_string()));
        }
        // The adoption notices name the button the re-probed state offers: a download keeps
        // every matching file and fetches only the rest.
        Some(DownloadNotice::AdoptMismatch { file }) => {
            let button = offered.clone().unwrap_or_else(|| download_button_label(snapshot.missing_bytes));
            lines.push((StatusTone::Warning, tf!("translation.ocr_model.adopt_mismatch_status", file = file, button = button)));
        }
        Some(DownloadNotice::AdoptFailed(err)) => lines.push((StatusTone::Error, tf!("translation.ocr_model.adopt_failed_error", err = err))),
        Some(DownloadNotice::AdoptCancelled) => {
            let button = offered.clone().unwrap_or_else(|| download_button_label(snapshot.missing_bytes));
            lines.push((StatusTone::Weak, tf!("translation.ocr_model.adopt_cancelled_status", button = button)));
        }
        None => {}
    }
    // The button the state offers: download / resume, or verify once installed.
    let idle_button = if let Some(caption) = offered {
        Some((caption, ModelDownloadAction::Download))
    } else if snapshot.state == OcrModelDownloadState::Installed {
        Some((t!("translation.ocr_model.verify_button").to_string(), ModelDownloadAction::Verify))
    } else {
        None
    };
    // Only a state that offers a button is blocked by another bundle's running job.
    if idle_button.is_some()
        && let Some(other) = &snapshot.other_download
    {
        lines.push((StatusTone::Weak, other_download_line(other)));
    }
    let running = match &snapshot.state {
        OcrModelDownloadState::Downloading(progress) | OcrModelDownloadState::Verifying(progress) => Some(progress),
        OcrModelDownloadState::Unknown
        | OcrModelDownloadState::Checking
        | OcrModelDownloadState::Missing
        | OcrModelDownloadState::Partial { .. }
        | OcrModelDownloadState::Installed
        | OcrModelDownloadState::Unavailable(_) => None,
    };
    let (progress, button) = if let Some(progress) = running {
        let cancel = BlockButton {
            caption: t!("translation.common.cancel_button").to_string(),
            action: ModelDownloadAction::Cancel,
            enabled: true,
        };
        (Some((progress_fraction(progress), progress_text(progress))), Some(cancel))
    } else {
        let button = idle_button.map(|(caption, action)| BlockButton { caption, action, enabled: snapshot.other_download.is_none() });
        (None, button)
    };
    DownloadBlockView { lines, progress, button }
}

/// Why a Load / Run button is disabled for this snapshot of the bundle with key `bundle_key`
/// (`ocr_model_download::bundle_key`), or `None` when the bundle is installed. A snapshot of
/// another bundle (taken before an engine / variant / language switch in this frame) counts
/// as still checking.
#[must_use]
pub fn load_blocked_reason(bundle_key: &str, snapshot: Option<&OcrModelPanelSnapshot>) -> Option<String> {
    let Some(snapshot) = snapshot.filter(|snapshot| snapshot.bundle_key == bundle_key) else {
        return Some(t!("translation.ocr_model.checking_status").to_string());
    };
    match &snapshot.state {
        OcrModelDownloadState::Installed => None,
        OcrModelDownloadState::Unknown | OcrModelDownloadState::Checking => Some(t!("translation.ocr_model.checking_status").to_string()),
        OcrModelDownloadState::Downloading(_) => Some(t!("translation.common.downloading_model_status").to_string()),
        OcrModelDownloadState::Verifying(_) => Some(t!("translation.ocr_model.verifying_files_status").to_string()),
        OcrModelDownloadState::Missing | OcrModelDownloadState::Partial { .. } => {
            Some(t!("translation.ocr_model.not_downloaded_status").to_string())
        }
        OcrModelDownloadState::Unavailable(reason) => Some(reason.clone()),
    }
}

/// Overall fraction done, `0.0..=1.0`, from whole-model byte counters. Integer per-mille
/// first, so the `f32` comes from a lossless `u16` conversion.
fn progress_fraction(progress: &ExternalDownloadProgress) -> f32 {
    if progress.total_bytes == 0 {
        return 0.0;
    }
    let per_mille = (u128::from(progress.total_done) * 1000 / u128::from(progress.total_bytes)).min(1000);
    // `per_mille <= 1000` after the clamp, so it fits in `u16`.
    let per_mille = u16::try_from(per_mille).unwrap_or(1000);
    f32::from(per_mille) / 1000.0
}

/// Progress bar text: which file (1-based) and the byte counters while downloading, the
/// file being hashed while verifying or adopting found files, the retry number and countdown (then "reconnecting")
/// while the downloader waits out a transient network failure.
fn progress_text(progress: &ExternalDownloadProgress) -> String {
    match progress.phase {
        ExternalDownloadPhase::Downloading => tf!(
            "translation.ocr_model.downloading_status",
            index = progress.file_index.saturating_add(1),
            count = progress.file_count,
            done = format_bytes(progress.total_done),
            total = format_bytes(progress.total_bytes)
        ),
        ExternalDownloadPhase::Verifying => tf!("translation.ocr_model.verifying_status", file = progress.file_path),
        ExternalDownloadPhase::Adopting => tf!("translation.ocr_model.adopting_status", file = progress.file_path),
        ExternalDownloadPhase::Retrying { attempt, max_attempts, delay_secs: 0 } => tf!(
            "translation.ocr_model.reconnecting_status",
            attempt = attempt,
            max = max_attempts,
            done = format_bytes(progress.total_done),
            total = format_bytes(progress.total_bytes)
        ),
        ExternalDownloadPhase::Retrying { attempt, max_attempts, delay_secs } => tf!(
            "translation.ocr_model.retrying_status",
            attempt = attempt,
            max = max_attempts,
            seconds = delay_secs,
            done = format_bytes(progress.total_done),
            total = format_bytes(progress.total_bytes)
        ),
    }
}

/// Draws the download block of a bundle from its `snapshot` and returns the clicked action.
/// The caller draws it only when `snapshot.bundle_key` matches the bundle it currently needs.
pub fn draw_ocr_model_download(ui: &mut egui::Ui, snapshot: &OcrModelPanelSnapshot) -> ModelDownloadAction {
    let view = download_block_view(snapshot);
    let mut action = ModelDownloadAction::None;
    for (tone, text) in &view.lines {
        let color = match tone {
            StatusTone::Weak => ui.visuals().weak_text_color(),
            StatusTone::Success => ms_theme::status::SUCCESS,
            StatusTone::Warning => ms_theme::status::WARNING,
            StatusTone::Error => ms_theme::status::ERROR,
        };
        ui.colored_label(color, text);
    }
    if let Some((fraction, text)) = &view.progress {
        ui.add(egui::ProgressBar::new(*fraction).text(text.as_str()));
    }
    if let Some(button) = &view.button
        && ui.add_enabled(button.enabled, egui::Button::new(button.caption.as_str())).clicked()
    {
        action = button.action;
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr_model_download::bundle_key;

    const BABERU: &[&ExternalModelSpec] = &[&BABERU_OCR];

    fn progress(phase: ExternalDownloadPhase, total_done: u64) -> ExternalDownloadProgress {
        ExternalDownloadProgress {
            file_index: 1,
            file_count: 5,
            file_path: "onnx/decoder_prefill_int8.onnx",
            phase,
            file_done: 0,
            file_total: 10,
            total_done,
            total_bytes: BABERU_OCR.total_bytes(),
        }
    }

    fn snapshot(state: OcrModelDownloadState) -> OcrModelPanelSnapshot {
        OcrModelPanelSnapshot { bundle_key: bundle_key(BABERU), state, missing_bytes: BABERU_OCR.total_bytes(), notice: None, other_download: None }
    }

    fn download_button(caption: String, enabled: bool) -> Option<BlockButton> {
        Some(BlockButton { caption, action: ModelDownloadAction::Download, enabled })
    }

    #[test]
    fn missing_and_partial_offer_the_owned_download_caption() {
        let _locale = crate::hold_locale_for_test();
        for state in [OcrModelDownloadState::Missing, OcrModelDownloadState::Partial { bytes_present: 1024 }] {
            let shown = snapshot(state.clone());
            let view = download_block_view(&shown);
            let caption = offered_download_label(&shown).expect("a download state");
            assert_eq!(view.button, download_button(caption, true), "{state:?}");
            assert_eq!(view.lines.len(), 1, "{state:?}");
        }
        // An unfinished download without a notice is reported as interrupted (L2).
        let partial = download_block_view(&snapshot(OcrModelDownloadState::Partial { bytes_present: 1024 }));
        assert_eq!(partial.lines, vec![(StatusTone::Warning, t!("translation.ocr_model.interrupted_status").to_string())]);
    }

    /// M1: after a cancel / failure the block shows the notice next to the re-probed
    /// state's own button, never a separate "Retry" caption.
    #[test]
    fn notices_keep_the_reprobed_button() {
        let _locale = crate::hold_locale_for_test();
        let state = OcrModelDownloadState::Partial { bytes_present: 1024 };
        let caption = offered_download_label(&snapshot(state.clone())).expect("a download state");
        let cancelled = OcrModelPanelSnapshot { notice: Some(DownloadNotice::Cancelled), ..snapshot(state.clone()) };
        let view = download_block_view(&cancelled);
        assert_eq!(view.lines, vec![(StatusTone::Weak, t!("translation.ocr_model.cancelled_status").to_string())]);
        assert_eq!(view.button, download_button(caption, true));

        let failed = OcrModelPanelSnapshot { notice: Some(DownloadNotice::Failed("boom".to_owned())), ..snapshot(OcrModelDownloadState::Missing) };
        let view = download_block_view(&failed);
        assert_eq!(view.lines.last(), Some(&(StatusTone::Error, tf!("translation.ocr_model.failed_error", err = "boom"))));
        assert_eq!(view.button, download_button(offered_download_label(&failed).expect("missing"), true));
    }

    /// M2: another bundle's download is named with its progress and disables the button.
    #[test]
    fn another_running_download_is_named_and_disables_the_button() {
        let _locale = crate::hold_locale_for_test();
        let other_specs = vec![&SURYA_DETECTION_2025_05_07, &SURYA_RECOGNITION_2025_09_23];
        let other = OtherDownload { specs: other_specs, kind: JobKind::Download, progress: progress(ExternalDownloadPhase::Downloading, 1024) };
        let blocked = OcrModelPanelSnapshot { other_download: Some(other.clone()), ..snapshot(OcrModelDownloadState::Missing) };
        let view = download_block_view(&blocked);
        let expected_line = tf!(
            "translation.ocr_model.other_download_status",
            name = format!(
                "{} + {}",
                external_model_display_name(&SURYA_DETECTION_2025_05_07),
                external_model_display_name(&SURYA_RECOGNITION_2025_09_23)
            ),
            done = format_bytes(1024),
            total = format_bytes(BABERU_OCR.total_bytes())
        );
        assert_eq!(view.lines.last(), Some(&(StatusTone::Weak, expected_line)));
        let button = view.button.expect("the download button stays visible");
        assert!(!button.enabled);
        assert_eq!(button.action, ModelDownloadAction::Download);
        // An installed model offers Verify, which the running download disables too.
        let installed = OcrModelPanelSnapshot { other_download: Some(other), ..snapshot(OcrModelDownloadState::Installed) };
        let view = download_block_view(&installed);
        assert_eq!(view.lines.len(), 2);
        assert_eq!(view.button.map(|button| (button.action, button.enabled)), Some((ModelDownloadAction::Verify, false)));
    }

    /// An installed bundle offers "Verify files" (disabled while another job runs); a running
    /// verify shows its progress and Cancel and blocks loading.
    #[test]
    fn installed_offers_verify_and_a_running_verify_shows_progress() {
        let _locale = crate::hold_locale_for_test();
        let installed = download_block_view(&snapshot(OcrModelDownloadState::Installed));
        let verify = Some(BlockButton { caption: t!("translation.ocr_model.verify_button").to_string(), action: ModelDownloadAction::Verify, enabled: true });
        assert_eq!(installed.button, verify);
        let other = OtherDownload { specs: vec![&SURYA_DETECTION_2025_05_07], kind: JobKind::Verify, progress: progress(ExternalDownloadPhase::Verifying, 5) };
        let blocked = download_block_view(&OcrModelPanelSnapshot { other_download: Some(other.clone()), ..snapshot(OcrModelDownloadState::Installed) });
        assert_eq!(blocked.button.map(|button| (button.action, button.enabled)), Some((ModelDownloadAction::Verify, false)));
        assert_eq!(blocked.lines.last(), Some(&(StatusTone::Weak, other_download_line(&other))));

        let running = snapshot(OcrModelDownloadState::Verifying(progress(ExternalDownloadPhase::Verifying, 0)));
        let view = download_block_view(&running);
        assert_eq!(view.button.map(|button| button.action), Some(ModelDownloadAction::Cancel));
        assert!(view.progress.is_some());
        assert_eq!(load_blocked_reason(&running.bundle_key, Some(&running)), Some(t!("translation.ocr_model.verifying_files_status").to_string()));
    }

    /// The verify notices: intact (neutral, next to Verify), N files removed naming the
    /// re-probed state's Download button, cancelled.
    #[test]
    fn verify_notices_are_shown_next_to_the_state() {
        let _locale = crate::hold_locale_for_test();
        let intact = OcrModelPanelSnapshot { notice: Some(DownloadNotice::VerifyIntact), ..snapshot(OcrModelDownloadState::Installed) };
        let view = download_block_view(&intact);
        assert_eq!(view.lines.last(), Some(&(StatusTone::Weak, t!("translation.ocr_model.verify_intact_status").to_string())));
        assert_eq!(view.button.map(|button| button.action), Some(ModelDownloadAction::Verify));

        let removed = OcrModelPanelSnapshot { notice: Some(DownloadNotice::VerifyRemoved { files: 2 }), ..snapshot(OcrModelDownloadState::Missing) };
        let view = download_block_view(&removed);
        let caption = offered_download_label(&removed).expect("missing offers a download");
        assert_eq!(view.lines.last(), Some(&(StatusTone::Warning, tf!("translation.ocr_model.verify_removed_status", count = 2, button = caption.clone()))));
        assert_eq!(view.button, download_button(caption, true));

        let cancelled = OcrModelPanelSnapshot { notice: Some(DownloadNotice::VerifyCancelled), ..snapshot(OcrModelDownloadState::Installed) };
        let view = download_block_view(&cancelled);
        assert_eq!(view.lines.last(), Some(&(StatusTone::Weak, t!("translation.ocr_model.verify_cancelled_status").to_string())));
    }

    #[test]
    fn checking_and_unavailable_offer_no_button() {
        let _locale = crate::hold_locale_for_test();
        for state in [
            OcrModelDownloadState::Checking,
            OcrModelDownloadState::Unknown,
            OcrModelDownloadState::Unavailable("gone".to_owned()),
        ] {
            let view = download_block_view(&snapshot(state.clone()));
            assert_eq!(view.button, None, "{state:?}");
            assert!(view.progress.is_none(), "{state:?}");
        }
    }

    #[test]
    fn downloading_shows_progress_and_cancel() {
        let _locale = crate::hold_locale_for_test();
        let half = BABERU_OCR.total_bytes() / 2;
        let view = download_block_view(&snapshot(OcrModelDownloadState::Downloading(progress(ExternalDownloadPhase::Downloading, half))));
        assert_eq!(
            view.button,
            Some(BlockButton { caption: t!("translation.common.cancel_button").to_string(), action: ModelDownloadAction::Cancel, enabled: true })
        );
        let (fraction, text) = view.progress.expect("progress bar while downloading");
        assert!((fraction - 0.5).abs() < 0.002, "{fraction}");
        assert_eq!(
            text,
            tf!(
                "translation.ocr_model.downloading_status",
                index = 2,
                count = 5,
                done = format_bytes(half),
                total = format_bytes(BABERU_OCR.total_bytes())
            )
        );
        let verifying = progress(ExternalDownloadPhase::Verifying, 0);
        assert_eq!(progress_text(&verifying), tf!("translation.ocr_model.verifying_status", file = "onnx/decoder_prefill_int8.onnx"));
    }

    /// A retry wait shows the attempt and the countdown, then "reconnecting" at zero, with
    /// the byte counters kept; the block still offers Cancel.
    #[test]
    fn retrying_shows_the_attempt_and_the_countdown() {
        let _locale = crate::hold_locale_for_test();
        let waiting = progress(ExternalDownloadPhase::Retrying { attempt: 2, max_attempts: 5, delay_secs: 4 }, 1024);
        let total = format_bytes(BABERU_OCR.total_bytes());
        assert_eq!(
            progress_text(&waiting),
            tf!("translation.ocr_model.retrying_status", attempt = 2, max = 5, seconds = 4, done = format_bytes(1024), total = total.clone())
        );
        let now = progress(ExternalDownloadPhase::Retrying { attempt: 2, max_attempts: 5, delay_secs: 0 }, 1024);
        assert_eq!(
            progress_text(&now),
            tf!("translation.ocr_model.reconnecting_status", attempt = 2, max = 5, done = format_bytes(1024), total = total)
        );
        assert_ne!(progress_text(&waiting), progress_text(&now));
        let view = download_block_view(&snapshot(OcrModelDownloadState::Downloading(waiting)));
        assert_eq!(view.button.map(|button| button.action), Some(ModelDownloadAction::Cancel));
        assert_eq!(view.progress.map(|(_, text)| text), Some(progress_text(&waiting)));
    }

    /// N2 / N3: the Load button's disabled reason follows the state, and a snapshot of a
    /// different bundle (stale for one frame after a switch) never counts as installed.
    #[test]
    fn load_blocked_reason_follows_the_state_and_the_bundle() {
        let _locale = crate::hold_locale_for_test();
        let key = bundle_key(BABERU);
        let checking = t!("translation.ocr_model.checking_status").to_string();
        assert_eq!(load_blocked_reason(&key, None), Some(checking.clone()));
        assert_eq!(load_blocked_reason(&key, Some(&snapshot(OcrModelDownloadState::Installed))), None);
        assert_eq!(load_blocked_reason(&key, Some(&snapshot(OcrModelDownloadState::Checking))), Some(checking.clone()));
        assert_eq!(
            load_blocked_reason(&key, Some(&snapshot(OcrModelDownloadState::Downloading(progress(ExternalDownloadPhase::Downloading, 0))))),
            Some(t!("translation.common.downloading_model_status").to_string())
        );
        assert_eq!(
            load_blocked_reason(&key, Some(&snapshot(OcrModelDownloadState::Missing))),
            Some(t!("translation.ocr_model.not_downloaded_status").to_string())
        );
        let stale = OcrModelPanelSnapshot { bundle_key: bundle_key(&[PaddleVlVariant::Official16.spec()]), ..snapshot(OcrModelDownloadState::Installed) };
        assert_eq!(load_blocked_reason(&key, Some(&stale)), Some(checking));
    }

    /// Every catalog spec a panel can show has its own name, never the raw id.
    #[test]
    fn display_names_cover_every_external_spec() {
        let _locale = crate::hold_locale_for_test();
        assert_eq!(external_model_display_name(&BABERU_OCR), "Baberu OCR");
        for variant in PaddleVlVariant::ALL {
            assert_eq!(external_model_display_name(variant.spec()), crate::panels::ocr::paddle_vl_variant_label(variant));
        }
        for script in EasyOcrScript::ALL {
            let spec = easyocr_spec(script);
            assert_eq!(external_model_display_name(spec), tf!("translation.ocr_model.easyocr_model_name", file = script.recognizer_file()));
        }
        for spec in ms_sysprobe::ai_models::external_catalog::all_specs() {
            assert_ne!(external_model_display_name(spec), spec.id, "{}", spec.id);
        }
    }

    /// The FLUX.2 klein 4B parts: one fixed name each for the base and the bf16 parts, and the
    /// quant token substituted into the GGUF names, all distinct.
    #[test]
    fn flux2_klein_parts_have_distinct_names_with_their_quant() {
        let _locale = crate::hold_locale_for_test();
        let variant = Flux2Variant::Klein4B;
        assert_eq!(external_model_display_name(&FLUX2_KLEIN_4B_BASE), t!("translation.ocr_model.flux2_klein_4b_base_name").to_string());
        let mut names = vec![external_model_display_name(&FLUX2_KLEIN_4B_BASE)];
        for axis in Flux2WeightAxis::all() {
            for &format in Flux2WeightFormat::offered_for(variant, axis) {
                let spec = flux2_klein_spec(variant, axis, format).expect("every offered 4B format has a spec");
                let name = external_model_display_name(spec);
                let expected = match (axis, format.quant_token()) {
                    (Flux2WeightAxis::Transformer, None) => t!("translation.ocr_model.flux2_klein_4b_transformer_bf16_name").to_string(),
                    (Flux2WeightAxis::TextEncoder, None) => t!("translation.ocr_model.flux2_klein_4b_text_encoder_bf16_name").to_string(),
                    (Flux2WeightAxis::Transformer, Some(quant)) => tf!("translation.ocr_model.flux2_klein_4b_transformer_gguf_name", quant = quant),
                    (Flux2WeightAxis::TextEncoder, Some(quant)) => tf!("translation.ocr_model.flux2_klein_4b_text_encoder_gguf_name", quant = quant),
                };
                assert_eq!(name, expected, "{}", spec.id);
                if let Some(quant) = format.quant_token() {
                    assert!(name.contains(quant), "{name}");
                }
                names.push(name);
            }
        }
        let count = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), count, "names must be distinct: {names:?}");
    }

    /// Adoption: the progress text says found files are being checked; the notices name the
    /// re-probed state's Download button; another bundle's adoption is named as such.
    #[test]
    fn adoption_progress_and_notices_are_shown() {
        let _locale = crate::hold_locale_for_test();
        let adopting = progress(ExternalDownloadPhase::Adopting, 5);
        assert_eq!(progress_text(&adopting), tf!("translation.ocr_model.adopting_status", file = "onnx/decoder_prefill_int8.onnx"));
        let running = download_block_view(&snapshot(OcrModelDownloadState::Verifying(adopting)));
        assert_eq!(running.button.map(|button| button.action), Some(ModelDownloadAction::Cancel));
        assert_eq!(running.progress.map(|(_, text)| text), Some(progress_text(&adopting)));

        let mismatch = OcrModelPanelSnapshot { notice: Some(DownloadNotice::AdoptMismatch { file: "a.bin" }), ..snapshot(OcrModelDownloadState::Missing) };
        let caption = offered_download_label(&mismatch).expect("missing offers a download");
        let view = download_block_view(&mismatch);
        assert_eq!(view.lines.last(), Some(&(StatusTone::Warning, tf!("translation.ocr_model.adopt_mismatch_status", file = "a.bin", button = caption.clone()))));
        assert_eq!(view.button, download_button(caption.clone(), true));
        let cancelled = OcrModelPanelSnapshot { notice: Some(DownloadNotice::AdoptCancelled), ..snapshot(OcrModelDownloadState::Missing) };
        assert_eq!(download_block_view(&cancelled).lines.last(), Some(&(StatusTone::Weak, tf!("translation.ocr_model.adopt_cancelled_status", button = caption))));
        let failed = OcrModelPanelSnapshot { notice: Some(DownloadNotice::AdoptFailed("boom".to_owned())), ..snapshot(OcrModelDownloadState::Missing) };
        assert_eq!(download_block_view(&failed).lines.last(), Some(&(StatusTone::Error, tf!("translation.ocr_model.adopt_failed_error", err = "boom"))));
        // Adoption notices go stale once the bundle reads installed (not verify notices).
        assert!(!DownloadNotice::AdoptCancelled.is_verify());

        let other = OtherDownload { specs: vec![&BABERU_OCR], kind: JobKind::Adopt, progress: adopting };
        let name = external_model_display_name(&BABERU_OCR);
        assert_eq!(
            other_download_line(&other),
            tf!("translation.ocr_model.other_adopt_status", name = name, done = format_bytes(5), total = format_bytes(BABERU_OCR.total_bytes()))
        );
    }

    #[test]
    fn progress_fraction_is_clamped_and_safe_on_empty_totals() {
        let mut report = progress(ExternalDownloadPhase::Downloading, 0);
        assert!(progress_fraction(&report).abs() < f32::EPSILON);
        report.total_done = report.total_bytes.saturating_mul(2);
        assert!((progress_fraction(&report) - 1.0).abs() < f32::EPSILON);
        report.total_bytes = 0;
        assert!(progress_fraction(&report).abs() < f32::EPSILON);
    }
}
