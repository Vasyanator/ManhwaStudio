/*
File: crates/ms-tab-translation/src/panels/ocr_model_download.rs

Purpose:
The OCR panel's download block for an external OCR model (Baberu OCR, the selected
PaddleOCR-VL variant): install status, the outcome of the last unsuccessful download, the
"Download (size)" / "Resume download" button, a progress bar with Cancel while a download
runs (its text also shows the automatic retry countdown after a network failure), and — while ANOTHER model downloads — a line naming it with its progress over a
disabled download button. It only draws and returns a `ModelDownloadAction`; the tab hands
the action to `OcrModelDownloadController`.

Key items:
- `ModelDownloadAction`: what the user asked for this frame.
- `download_block_view()`: pure snapshot -> (lines, progress, button) mapping, unit-tested.
- `draw_ocr_model_download()`: renders that view.
- `load_blocked_reason()`: why the panel's Load button is disabled for the model's state.

Notes:
- Download / resume captions come from `ocr_model_download::offered_download_label`, the
  same owner the OCR "model is not downloaded" error uses, so the error names exactly the
  button drawn here.
*/

use crate::ocr_model_download::{
    DownloadNotice, OcrModelDownloadState, OcrModelPanelSnapshot, OtherDownload, format_bytes, offered_download_label,
};
use ms_sysprobe::ai_models::external::{ExternalDownloadPhase, ExternalDownloadProgress, ExternalModelSpec};
use ms_sysprobe::ai_models::external_catalog::{BABERU_OCR, PaddleVlVariant};

/// The user's request from the download block in this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelDownloadAction {
    /// Nothing clicked.
    #[default]
    None,
    /// Start (or resume) downloading the shown model.
    Download,
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
/// for Baberu OCR, the localized variant label for a PaddleOCR-VL variant.
fn external_model_display_name(spec: &ExternalModelSpec) -> String {
    if spec.id == BABERU_OCR.id {
        // Product name, not translated (the engine selector shows the same literal).
        return "Baberu OCR".to_owned();
    }
    PaddleVlVariant::ALL
        .into_iter()
        .find(|variant| variant.spec().id == spec.id)
        .map_or_else(|| spec.id.to_owned(), |variant| crate::panels::ocr::paddle_vl_variant_label(variant).to_owned())
}

/// The line naming the other model's running download with its byte progress.
fn other_download_line(other: &OtherDownload) -> String {
    tf!(
        "translation.ocr_model.other_download_status",
        name = external_model_display_name(other.spec),
        done = format_bytes(other.progress.total_done),
        total = format_bytes(other.progress.total_bytes)
    )
}

/// Maps the download snapshot of `spec` to what the block shows. Pure (reads only the
/// active i18n catalog), so the mapping is unit-tested without a `Ui`.
fn download_block_view(spec: &ExternalModelSpec, snapshot: &OcrModelPanelSnapshot) -> DownloadBlockView {
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
        OcrModelDownloadState::Downloading(_) => None,
        OcrModelDownloadState::Unavailable(reason) => Some((StatusTone::Error, reason.clone())),
    };
    lines.extend(status);
    match &snapshot.notice {
        Some(DownloadNotice::Cancelled) => lines.push((StatusTone::Weak, t!("translation.ocr_model.cancelled_status").to_string())),
        Some(DownloadNotice::Failed(err)) => lines.push((StatusTone::Error, tf!("translation.ocr_model.failed_error", err = err))),
        None => {}
    }
    let offered = offered_download_label(spec, &snapshot.state);
    // Only a state that offers a download is blocked by another running download.
    if offered.is_some()
        && let Some(other) = &snapshot.other_download
    {
        lines.push((StatusTone::Weak, other_download_line(other)));
    }
    let (progress, button) = if let OcrModelDownloadState::Downloading(progress) = &snapshot.state {
        let cancel = BlockButton {
            caption: t!("translation.common.cancel_button").to_string(),
            action: ModelDownloadAction::Cancel,
            enabled: true,
        };
        (Some((progress_fraction(progress), progress_text(progress))), Some(cancel))
    } else {
        let button = offered.map(|caption| BlockButton {
            caption,
            action: ModelDownloadAction::Download,
            enabled: snapshot.other_download.is_none(),
        });
        (None, button)
    };
    DownloadBlockView { lines, progress, button }
}

/// Why the panel's Load button is disabled for this snapshot of `spec`, or `None` when the
/// model is installed. A snapshot of another spec (taken before an engine / variant switch
/// in this frame) counts as still checking.
pub(crate) fn load_blocked_reason(spec: &ExternalModelSpec, snapshot: Option<&OcrModelPanelSnapshot>) -> Option<String> {
    let Some(snapshot) = snapshot.filter(|snapshot| snapshot.spec_id == spec.id) else {
        return Some(t!("translation.ocr_model.checking_status").to_string());
    };
    match &snapshot.state {
        OcrModelDownloadState::Installed => None,
        OcrModelDownloadState::Unknown | OcrModelDownloadState::Checking => Some(t!("translation.ocr_model.checking_status").to_string()),
        OcrModelDownloadState::Downloading(_) => Some(t!("translation.common.downloading_model_status").to_string()),
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
/// file being hashed while verifying, the retry number and countdown (then "reconnecting")
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

/// Draws the download block of `spec` from `snapshot` and returns the clicked action.
pub(crate) fn draw_ocr_model_download(
    ui: &mut egui::Ui,
    spec: &ExternalModelSpec,
    snapshot: &OcrModelPanelSnapshot,
) -> ModelDownloadAction {
    let view = download_block_view(spec, snapshot);
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
        OcrModelPanelSnapshot { spec_id: BABERU_OCR.id, state, notice: None, other_download: None }
    }

    fn download_button(caption: String, enabled: bool) -> Option<BlockButton> {
        Some(BlockButton { caption, action: ModelDownloadAction::Download, enabled })
    }

    #[test]
    fn missing_and_partial_offer_the_owned_download_caption() {
        for state in [OcrModelDownloadState::Missing, OcrModelDownloadState::Partial { bytes_present: 1024 }] {
            let view = download_block_view(&BABERU_OCR, &snapshot(state.clone()));
            let caption = offered_download_label(&BABERU_OCR, &state).expect("a download state");
            assert_eq!(view.button, download_button(caption, true), "{state:?}");
            assert_eq!(view.lines.len(), 1, "{state:?}");
        }
        // An unfinished download without a notice is reported as interrupted (L2).
        let partial = download_block_view(&BABERU_OCR, &snapshot(OcrModelDownloadState::Partial { bytes_present: 1024 }));
        assert_eq!(partial.lines, vec![(StatusTone::Warning, t!("translation.ocr_model.interrupted_status").to_string())]);
    }

    /// M1: after a cancel / failure the block shows the notice next to the re-probed
    /// state's own button, never a separate "Retry" caption.
    #[test]
    fn notices_keep_the_reprobed_button() {
        let state = OcrModelDownloadState::Partial { bytes_present: 1024 };
        let caption = offered_download_label(&BABERU_OCR, &state).expect("a download state");
        let cancelled = OcrModelPanelSnapshot { notice: Some(DownloadNotice::Cancelled), ..snapshot(state.clone()) };
        let view = download_block_view(&BABERU_OCR, &cancelled);
        assert_eq!(view.lines, vec![(StatusTone::Weak, t!("translation.ocr_model.cancelled_status").to_string())]);
        assert_eq!(view.button, download_button(caption, true));

        let failed = OcrModelPanelSnapshot { notice: Some(DownloadNotice::Failed("boom".to_owned())), ..snapshot(OcrModelDownloadState::Missing) };
        let view = download_block_view(&BABERU_OCR, &failed);
        assert_eq!(view.lines.last(), Some(&(StatusTone::Error, tf!("translation.ocr_model.failed_error", err = "boom"))));
        assert_eq!(view.button, download_button(offered_download_label(&BABERU_OCR, &OcrModelDownloadState::Missing).expect("missing"), true));
    }

    /// M2: another model's download is named with its progress and disables the button.
    #[test]
    fn another_running_download_is_named_and_disables_the_button() {
        let other_spec = PaddleVlVariant::Official16.spec();
        let other = OtherDownload { spec: other_spec, progress: progress(ExternalDownloadPhase::Downloading, 1024) };
        let blocked = OcrModelPanelSnapshot { other_download: Some(other), ..snapshot(OcrModelDownloadState::Missing) };
        let view = download_block_view(&BABERU_OCR, &blocked);
        let expected_line = tf!(
            "translation.ocr_model.other_download_status",
            name = crate::panels::ocr::paddle_vl_variant_label(PaddleVlVariant::Official16),
            done = format_bytes(1024),
            total = format_bytes(BABERU_OCR.total_bytes())
        );
        assert_eq!(view.lines.last(), Some(&(StatusTone::Weak, expected_line)));
        let button = view.button.expect("the download button stays visible");
        assert!(!button.enabled);
        assert_eq!(button.action, ModelDownloadAction::Download);
        // An installed model has nothing to download, so nothing blocks it.
        let installed = OcrModelPanelSnapshot { other_download: Some(other), ..snapshot(OcrModelDownloadState::Installed) };
        let view = download_block_view(&BABERU_OCR, &installed);
        assert_eq!(view.lines.len(), 1);
        assert_eq!(view.button, None);
    }

    #[test]
    fn installed_checking_and_unavailable_offer_no_button() {
        for state in [
            OcrModelDownloadState::Installed,
            OcrModelDownloadState::Checking,
            OcrModelDownloadState::Unknown,
            OcrModelDownloadState::Unavailable("gone".to_owned()),
        ] {
            let view = download_block_view(&BABERU_OCR, &snapshot(state.clone()));
            assert_eq!(view.button, None, "{state:?}");
            assert!(view.progress.is_none(), "{state:?}");
        }
    }

    #[test]
    fn downloading_shows_progress_and_cancel() {
        let half = BABERU_OCR.total_bytes() / 2;
        let view = download_block_view(&BABERU_OCR, &snapshot(OcrModelDownloadState::Downloading(progress(ExternalDownloadPhase::Downloading, half))));
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
        let view = download_block_view(&BABERU_OCR, &snapshot(OcrModelDownloadState::Downloading(waiting)));
        assert_eq!(view.button.map(|button| button.action), Some(ModelDownloadAction::Cancel));
        assert_eq!(view.progress.map(|(_, text)| text), Some(progress_text(&waiting)));
    }

    /// N2 / N3: the Load button's disabled reason follows the state, and a snapshot of a
    /// different spec (stale for one frame after a switch) never counts as installed.
    #[test]
    fn load_blocked_reason_follows_the_state_and_the_spec() {
        let checking = t!("translation.ocr_model.checking_status").to_string();
        assert_eq!(load_blocked_reason(&BABERU_OCR, None), Some(checking.clone()));
        assert_eq!(load_blocked_reason(&BABERU_OCR, Some(&snapshot(OcrModelDownloadState::Installed))), None);
        assert_eq!(load_blocked_reason(&BABERU_OCR, Some(&snapshot(OcrModelDownloadState::Checking))), Some(checking.clone()));
        assert_eq!(
            load_blocked_reason(&BABERU_OCR, Some(&snapshot(OcrModelDownloadState::Downloading(progress(ExternalDownloadPhase::Downloading, 0))))),
            Some(t!("translation.common.downloading_model_status").to_string())
        );
        assert_eq!(
            load_blocked_reason(&BABERU_OCR, Some(&snapshot(OcrModelDownloadState::Missing))),
            Some(t!("translation.ocr_model.not_downloaded_status").to_string())
        );
        let stale = OcrModelPanelSnapshot { spec_id: PaddleVlVariant::Official16.spec().id, ..snapshot(OcrModelDownloadState::Installed) };
        assert_eq!(load_blocked_reason(&BABERU_OCR, Some(&stale)), Some(checking));
    }

    #[test]
    fn display_names_cover_every_external_spec() {
        assert_eq!(external_model_display_name(&BABERU_OCR), "Baberu OCR");
        for variant in PaddleVlVariant::ALL {
            assert_eq!(external_model_display_name(variant.spec()), crate::panels::ocr::paddle_vl_variant_label(variant));
        }
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
