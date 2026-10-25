/*
File: src/studio/session/project/discard.rs

Purpose:
The project session's DISCARD exit («Не сохранять» in an exit dialog): quiesce every staging
writer the discard way, delete the `{chapter}_unsaved` staging dir on a job, then run the close
action; or, when the delete fails, un-latch and restart the writers for the app that keeps running.

Key structures:
- `PendingExitCleanup` (in `mod.rs`): the running delete job and the close it ends in.

Key functions:
- `ProjectSession::start_exit_cleanup`: start the delete job thread (parked) -> on success
  `StagingWriters::quiesce_discarding` -> hand the клин autosave thread to the job (joined BEFORE
  deleting); a refused thread aborts before anything is latched or stopped.
- `ProjectSession::poll_pending_exit_cleanup`: success -> close the dialog, mark both models saved,
  `CloseFlow::finalize`; failure -> `abort_discard_after_failed_cleanup`.

Notes:
Order and rationale: "Save / page-op / discard / exit choreography" in `src/studio/MODULE_README.md`. An
image-set session never runs this (its closes quiesce without a delete job, `SessionMode`).
*/

use super::{PendingExitCleanup, ProjectSession, WakeGuiOnDrop};
use crate::studio::PendingCloseAction;
use crate::studio::session::SessionParts;
use crate::runtime_log;
use ms_thread::{self as thread, JoinHandle};
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

/// The parked delete job of a DISCARD (`spawn_unsaved_delete_job`): `start` releases it with the
/// клин autosave thread to join; `result` carries its verdict.
struct UnsavedDeleteJob {
    start: Sender<Option<JoinHandle<()>>>,
    result: Receiver<Result<(), String>>,
}

impl ProjectSession {
    /// Starts the thread that deletes the unsaved staging folder and reports the result. The thread
    /// PARKS until `start` delivers the клин autosave thread (already told to stop-now by the
    /// caller); it joins that thread BEFORE the delete, so a finishing autosave pass cannot re-create
    /// the staging dir after it was removed, and the GUI thread never waits on it. Spawning first is
    /// what lets a refused thread abort the discard before anything was latched or stopped.
    /// `wake_gui` is held by the thread for its whole run, so its end wakes the GUI's poll.
    ///
    /// # Errors
    /// The OS error when the thread could not be started; nothing ran.
    fn spawn_unsaved_delete_job(unsaved_dir: PathBuf, wake_gui: WakeGuiOnDrop) -> std::io::Result<UnsavedDeleteJob> {
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let (start_tx, start_rx) = std::sync::mpsc::channel::<Option<JoinHandle<()>>>();
        // The handle is dropped: the job is detached and answers only through `tx`.
        let _detached = thread::Builder::new().name("discard-staging".to_owned()).spawn(move || {
            // Declared first so it drops last: after the result was sent, on every return path.
            let _wake_gui = wake_gui;
            // A closed start channel means `start_exit_cleanup` dropped the job without releasing
            // it; nothing was quiesced for it, so it must not delete anything. Dropping `tx` reports
            // that as a lost worker.
            let Ok(overlay_autosave_thread) = start_rx.recv() else {
                runtime_log::log_error("[exit] discard cleanup: released without a start signal; nothing deleted");
                return;
            };
            if let Some(handle) = overlay_autosave_thread
                && handle.join().is_err()
            {
                runtime_log::log_error("[exit] overlay autosave worker panicked during discard");
            }
            let result = if unsaved_dir.exists() {
                fs::remove_dir_all(&unsaved_dir).map_err(|err| {
                    tf!("app.save.remove_temp_error", unsaved_dir = unsaved_dir.display(), err = err)
                })
            } else {
                Ok(())
            };
            if let Err(err) = tx.send(result) {
                // The GUI dropped the receiver (the app is being torn down); the cleanup itself
                // already ran, only its report has nowhere to go.
                runtime_log::log_warn(format!("[app] discard cleanup: result not delivered, receiver gone: {err}"));
            }
        })?;
        Ok(UnsavedDeleteJob { start: start_tx, result: rx })
    }

    /// Starts the DISCARD exit: starts the staging-dir delete job, then drops pending edits and
    /// quiesces every staging writer (`StagingWriters::quiesce_discarding`) and releases the job;
    /// `action` runs once the delete succeeds (`poll_pending_exit_cleanup`).
    ///
    /// When the OS refuses the job thread, the discard is refused BEFORE anything changed: nothing
    /// is latched, no writer is stopped and no edit dropped, the dialog closes and the status line
    /// reports the failure (as a failed cleanup does); the session keeps running unsaved and the
    /// next close offers save/discard again.
    pub(super) fn start_exit_cleanup(&mut self, action: PendingCloseAction, parts: &mut SessionParts<'_>) {
        if self.activity(parts).exit_cleanup_running() {
            return;
        }
        let job = match Self::spawn_unsaved_delete_job(parts.project.paths.unsaved_dir.clone(), self.job_waker("discard-staging")) {
            Ok(job) => job,
            Err(err) => {
                runtime_log::log_error(format!(
                    "[exit] discard refused: the staging cleanup thread could not be started: {err}. Possible cause: the OS thread limit or low memory. Nothing was discarded or stopped; '{}' is intact and the session stays unsaved",
                    parts.project.paths.unsaved_dir.display()
                ));
                parts.close.close_dialog();
                self.has_unsaved_changes_cached = true;
                self.save_to_project_status = Some((t!("app.cleanup.worker_start_error").to_string(), 0.0));
                return;
            }
        };
        parts.writers.quiesce_discarding(parts.typing);
        if let Err(unsent) = job.start.send(parts.writers.take_overlay_thread()) {
            // Unreachable while the job is parked on `recv` (it returns only after receiving); if
            // it happens anyway, `result` is disconnected and `poll_pending_exit_cleanup` takes the
            // lost-worker path (un-latch + restart). The клин thread was told to stop-now and writes
            // nothing more, so dropping its handle detaches a thread that is already exiting.
            runtime_log::log_error(format!(
                "[exit] discard cleanup job gone before its start signal; the cleanup is reported as failed (autosave handle present={})",
                unsent.0.is_some()
            ));
        }
        self.pending_exit_cleanup = Some(PendingExitCleanup { action, rx: job.result });
        self.save_to_project_status = Some((t!("app.cleanup.cleaning").to_string(), 0.0));
    }

    /// Un-latches the DISCARD path after its staging cleanup FAILED, handing a coherent state back to
    /// the app that is about to keep running.
    ///
    /// `start_exit_cleanup` latches the discard and silences every persistence path on the
    /// assumption that the process ends moments later. When the cleanup fails instead, that
    /// assumption is void and the latch becomes a permanent lie: `refresh_unsaved_changes_cache` would
    /// pin `has_unsaved_changes_cached` to false forever (no unsaved-changes prompt on any later close)
    /// and `shutdown_blocking` would skip the typing text flush for the rest of the session, so every later
    /// deferred edit would be dropped without a word.
    ///
    /// The session is re-reported as UNSAVED rather than recomputed. The discard did not complete, so
    /// the staging dir is in an unknown, possibly partial state (a `remove_dir_all` failure means it is
    /// still there) while the shared document still holds every text edit of the session. A recompute
    /// could answer "clean" — `start_exit_cleanup` has already dropped the typing tab's pending-edit
    /// flags — and claiming a clean session right after a failed discard is exactly the false negative
    /// that hides the exit dialog. Being sticky-unsaved is the conservative, correct direction: the
    /// next close offers save/discard again.
    ///
    /// Every `_unsaved` writer the discard stopped is restarted on the app's autosave gate
    /// (`StagingWriters::restart_after_failed_discard`).
    fn abort_discard_after_failed_cleanup(&mut self, parts: &mut SessionParts<'_>) {
        parts.writers.restart_after_failed_discard();
        self.has_unsaved_changes_cached = true;
    }

    pub(super) fn poll_pending_exit_cleanup(&mut self, now: f64, parts: &mut SessionParts<'_>) {
        let Some(cleanup) = self.pending_exit_cleanup.as_ref() else {
            return;
        };
        match cleanup.rx.try_recv() {
            Ok(Ok(())) => {
                let action = cleanup.action;
                self.pending_exit_cleanup = None;
                parts.close.close_dialog();
                if let Ok(mut model) = parts.writers.bubbles_model().lock() {
                    model.mark_saved_to_project();
                }
                if let Ok(mut model) = parts.writers.clean_overlays_model().lock() {
                    model.mark_saved_to_project();
                }
                self.reset_unsaved_after_persist(false);
                self.save_to_project_status = Some((t!("app.cleanup.cleaned").to_string(), now));
                runtime_log::log_info("[exit] unsaved cleanup complete");
                parts.close.finalize(action);
            }
            Ok(Err(err)) => {
                self.pending_exit_cleanup = None;
                parts.close.close_dialog();
                // The discard did not happen and the window is NOT being closed: control returns to a
                // running app, so the discard latch must go with it.
                self.abort_discard_after_failed_cleanup(parts);
                let msg = tf!("app.cleanup.error", err = err);
                self.save_to_project_status = Some((msg.clone(), now));
                runtime_log::log_error(format!(
                    "[exit] {msg}. The staging dir was not removed; the discard latch is released and \
                     the session is reported as unsaved again so the next close offers save/discard."
                ));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending_exit_cleanup = None;
                // Same as above: the cleanup worker died without a verdict, the app keeps running.
                self.abort_discard_after_failed_cleanup(parts);
                let msg = t!("app.cleanup.thread_crashed").to_string();
                self.save_to_project_status = Some((msg.clone(), now));
                runtime_log::log_error(format!(
                    "[exit] {msg} The staging dir's state is unknown; the discard latch is released \
                     and the session is reported as unsaved again."
                ));
            }
        }
    }
}
