/*
File: src/shell/closing.rs

Purpose:
The Closing screen of the main window: the studio's blocking teardown runs on a worker while
the window stays responsive. The whole `Box<MangaApp>` moves onto the worker (dropping it joins
threads in many places, so draining it in a frame would block), which runs
`MangaApp::shutdown_blocking`, drops the app, then releases the session's `StudioOpenRequest`
(single-image scratch deletion) and reports last.

Key structures:
- `StudioShutdownJob`: the worker (`spawn`, non-blocking `poll`, `finish_blocking` for the
  process-shutdown hook only).
- `ShutdownOutcome`: the teardown's `ShutdownReport`, or "the worker died".
- `ClosingScreen`: the job plus its screen — a spinner and "Saving changes…", or, after a failed
  write, the failure on screen with a Continue button.

Notes:
A spawn failure is logged and the teardown then runs inline on the calling thread (the
behaviour before the Closing screen existed). The payload is handed over through a
`Mutex<Option<_>>` so it can be taken back when the thread never started.
*/

use crate::app::{MangaApp, ShutdownReport};
use crate::runtime_log;
use crate::shell::studio::StudioOpenRequest;
use ms_thread as thread;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How often the Closing screen polls the teardown worker.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What the teardown worker handles: the running app (absent when the studio never ran) and the
/// session's open request (absent when the open worker produced none).
type Payload = (Option<Box<MangaApp>>, Option<StudioOpenRequest>);

/// How the studio teardown ended.
#[derive(Debug)]
pub(crate) enum ShutdownOutcome {
    /// The teardown ran; the report lists what it could not make durable.
    Finished(ShutdownReport),
    /// The worker died without a report (a panic during the teardown; already logged by the
    /// panic hook). Nothing is known about what was written.
    WorkerLost,
}

impl ShutdownOutcome {
    /// `true` when everything was made durable, so the shell continues without asking.
    #[must_use]
    pub(crate) fn is_clean(&self) -> bool {
        match self {
            Self::Finished(report) => report.failed_layer_pages.is_empty() && report.bubbles_error.is_none(),
            Self::WorkerLost => false,
        }
    }
}

/// The blocking studio teardown: `shutdown_blocking`, drop the app (joins its threads), then
/// release the request (deletes a single-image scratch; after the app, so no writer still holds its
/// files). BLOCKING: the teardown worker, or the process-shutdown hook (`AppShell::on_exit`).
pub(crate) fn teardown_blocking(app: Option<Box<MangaApp>>, request: Option<StudioOpenRequest>) -> ShutdownReport {
    let report = match app {
        Some(mut app) => {
            let report = app.shutdown_blocking();
            drop(app);
            report
        }
        None => ShutdownReport::default(),
    };
    if let Some(request) = request {
        request.release();
    }
    report
}

/// State of the teardown worker.
enum JobState {
    /// The worker runs; `rx` delivers its report as its last act.
    Running { rx: Receiver<ShutdownReport>, handle: thread::JoinHandle<()> },
    /// The worker is done: `Some` until `poll` took the outcome.
    Done(Option<ShutdownOutcome>),
}

/// The studio teardown on a named worker thread.
pub(crate) struct StudioShutdownJob {
    state: JobState,
}

impl StudioShutdownJob {
    /// Moves `app` and `request` onto the teardown worker and returns immediately. When the
    /// thread cannot be spawned the failure is logged and the teardown runs inline (blocking the
    /// caller) instead of being skipped.
    pub(crate) fn spawn(app: Option<Box<MangaApp>>, request: Option<StudioOpenRequest>) -> Self {
        let payload: Arc<Mutex<Option<Payload>>> = Arc::new(Mutex::new(Some((app, request))));
        let worker_payload = Arc::clone(&payload);
        let (tx, rx) = mpsc::channel();
        let spawn_result = thread::Builder::new().name("studio-shutdown".to_string()).spawn(move || {
            // The lock only guards the hand-over; a poisoned one still holds a whole `Option`.
            let taken = worker_payload.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(payload) = taken {
                let (app, request) = payload;
                let report = teardown_blocking(app, request);
                if tx.send(report).is_err() {
                    runtime_log::log_warn("[shell] the studio teardown finished after the main window was gone; report dropped");
                }
            }
        });
        match spawn_result {
            Ok(handle) => Self { state: JobState::Running { rx, handle } },
            Err(err) => {
                runtime_log::log_error(format!("[shell] could not start the studio-shutdown thread: {err}; tearing the studio down on the GUI thread"));
                let taken = payload.lock().unwrap_or_else(PoisonError::into_inner).take();
                let report = taken.map(|(app, request)| teardown_blocking(app, request)).unwrap_or_default();
                Self { state: JobState::Done(Some(ShutdownOutcome::Finished(report))) }
            }
        }
    }

    /// The outcome once the worker has finished (non-blocking, one-shot: later calls return
    /// `None`). Requests a repaint after [`POLL_INTERVAL`] while the worker runs, so the caller is
    /// polled again even without input.
    pub(crate) fn poll(&mut self, ctx: &egui::Context) -> Option<ShutdownOutcome> {
        match &mut self.state {
            JobState::Running { rx, .. } => {
                let outcome = match rx.try_recv() {
                    Ok(report) => ShutdownOutcome::Finished(report),
                    Err(TryRecvError::Disconnected) => {
                        runtime_log::log_error("[shell] the studio-shutdown worker exited without a report");
                        ShutdownOutcome::WorkerLost
                    }
                    Err(TryRecvError::Empty) => {
                        ctx.request_repaint_after(POLL_INTERVAL);
                        return None;
                    }
                };
                // The worker sent its report as its last act (or died); its handle is dropped
                // (detached) here, the thread is exiting.
                self.state = JobState::Done(None);
                Some(outcome)
            }
            JobState::Done(outcome) => outcome.take(),
        }
    }

    /// Waits for the worker (BLOCKING): only for the process-shutdown hook (`on_exit`), where no
    /// frame is left to wait in. The report is logged by the teardown itself.
    pub(crate) fn finish_blocking(self) {
        if let JobState::Running { handle, .. } = self.state {
            runtime_log::log_info("[shell] waiting for the studio teardown before the process exits");
            if handle.join().is_err() {
                runtime_log::log_error("[shell] the studio-shutdown worker panicked during process exit");
            }
        }
    }
}

/// The Closing screen: the teardown job and, after a failed teardown, its outcome until the user
/// continues.
pub(crate) struct ClosingScreen {
    job: StudioShutdownJob,
    /// A teardown that could not make everything durable; shown until Continue.
    failure: Option<ShutdownOutcome>,
}

impl ClosingScreen {
    /// Starts the teardown of `app` / `request` (see [`StudioShutdownJob::spawn`]).
    pub(crate) fn start(app: Option<Box<MangaApp>>, request: Option<StudioOpenRequest>) -> Self {
        Self { job: StudioShutdownJob::spawn(app, request), failure: None }
    }

    /// Polls the teardown. Returns `true` once it finished cleanly (the shell continues); a
    /// failed teardown is kept for the screen and returns `false` — the user's Continue
    /// ([`Self::ui`]) finishes it then.
    pub(crate) fn poll(&mut self, ctx: &egui::Context) -> bool {
        let Some(outcome) = self.job.poll(ctx) else {
            return false;
        };
        if outcome.is_clean() {
            return true;
        }
        runtime_log::log_warn(format!("[shell] studio teardown incomplete: {outcome:?}"));
        self.failure = Some(outcome);
        // Show the failure without waiting for input.
        ctx.request_repaint();
        false
    }

    /// Draws the screen; returns `true` when the user pressed Continue after a failure.
    pub(crate) fn ui(&self, ui: &mut egui::Ui) -> bool {
        let mut continue_clicked = false;
        egui::CentralPanel::default().show(ui, |ui| {
            let offset = (ui.available_height() * 0.5 - 60.0).max(0.0);
            ui.add_space(offset);
            ui.vertical_centered(|ui| match &self.failure {
                None => {
                    ui.spinner();
                    ui.add_space(10.0);
                    ui.label(t!("shell.closing.saving_label"));
                }
                Some(outcome) => {
                    for message in failure_messages(outcome) {
                        ui.colored_label(ms_theme::status::ERROR, message);
                        ui.add_space(6.0);
                    }
                    ui.add_space(8.0);
                    if ui.add_sized([220.0, 34.0], egui::Button::new(t!("shell.closing.continue_button"))).clicked() {
                        continue_clicked = true;
                    }
                }
            });
        });
        continue_clicked
    }

    /// Waits for a running teardown (BLOCKING, process-shutdown hook only).
    pub(crate) fn finish_blocking(self) {
        self.job.finish_blocking();
    }
}

/// The user-facing lines of a failed teardown.
fn failure_messages(outcome: &ShutdownOutcome) -> Vec<String> {
    match outcome {
        ShutdownOutcome::Finished(report) => {
            let mut messages = Vec::with_capacity(2);
            if !report.failed_layer_pages.is_empty() {
                messages.push(tf!("shell.closing.save_failed_error", pages = page_numbers(&report.failed_layer_pages)));
            }
            if let Some(error) = &report.bubbles_error {
                messages.push(tf!("shell.closing.bubbles_failed_error", error = error));
            }
            messages
        }
        ShutdownOutcome::WorkerLost => vec![t!("shell.closing.worker_lost_error").to_string()],
    }
}

/// The page list as the user reads it: page indices are 0-based, page numbers start at 1.
fn page_numbers(pages: &[usize]) -> String {
    pages.iter().map(|page| page.saturating_add(1).to_string()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::{ShutdownOutcome, StudioShutdownJob, page_numbers};
    use crate::app::ShutdownReport;

    #[test]
    fn only_a_report_without_failures_is_clean() {
        assert!(ShutdownOutcome::Finished(ShutdownReport::default()).is_clean());
        let pages = ShutdownReport { failed_layer_pages: vec![0], bubbles_error: None };
        assert!(!ShutdownOutcome::Finished(pages).is_clean());
        let bubbles = ShutdownReport { failed_layer_pages: Vec::new(), bubbles_error: Some("disk full".to_string()) };
        assert!(!ShutdownOutcome::Finished(bubbles).is_clean());
        assert!(!ShutdownOutcome::WorkerLost.is_clean());
    }

    #[test]
    fn failed_pages_are_shown_as_one_based_numbers() {
        assert_eq!(page_numbers(&[0, 4, 9]), "1, 5, 10");
    }

    /// A job with nothing to tear down finishes cleanly on its worker; `poll` is one-shot.
    #[test]
    fn empty_job_finishes_cleanly_and_poll_is_one_shot() {
        let ctx = egui::Context::default();
        let mut job = StudioShutdownJob::spawn(None, None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let outcome = loop {
            if let Some(outcome) = job.poll(&ctx) {
                break outcome;
            }
            assert!(std::time::Instant::now() < deadline, "the empty teardown did not finish");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(outcome.is_clean());
        assert!(job.poll(&ctx).is_none());
        job.finish_blocking();
    }
}
