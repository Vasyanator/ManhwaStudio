/*
File: crates/ms-tab-translation/src/tab/background.rs

Purpose:
Drains the tab's background job results while the Translation tab is NOT drawn (another tab is
active), so OCR bubble inserts, machine-translation results and text-detector pages reach the
shared models (`BubblesModel`, `TextMaskModel`) without waiting for the user to come back, and a
detector block-recognition batch keeps dispatching its next request.

Key structures:
- BackgroundWork: which of the tab's jobs are in flight (pure snapshot).

Key functions:
- TranslationTabState::poll_background_work(): the off-draw drain, called by
  `TranslationTab::poll_background` (host.rs) only on frames after one in which the tab was not
  drawn (`tab_hidden_last_frame`).
- tab_hidden_last_frame(): the pure "was the tab drawn in the previous frame" rule.

Notes:
- The drains are the SAME methods `draw_canvas_overlay_top_left` runs, in the same relative order
  (detector events, detector storage, OCR events, MT events, next detector-OCR request). They
  consume their channels, so a frame that drains here and then draws (the tab became active this
  frame) finds nothing left to apply twice; the next-request dispatch is idempotent (it returns
  while a request is active).
- The canvas is first brought up to date with `CanvasView::sync_shared_models_while_hidden`, so
  a result applied through it (MT text, an OCR-made bubble) is written over the current model
  state, and its queued upserts are flushed the same frame by the studio's flush of inactive
  canvases (`ViewSync::flush_inactive_canvases`).
- Workers do not wake the GUI. While a job is in flight the drain asks for another frame after
  `BACKGROUND_POLL_INTERVAL`; when nothing is in flight it touches nothing (no canvas sync, no
  wake), so an idle hidden tab costs one cheap check per frame.
- Draw-only work stays in the draw: canvas selections, the advanced-recognition window, the
  panels, pending MT start actions, settings loads.
*/

use super::TranslationTabState;
use ms_canvas::CanvasView;
use ms_project::ProjectData;
use web_time::Duration;

/// How soon a hidden tab with a job in flight is polled again (workers do not wake the GUI).
/// Modest on purpose: results only need to reach the shared models, nothing is on screen.
const BACKGROUND_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Which of the tab's background jobs are in flight; a job whose last event is still queued
/// counts (each controller clears its busy flag when that event is polled).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct BackgroundWork {
    /// The OCR engine is downloading its model or loading.
    pub(super) ocr_loading: bool,
    /// OCR requests whose result will create a bubble (`pending_bubble_inserts`).
    pub(super) ocr_bubble_inserts: bool,
    /// The detector block-recognition batch (active request, retry wait or queued tasks).
    pub(super) detector_ocr_batch: bool,
    /// A machine-translation run.
    pub(super) machine_translation: bool,
    /// A text-detector run (page batch or region).
    pub(super) text_detection: bool,
    /// A detector storage load / save job.
    pub(super) detection_storage: bool,
}

impl BackgroundWork {
    /// Whether any job is in flight, i.e. whether a hidden tab must drain and stay polled.
    #[must_use]
    pub(super) fn in_flight(self) -> bool {
        let Self { ocr_loading, ocr_bubble_inserts, detector_ocr_batch, machine_translation, text_detection, detection_storage } = self;
        ocr_loading || ocr_bubble_inserts || detector_ocr_batch || machine_translation || text_detection || detection_storage
    }
}

/// Whether the tab was NOT drawn in the frame before `current_frame` (`egui::Context::
/// cumulative_frame_nr`), given the frame of its last draw (`None` = never drawn). A tab drawn in
/// the previous frame is (normally) drawn again in this one and drains in its draw; its canvas
/// also still holds that draw's focus marks, which the hidden-canvas sync would clear.
#[must_use]
pub(crate) fn tab_hidden_last_frame(last_drawn_frame: Option<u64>, current_frame: u64) -> bool {
    last_drawn_frame.is_none_or(|drawn| drawn.saturating_add(1) < current_frame)
}

impl TranslationTabState {
    /// The tab's in-flight background jobs.
    fn background_work(&self) -> BackgroundWork {
        BackgroundWork {
            ocr_loading: self.ocr_controller.state().is_busy(),
            ocr_bubble_inserts: !self.pending_bubble_inserts.is_empty(),
            detector_ocr_batch: self.textdetector_ocr_is_running(),
            machine_translation: self.mt_controller.is_busy(),
            text_detection: self.text_detector_controller.is_busy(),
            detection_storage: self.text_detection_storage_busy,
        }
    }

    /// Off-draw drain of the tab's background results (see the file header). Call only while the
    /// tab is hidden (`tab_hidden_last_frame`); does nothing when no job is in flight.
    pub(crate) fn poll_background_work(&mut self, ctx: &egui::Context, canvas: &mut CanvasView, project: &ProjectData) {
        if !self.background_work().in_flight() {
            return;
        }
        canvas.sync_shared_models_while_hidden(project);
        self.poll_text_detector_events(ctx, canvas, project);
        self.poll_text_detection_storage_events(project);
        self.poll_ocr_events(ctx, canvas, project);
        self.poll_mt_events(ctx, canvas);
        self.maybe_dispatch_next_textdetector_ocr_request(ctx, project);
        if self.background_work().in_flight() {
            ctx.request_repaint_after(BACKGROUND_POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BackgroundWork, tab_hidden_last_frame};

    #[test]
    fn any_single_job_counts_as_in_flight() {
        assert!(!BackgroundWork::default().in_flight());
        let each = [
            BackgroundWork { ocr_loading: true, ..BackgroundWork::default() },
            BackgroundWork { ocr_bubble_inserts: true, ..BackgroundWork::default() },
            BackgroundWork { detector_ocr_batch: true, ..BackgroundWork::default() },
            BackgroundWork { machine_translation: true, ..BackgroundWork::default() },
            BackgroundWork { text_detection: true, ..BackgroundWork::default() },
            BackgroundWork { detection_storage: true, ..BackgroundWork::default() },
        ];
        for work in each {
            assert!(work.in_flight(), "{work:?}");
        }
    }

    #[test]
    fn hidden_means_not_drawn_in_the_previous_frame() {
        assert!(tab_hidden_last_frame(None, 0), "never drawn");
        assert!(tab_hidden_last_frame(None, 10));
        assert!(!tab_hidden_last_frame(Some(9), 10), "drawn last frame: the draw drains");
        assert!(!tab_hidden_last_frame(Some(10), 10), "already drawn this frame");
        assert!(tab_hidden_last_frame(Some(8), 10), "skipped a frame: hidden");
        assert!(!tab_hidden_last_frame(Some(u64::MAX), 0), "no overflow");
    }
}
