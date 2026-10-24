/*
File: src/studio/session/project/mod.rs

Purpose:
`ProjectSession`, the project (chapter) strategy of a studio session: the persistence state that
exists only when a real chapter is open — the save-to-project job and its status line, the save
deferred behind the layer preload, the save-then-close action, the structural page-op job / its
failure / the reload it requests, the DISCARD delete job, and the cached unsaved-changes flag with
its probe timer — plus the per-frame poll of all of them.

Key structures:
- `ProjectSession`
- `SavePoll` + `save_then_close_end`: the pure end of a save-then-close.
- `PendingExitCleanup`: the running staging delete job and the close it ends in.

Key functions:
- `ProjectSession::new`, `tick` (per-frame polls in their fixed order), `busy_jobs` / `activity`
  (the facts `studio/busy.rs` predicates read), `draw_top_bar`.
- Save: `save.rs`; page op: `page_op.rs`; DISCARD: `discard.rs`; exit dialog: `exit_dialog.rs`.

Notes:
Owned struct with a narrow API: it never reaches `StudioApp`; every flow takes the app parts it
needs as one borrowed `SessionParts` (tabs, writers, close intent, project). Each ordered sequence
(save, page op, discard, failed-discard restart, save-then-close) stays inside ONE method. Built only
by `SessionMode::from_project` for a project session; an image-set session has none of this state.
The save-then-close is armed only by an exit dialog's Save and ends with that save, whatever its
outcome (`save_then_close_end`).
*/

mod discard;
mod exit_dialog;
mod page_op;
mod save;
mod staging_probe;

use crate::studio::PendingCloseAction;
use crate::studio::busy::{PersistenceActivity, PersistenceJobs};
use crate::studio::close_flow::SaveThenCloseEnd;
use crate::studio::session::{DeferredTabEdits, SessionParts, canvas_bubble_edits_pending};
use self::staging_probe::StagingDirProbe;
use crate::page_ops::PageOpOutcome;
use crate::runtime_log;
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

/// What `poll_save_to_project` (`save.rs`) saw this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SavePoll {
    /// No save finished this frame (none runs, or its worker is still busy).
    NotFinished,
    /// The merge finished successfully this frame.
    Succeeded,
    /// The save worker reported an error or died this frame; its error is in the status line.
    Failed,
}

/// Pure core of the save-then-close step of [`ProjectSession::tick`], asked only while a
/// save-then-close is armed. `save_pending`: a save worker runs or waits on the layer preload
/// after this frame's poll; `still_unsaved`: the session's unsaved flag after this frame's refresh.
/// `None` keeps waiting. With no save finished and none pending, the save the dialog asked for was
/// refused or aborted before its worker ran, which ends the close as `Failed` — so the arm can never
/// outlive its save and close the app on a later, unrelated save.
fn save_then_close_end(poll: SavePoll, save_pending: bool, still_unsaved: bool) -> Option<SaveThenCloseEnd> {
    match poll {
        SavePoll::Succeeded if still_unsaved => Some(SaveThenCloseEnd::StillUnsaved),
        SavePoll::Succeeded => Some(SaveThenCloseEnd::Saved),
        SavePoll::Failed => Some(SaveThenCloseEnd::Failed),
        SavePoll::NotFinished if save_pending => None,
        SavePoll::NotFinished => Some(SaveThenCloseEnd::Failed),
    }
}

/// Seconds a timed status line stays visible after the app time it was set.
const STATUS_LINE_SECONDS: f64 = 5.0;

/// What [`ProjectSession::tick`] does with the status line this frame (`status_line_step`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusLineStep {
    /// A timed status whose [`STATUS_LINE_SECONDS`] ran out: clear it.
    Expire,
    /// A timed status still visible: wake the GUI again (1 Hz) so it expires without input.
    WakeToExpire,
    /// A sticky status (set with timestamp `0.0`: a running save / cleanup, a refusal or a
    /// worker-start error): it stays until a later status replaces it. Nothing on screen changes
    /// until then, so it schedules no repaint; the jobs whose result replaces it keep their own
    /// poll in `tick`, a refusal is replaced only after input.
    Sticky,
}

/// Pure core of the status-line expiry: `set_at` is the app time (seconds) the status was set
/// (`0.0` = sticky), `now` the current app time.
fn status_line_step(set_at: f64, now: f64) -> StatusLineStep {
    if set_at <= 0.0 {
        StatusLineStep::Sticky
    } else if now - set_at > STATUS_LINE_SECONDS {
        StatusLineStep::Expire
    } else {
        StatusLineStep::WakeToExpire
    }
}

/// The running DISCARD delete job (`discard.rs`) and the close it ends in.
struct PendingExitCleanup {
    action: PendingCloseAction,
    rx: Receiver<Result<(), String>>,
}

/// Persistence state of a project session (see the file header). Field names are those the flows
/// in `save.rs`, `page_op.rs` and `discard.rs` use.
pub(in crate::studio) struct ProjectSession {
    /// Active "save to project" merge job.
    save_to_project_rx: Option<Receiver<Result<save::MergeDone, String>>>,
    /// Status text shown next to the "save to project" button, with the app time it was set
    /// (`0.0` = sticky: kept until replaced, without repaints; see `status_line_step`).
    save_to_project_status: Option<(String, f64)>,
    /// The close a "save then close" ends in, armed ONLY by an exit dialog's Save
    /// (`exit_dialog.rs`), cleared by its Cancel, and taken by `tick` when that save ends
    /// (`save_then_close_end`): it never outlives its save.
    pending_save_completion_action: Option<PendingCloseAction>,
    /// A project save requested while the typing tab's whole-project LAYER preload was still
    /// incomplete, deferred until the preload PASS drains. While true, `start_save_to_project` has
    /// NOT run yet: `request_save_to_project` started the async preload (`begin_preload_all_pages`)
    /// and `drive_pending_save_preload` advances it each frame (regardless of the active tab, since
    /// the typing tab only drives its own preload when it is drawn) then dispatches the real save
    /// once the pass drains (`!preload_all_pages_active`). Gating on pass COMPLETION rather than full
    /// residency is required so a page that genuinely fails to decode (and never becomes resident)
    /// cannot hang the save forever (Finding 1); every page that DID load has its lazy legacy→v3 text
    /// migration baked into the committed `layers.json` (see `flush_text_layers`), and a decode-failed
    /// page keeps its committed text verbatim. Masks and клин are intentionally NOT gated (no save-time
    /// correctness effect — masks are copied verbatim from the committed store, клин edits are
    /// dirty-flushed synchronously). Also feeds `save_busy`, which suppresses concurrent export
    /// dispatch (Finding 2).
    pending_save_after_preload: bool,
    /// In-flight unsaved cleanup that must complete before the app is allowed to close.
    pending_exit_cleanup: Option<PendingExitCleanup>,
    /// Result channel of the quiesced structural page operation worker.
    page_op_rx: Option<Receiver<Result<PageOpOutcome, String>>>,
    /// Error dialog shown after a failed operation; disk state must be reloaded before editing.
    page_op_error: Option<String>,
    /// Consumed by the main window's studio screen (`src/shell/studio/`) to reload the chapter from disk.
    pending_project_reload: bool,
    /// Sticky "the chapter has unsaved changes" flag (see `refresh_unsaved_changes_cache`).
    has_unsaved_changes_cached: bool,
    /// App time (seconds) of the next throttled unsaved-changes probe.
    next_unsaved_dir_check_s: f64,
    /// The off-GUI-thread staging-directory check the throttled refresh asks (`staging_probe.rs`).
    staging_probe: StagingDirProbe,
    /// A save-to-project merge succeeded since the last `take_committed_tree_changed`: the
    /// committed chapter tree changed under the tabs that cache a view of it.
    committed_tree_changed: bool,
}

impl ProjectSession {
    /// The state of a freshly opened chapter: nothing running; unsaved iff a staging dir from an
    /// earlier session existed when it opened (`has_staging`, probed by `StudioBootstrap` off the
    /// GUI thread). `unsaved_dir` is the chapter's staging directory, for the later probes. No I/O,
    /// no thread (the probe worker starts on the first refresh that needs it).
    pub(in crate::studio) fn new(has_staging: bool, unsaved_dir: PathBuf) -> Self {
        Self {
            save_to_project_rx: None,
            save_to_project_status: None,
            pending_save_completion_action: None,
            pending_save_after_preload: false,
            pending_exit_cleanup: None,
            page_op_rx: None,
            page_op_error: None,
            pending_project_reload: false,
            has_unsaved_changes_cached: has_staging,
            next_unsaved_dir_check_s: 0.0,
            staging_probe: StagingDirProbe::new(unsaved_dir),
            committed_tree_changed: false,
        }
    }

    /// Returns and clears the "committed tree changed" latch (set by a successful merge in
    /// `poll_save_to_project`).
    pub(in crate::studio) fn take_committed_tree_changed(&mut self) -> bool {
        std::mem::take(&mut self.committed_tree_changed)
    }

    /// Whether the chapter has changes not yet merged into the project folder (the cached flag).
    pub(in crate::studio) fn has_unsaved_changes(&self) -> bool {
        self.has_unsaved_changes_cached
    }

    /// Latches the cached unsaved flag (the bubbles model changed revision).
    pub(in crate::studio) fn note_unsaved(&mut self) {
        self.has_unsaved_changes_cached = true;
    }

    /// Sets the cached unsaved flag after a save or a discard decided it (`unsaved`), and drops
    /// every staging-probe answer asked before: that answer described the directory as it was
    /// before the merge / delete and must not re-latch the flag.
    fn reset_unsaved_after_persist(&mut self, unsaved: bool) {
        self.has_unsaved_changes_cached = unsaved;
        self.staging_probe.invalidate();
    }

    /// Returns and clears the request to rebuild the app from the project on disk.
    pub(in crate::studio) fn take_reload_request(&mut self) -> bool {
        std::mem::take(&mut self.pending_project_reload)
    }

    /// The close action of the running DISCARD delete job, if any.
    pub(in crate::studio) fn exit_cleanup_action(&self) -> Option<PendingCloseAction> {
        self.pending_exit_cleanup.as_ref().map(|cleanup| cleanup.action)
    }

    /// This session's persistence jobs, for the `studio/busy.rs` predicates.
    pub(in crate::studio) fn busy_jobs(&self) -> PersistenceJobs {
        PersistenceJobs {
            save_running: self.save_to_project_rx.is_some(),
            save_pending_preload: self.pending_save_after_preload,
            page_op_running: self.page_op_rx.is_some(),
            page_op_failed: self.page_op_error.is_some(),
            reload_pending: self.pending_project_reload,
            exit_cleanup_running: self.pending_exit_cleanup.is_some(),
        }
    }

    /// The full busy snapshot (jobs plus the two tab facts), taken where a decision is made.
    fn activity(&self, parts: &SessionParts<'_>) -> PersistenceActivity {
        PersistenceActivity::of(self.busy_jobs(), parts.typing, parts.page_manager)
    }

    /// Per-frame poll of every project job, in the fixed order of `StudioApp::ui`: advance a save
    /// deferred behind the layer preload, poll the save, the page op and the DISCARD cleanup,
    /// refresh the unsaved cache, expire the status line, then — when a save-then-close is armed
    /// and its save ended — close on a clean success, or close the dialog and keep running.
    pub(in crate::studio) fn tick(&mut self, ctx: &egui::Context, now: f64, parts: &mut SessionParts<'_>) {
        // Advance a save deferred behind the whole-project layer preload (started by
        // `request_save_to_project` when not every page was resident). Runs every frame regardless of
        // the active tab and dispatches the real save once every page is resident.
        self.drive_pending_save_preload(ctx, parts);
        let save_poll = self.poll_save_to_project(now, parts);
        self.poll_page_op(ctx);
        self.poll_pending_exit_cleanup(now, parts);
        self.refresh_unsaved_changes_cache(now, parts);
        // Expire a timed status line; a sticky one stays without waking the GUI (`status_line_step`).
        if let Some((_, ts)) = &self.save_to_project_status {
            match status_line_step(*ts, now) {
                StatusLineStep::Expire => self.save_to_project_status = None,
                StatusLineStep::WakeToExpire => ctx.request_repaint_after(web_time::Duration::from_secs(1)),
                StatusLineStep::Sticky => {}
            }
        }
        // The save merge and the DISCARD cleanup workers answer only through their channels and do
        // not wake the GUI: poll them at 1 Hz while they run (their sticky «Saving…» / «Cleaning…»
        // line schedules nothing by itself).
        if self.save_to_project_rx.is_some() || self.pending_exit_cleanup.is_some() {
            ctx.request_repaint_after(web_time::Duration::from_secs(1));
        }

        // A save-then-close armed by an exit dialog's Save ends with its save: close on a clean
        // success, otherwise drop the arm (it never outlives its save) and keep running.
        if let Some(action) = self.pending_save_completion_action {
            let save_pending = self.save_to_project_rx.is_some() || self.pending_save_after_preload;
            if let Some(end) = save_then_close_end(save_poll, save_pending, self.has_unsaved_changes()) {
                self.pending_save_completion_action = None;
                if end == SaveThenCloseEnd::StillUnsaved {
                    let msg = tf!("app.save.temp_not_removed_error", arg = parts.project.paths.unsaved_dir.display());
                    self.save_to_project_status = Some((msg.clone(), now));
                    runtime_log::log_error(format!("[save_to_project] {msg}"));
                }
                // A failed save already put its own error in the status line (`poll_save_to_project`,
                // or the refusal / abort in `request_save_to_project` / `start_save_to_project`).
                parts.close.end_save_then_close(action, end);
            }
        }
    }

    /// The project half of the top bar: «Сохранить проект» (disabled while any `SaveBlocker` holds,
    /// with the blocker's hover text) and the persistence status line.
    pub(in crate::studio) fn draw_top_bar(&mut self, ui: &mut egui::Ui, parts: &mut SessionParts<'_>) {
        let blocker = self.activity(parts).save_blocker();
        let save_response = ui
            .add_enabled(blocker.is_none(), egui::Button::new(t!("app.menu.save_project_button")));
        let save_response = match blocker.and_then(save::save_blocker_text) {
            Some(text) => save_response.on_disabled_hover_text(text),
            None => save_response,
        };
        if save_response.clicked() {
            self.request_save_to_project(parts);
        }
        if let Some((status, _)) = &self.save_to_project_status {
            ui.label(status.as_str());
        }
    }

    /// Refreshes the cached unsaved-changes flag.
    ///
    /// `has_unsaved_changes_cached` is a sticky latch that is only cleared on an explicit
    /// save or discard (`reset_unsaved_after_persist`). While it is already true there is nothing
    /// to recompute, so the per-frame model locks and the staging-dir probe are skipped.
    /// While it is false the work is throttled to roughly once per second via the
    /// `next_unsaved_dir_check_s` timer; this keeps the GUI thread off the shared model
    /// locks every frame while still surfacing new unsaved state within ~1s. The staging
    /// directory itself is never stat-ed here: [`StagingDirProbe`] asks a worker and its answer
    /// is taken on a later frame (it latches the flag when the directory exists).
    fn refresh_unsaved_changes_cache(&mut self, now: f64, parts: &SessionParts<'_>) {
        // The DISCARD path is already committed: the staging dir is being deleted and every pending
        // write was dropped, so there is nothing left to report and recomputing can only do harm.
        // `next_unsaved_dir_check_s` is not advanced while the latch is set, so WITHOUT this guard the
        // very next frame would recompute, re-latch (the typing tab still draws during the cleanup
        // frames and can re-mark itself dirty), and `close_requested` would re-open the exit dialog —
        // the user could never discard-and-exit.
        //
        // The latch is released only if the cleanup FAILS: `poll_pending_exit_cleanup` then calls
        // `abort_discard_after_failed_cleanup`, which clears it and re-reports the session as unsaved,
        // so this fn starts answering honestly again for the app that keeps running. On SUCCESS the
        // latch stays set — that branch dispatches the window close itself, so the quiet is correct.
        // A probe answer asked before the discard describes a directory being deleted: dropped.
        if parts.writers.is_discarding() {
            self.reset_unsaved_after_persist(false);
            return;
        }
        // The answer to the last staging-dir question, asked on an earlier refresh. Taken before the
        // throttle so it latches as soon as it arrives.
        if self.staging_probe.poll() == Some(true) {
            self.has_unsaved_changes_cached = true;
        }
        if self.has_unsaved_changes_cached || now < self.next_unsaved_dir_check_s {
            return;
        }
        self.next_unsaved_dir_check_s = now + 1.0;
        // A DEFERRED tab edit (typing text, PS-editor layer visibility / opacity / collapse, a canvas
        // bubble edit not yet pushed into the bubbles model) is unsaved work that has not reached the
        // disk yet, so the staging-dir probe cannot see it. Ask the one owner of that question
        // (`DeferredTabEdits`, shared with the image set): otherwise an edit made shortly before
        // closing, with no other unsaved state to have created the staging dir, would leave this
        // false and the exit dialog would not offer to save the chapter. Latched here, so it survives
        // the flush that writes it to staging.
        let canvas_bubbles = canvas_bubble_edits_pending(parts.translation_canvas, parts.cleaning, parts.typing);
        if DeferredTabEdits::of(parts.typing, parts.ps_editor, canvas_bubbles).any() {
            self.has_unsaved_changes_cached = true;
            return;
        }
        // Layer writes enqueued to the background saver but not yet acknowledged are unsaved work the
        // staging-dir probe may not see yet. A stale positive (acked, not yet polled) is harmless: an
        // acked write means staging exists, so the probe would answer true as well.
        if let Ok(doc) = parts.writers.layer_doc().lock()
            && doc.has_pending_saves()
        {
            self.has_unsaved_changes_cached = true;
            return;
        }
        if let Ok(model) = parts.writers.bubbles_model().lock()
            && model.has_unsaved_changes()
        {
            self.has_unsaved_changes_cached = true;
            return;
        }
        // `has_unsaved_overlay_changes` covers клин pages the autosave gate still HOLDS after a
        // save-to-project cleared the project flag (edits made during the merge): they are unsaved
        // work, but staging may not exist yet.
        if let Ok(model) = parts.writers.clean_overlays_model().lock()
            && (model.has_project_unsaved_changes() || model.has_unsaved_overlay_changes())
        {
            self.has_unsaved_changes_cached = true;
            return;
        }
        // No in-memory fact says unsaved: whatever else wrote staging (the translation tab and the
        // canvas write some files there directly) shows only in the directory itself. Asked off the
        // GUI thread; a positive answer latches the flag on the frame it arrives (above).
        self.staging_probe.request();
    }
}

#[cfg(test)]
mod tests {
    use super::{ProjectSession, STATUS_LINE_SECONDS, SavePoll, SaveThenCloseEnd, StatusLineStep, save_then_close_end, status_line_step};

    /// A sticky status (timestamp `0.0`) never expires and never asks for a repaint, however late
    /// it is; a timed one wakes the GUI until its time ran out, then expires.
    #[test]
    fn a_sticky_status_never_wakes_the_gui_and_a_timed_one_expires() {
        for now in [0.0, 1.0, 1_000.0, 1.0e9] {
            assert_eq!(status_line_step(0.0, now), StatusLineStep::Sticky, "now = {now}");
        }
        let set_at = 10.0;
        assert_eq!(status_line_step(set_at, set_at), StatusLineStep::WakeToExpire);
        assert_eq!(status_line_step(set_at, set_at + STATUS_LINE_SECONDS), StatusLineStep::WakeToExpire);
        assert_eq!(status_line_step(set_at, set_at + STATUS_LINE_SECONDS + 0.01), StatusLineStep::Expire);
    }

    /// The committed-tree latch is off for a freshly opened chapter, and a set latch is reported
    /// exactly once: `StudioApp::ui` drains it every frame, so one merge notifies the tabs once.
    #[test]
    fn the_committed_tree_latch_is_drained_once() {
        let mut session = ProjectSession::new(false, std::path::PathBuf::new());
        assert!(!session.take_committed_tree_changed());
        session.committed_tree_changed = true;
        assert!(session.take_committed_tree_changed());
        assert!(!session.take_committed_tree_changed());
    }

    /// An armed save-then-close: a clean success closes; a success that left unsaved work, a failed
    /// save, and a save that never ran (refused or aborted before its worker) all end it without a
    /// close; a save still running or deferred keeps it waiting.
    #[test]
    fn save_then_close_ends_with_its_save_whatever_the_outcome() {
        for still_unsaved in [false, true] {
            assert_eq!(save_then_close_end(SavePoll::NotFinished, true, still_unsaved), None, "save still pending");
            assert_eq!(save_then_close_end(SavePoll::NotFinished, false, still_unsaved), Some(SaveThenCloseEnd::Failed), "no save runs: refused or aborted");
            for pending in [false, true] {
                assert_eq!(save_then_close_end(SavePoll::Failed, pending, still_unsaved), Some(SaveThenCloseEnd::Failed));
            }
        }
        for pending in [false, true] {
            assert_eq!(save_then_close_end(SavePoll::Succeeded, pending, false), Some(SaveThenCloseEnd::Saved));
            assert_eq!(save_then_close_end(SavePoll::Succeeded, pending, true), Some(SaveThenCloseEnd::StillUnsaved));
        }
    }
}
