/*
File: crates/ms-tab-translation/src/model_download_status.rs

Purpose:
The one owner of how an app-managed model download (`ms_sysprobe::ai_models::ModelDownloadEvent`)
is shown while an OCR load, a text detection or a cleaning run performs it: which event is kept,
its localized text and its per-file progress-bar fraction. Shared by the Translation panels
(OCR, text detector) and the Cleaning tools (mask generation, the region-inpaint editor, the
LaMa / SDXL engines), so every place reads the same.

Key items:
- `ModelDownloadStatus`: the latest event of a running job, cleared once its last file is in.
- `model_download_status_view()`: pure event -> (text, optional fraction) mapping, unit-tested.
- `draw_model_download_status()`: renders that view (bar when the size is known, else a line).

Notes:
A retry is progress, never an error: it reads "network error, retrying in N s (k of n)" in the
normal text colour. Cancel stays with each caller (its own job / token), and so does the
"cancelled" outcome line.
*/

use crate::ocr_model_download::format_bytes;
use ms_sysprobe::ai_models::{ModelDownloadEvent, ModelDownloadPhase};

/// The latest model-download event of one running job, or nothing.
///
/// Owned by whoever owns the job (the tab for OCR / detection, the job itself in Cleaning) and
/// reset with it. Holds nothing once the job's last file is fully staged, so a run that goes on
/// after its download (model load, backend call) stops showing a full download bar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelDownloadStatus {
    current: Option<ModelDownloadEvent>,
}

impl ModelDownloadStatus {
    /// Takes `event` as the job's current download state; a `Downloading` event of the last
    /// file with every announced byte staged ends the shown download instead.
    pub fn record(&mut self, event: ModelDownloadEvent) {
        let finished = event.phase == ModelDownloadPhase::Downloading
            && event.total == Some(event.staged)
            && event.file_index.saturating_add(1) >= event.file_count;
        self.current = if finished { None } else { Some(event) };
    }

    /// Forgets the shown download (the job ended, or moved on to non-download work).
    pub fn clear(&mut self) {
        self.current = None;
    }

    /// The download to show, if one is in progress.
    #[must_use]
    pub fn current(&self) -> Option<&ModelDownloadEvent> {
        self.current.as_ref()
    }
}

/// What one model-download event shows: its text and, when the file size is known, the
/// file's fraction staged (`0.0..=1.0`) for a filled progress bar.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelDownloadStatusView {
    pub text: String,
    pub fraction: Option<f32>,
}

/// File name of a repository path (`ONNX/PaddleOCR/detection/v5/det.onnx` -> `det.onnx`): the
/// directories add length to a side-panel line, not information.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Fraction of `staged` out of `total`, `0.0..=1.0`, or `None` for an unknown or empty total.
/// Integer per-mille first, so the `f32` comes from a lossless `u16` conversion.
fn staged_fraction(staged: u64, total: Option<u64>) -> Option<f32> {
    let total = total.filter(|total| *total > 0)?;
    let per_mille = (u128::from(staged) * 1000 / u128::from(total)).min(1000);
    // `per_mille <= 1000` after the clamp, so it fits in `u16`.
    let per_mille = u16::try_from(per_mille).unwrap_or(1000);
    Some(f32::from(per_mille) / 1000.0)
}

/// Maps `event` to what is shown. Pure (reads only the active i18n catalog), so the mapping
/// is unit-tested without a `Ui`. File numbers are 1-based in the text.
#[must_use]
pub fn model_download_status_view(event: &ModelDownloadEvent) -> ModelDownloadStatusView {
    let file = file_name(&event.file);
    let index = event.file_index.saturating_add(1);
    let count = event.file_count;
    match event.phase {
        ModelDownloadPhase::WaitingForOtherDownload => ModelDownloadStatusView {
            text: tf!("translation.model_download.waiting_status", file = file),
            fraction: None,
        },
        ModelDownloadPhase::Downloading => {
            let fraction = staged_fraction(event.staged, event.total);
            let text = match event.total {
                Some(total) => tf!(
                    "translation.model_download.downloading_status",
                    file = file,
                    index = index,
                    count = count,
                    done = format_bytes(event.staged),
                    total = format_bytes(total)
                ),
                None => tf!(
                    "translation.model_download.downloading_unknown_size_status",
                    file = file,
                    index = index,
                    count = count,
                    done = format_bytes(event.staged)
                ),
            };
            ModelDownloadStatusView { text, fraction }
        }
        ModelDownloadPhase::Retrying { attempt, max_attempts, delay_secs } => ModelDownloadStatusView {
            text: tf!(
                "translation.model_download.retrying_status",
                file = file,
                seconds = delay_secs,
                attempt = attempt,
                max = max_attempts
            ),
            // The bytes staged so far are kept for the retry, so the bar keeps its fill.
            fraction: staged_fraction(event.staged, event.total),
        },
    }
}

/// Draws `event`: a filled progress bar carrying the text when the file size is known, a
/// plain line otherwise. Draws only; the caller owns any Cancel button.
pub fn draw_model_download_status(ui: &mut egui::Ui, event: &ModelDownloadEvent) {
    let view = model_download_status_view(event);
    match view.fraction {
        Some(fraction) => {
            ui.add(egui::ProgressBar::new(fraction).text(view.text));
        }
        None => {
            ui.label(view.text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(phase: ModelDownloadPhase, file_index: usize, staged: u64, total: Option<u64>) -> ModelDownloadEvent {
        ModelDownloadEvent { file: "ONNX/PaddleOCR/detection/v5/det.onnx".to_string(), file_index, file_count: 2, phase, staged, total }
    }

    #[test]
    fn downloading_with_a_known_size_fills_the_bar_and_names_the_file() {
        let view = model_download_status_view(&event(ModelDownloadPhase::Downloading, 0, 512, Some(2048)));
        assert_eq!(
            view.text,
            tf!(
                "translation.model_download.downloading_status",
                file = "det.onnx",
                index = 1,
                count = 2,
                done = format_bytes(512),
                total = format_bytes(2048)
            )
        );
        assert_eq!(view.fraction, Some(0.25));
    }

    #[test]
    fn downloading_with_an_unknown_size_draws_no_bar() {
        let view = model_download_status_view(&event(ModelDownloadPhase::Downloading, 1, 512, None));
        assert_eq!(view.fraction, None);
        assert_eq!(
            view.text,
            tf!("translation.model_download.downloading_unknown_size_status", file = "det.onnx", index = 2, count = 2, done = format_bytes(512))
        );
    }

    /// A retry reads as progress with its delay and attempt, and keeps the staged fill.
    #[test]
    fn retrying_names_the_delay_and_the_attempt() {
        let phase = ModelDownloadPhase::Retrying { attempt: 2, max_attempts: 5, delay_secs: 4 };
        let view = model_download_status_view(&event(phase, 0, 1024, Some(2048)));
        assert_eq!(view.text, tf!("translation.model_download.retrying_status", file = "det.onnx", seconds = 4, attempt = 2, max = 5));
        assert_eq!(view.fraction, Some(0.5));
    }

    #[test]
    fn waiting_for_another_download_is_a_line() {
        let view = model_download_status_view(&event(ModelDownloadPhase::WaitingForOtherDownload, 0, 0, None));
        assert_eq!(view.text, tf!("translation.model_download.waiting_status", file = "det.onnx"));
        assert_eq!(view.fraction, None);
    }

    /// The last file fully staged ends the shown download; an earlier complete file, an
    /// unknown size or a retry at full size does not.
    #[test]
    fn the_status_drops_a_finished_last_file_only() {
        let mut status = ModelDownloadStatus::default();
        status.record(event(ModelDownloadPhase::Downloading, 0, 2048, Some(2048)));
        assert!(status.current().is_some(), "the first of two files ended; the second is still to come");
        status.record(event(ModelDownloadPhase::Downloading, 1, 1024, None));
        assert!(status.current().is_some());
        status.record(event(ModelDownloadPhase::Retrying { attempt: 1, max_attempts: 5, delay_secs: 1 }, 1, 2048, Some(2048)));
        assert!(status.current().is_some());
        status.record(event(ModelDownloadPhase::Downloading, 1, 2048, Some(2048)));
        assert_eq!(status.current(), None);
        status.record(event(ModelDownloadPhase::Downloading, 1, 10, Some(2048)));
        status.clear();
        assert_eq!(status.current(), None);
    }

    #[test]
    fn fractions_are_clamped_and_safe_on_empty_totals() {
        assert_eq!(staged_fraction(10, Some(0)), None);
        assert_eq!(staged_fraction(4096, Some(2048)), Some(1.0));
        assert_eq!(staged_fraction(0, Some(2048)), Some(0.0));
    }
}
