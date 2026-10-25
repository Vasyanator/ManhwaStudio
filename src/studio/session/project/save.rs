/*
File: src/studio/session/project/save.rs

Purpose:
The project session's save-to-project: the trigger (toolbar button, both exit dialogs), the
deferral behind the typing tab's whole-project layer preload, the merge worker, and its poll.

Key structures:
- `SaveGateDecision` + `save_trigger_decision`, `deferred_save_ready`: the pure trigger gates.
- `save_blocker_text`: the user-visible text of a `SaveBlocker` (status line, hover texts).
- `MergeDone`: what a successful merge worker reports (whether staging exists after it).

Key functions:
- `ProjectSession::request_save_to_project` (refused on any `SaveBlocker`, then save now or defer),
  `drive_pending_save_preload` (per frame), `start_save_to_project` (ONE method, fixed order: canvas
  bubble flush (all three canvases) -> PS flush -> typing flush (abort on `Err` / failed pages) -> worker: bubbles barrier + hold -> layer
  barrier (abort on a failed owned text write) -> клин pause + snapshots + staging write -> merge
  -> resume -> bubbles re-barrier), `poll_save_to_project` (success: gate `force_flush`, bubbles
  `mark_saved_to_project_as_of` the revision read when the save started, clean overlays
  `mark_saved_to_project`, the committed-tree latch set — drained by `StudioApp::ui` into
  `StudioTab::on_committed_tree_changed` — cache set from the worker's post-merge staging stat,
  `MergeDone::staging_exists`, so the GUI thread never stats the directory).

Notes:
Order and abort rules: "Save / page-op / discard / exit choreography" in `src/studio/MODULE_README.md`.
The save-then-close that follows a completed save is `ProjectSession::tick` (`mod.rs`).
*/

use super::{ProjectSession, SavePoll};
use crate::studio::busy::SaveBlocker;
use crate::studio::session::SessionParts;
use crate::models::clean_overlays_model::save_overlay_snapshots_guarded;
use crate::runtime_log;
use eframe::egui;
use ms_thread as thread;
use std::collections::HashSet;
use std::sync::Arc;

/// What the save worker reports for a successful merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MergeDone {
    /// `BubblesModel::revision` at the start of the save (read on the GUI thread, echoed by the
    /// worker): a model revision past it on receipt means a bubble edit the merge did not include.
    pub(super) saved_bubbles_revision: u64,
    /// Whether the chapter's staging directory exists once the merge and the post-merge bubbles
    /// barrier are done: an edit accepted during the merge re-created it, so the session is still
    /// unsaved. Stat-ed by the worker so the GUI-side poll needs no file access.
    pub(super) staging_exists: bool,
}

impl ProjectSession {
    /// Entry point for every "save to project" trigger (the toolbar button and both exit-dialog
    /// "save chapter" buttons). Gates the save behind the whole-project layer preload so every page
    /// that can load has its lazy legacy→v3 text migration baked into the committed `layers.json`.
    ///
    /// Fast path: when every page is already resident (`all_pages_loaded`), it runs
    /// `start_save_to_project` immediately (no behavior change from before). Otherwise it DEFERS:
    /// it starts the async, non-blocking preload (`begin_preload_all_pages`) and sets
    /// `pending_save_after_preload`; `drive_pending_save_preload`, called every frame, advances the
    /// preload and dispatches the real save once the PASS drains (not full residency — a page that
    /// fails to decode never becomes resident, so a residency gate would hang; its committed text is
    /// preserved verbatim). A save already running or already pending on the preload is a no-op
    /// (idempotent trigger).
    ///
    /// Only LAYER residency is gated: masks are copied verbatim from the committed `text_images/` by
    /// the merge (no in-memory mask data is consumed), and клин edits are captured synchronously via
    /// `take_dirty_save_snapshots`, so gating on the mask loader or the клин loader would have no
    /// save-time correctness effect (CLAUDE.md §14).
    pub(in crate::studio) fn request_save_to_project(&mut self, parts: &mut SessionParts<'_>) {
        // A save is refused while anything that blocks a page op runs (`PersistenceActivity::
        // save_blocker`): a page op owns the trees; a clean attach/detach worker mutates the overlays
        // model and moves clean files (merging mid-operation could persist a half-applied state); the
        // DISCARD delete job is removing the staging tree a save would flush into and merge; an export
        // reads the layer document the save flushes. The user retries once it is over.
        // A save already under way falls through to `save_trigger_decision`, which ignores it.
        match self.activity(parts).save_blocker() {
            None | Some(SaveBlocker::SaveInFlight) => {}
            Some(blocker @ (SaveBlocker::PageOperation | SaveBlocker::CleanOperation | SaveBlocker::ExitCleanup | SaveBlocker::Export)) => {
                runtime_log::log_info(format!("[save_to_project] trigger ignored: {blocker:?} in progress"));
                if let Some(text) = save_blocker_text(blocker) {
                    self.save_to_project_status = Some((text.to_string(), 0.0));
                }
                return;
            }
        }
        match save_trigger_decision(
            self.save_to_project_rx.is_some(),
            self.pending_save_after_preload,
            parts.typing.all_pages_loaded(parts.project),
        ) {
            SaveGateDecision::Ignore => {}
            SaveGateDecision::SaveNow => self.start_save_to_project(parts),
            SaveGateDecision::Defer => {
                // Start the whole-project layer preload (idempotent; wires the typing loader dirs if
                // the tab was never visited) and remember the intent. `drive_pending_save_preload`
                // completes it from the frame loop regardless of which tab is active.
                parts.typing.begin_preload_all_pages(parts.project);
                self.pending_save_after_preload = true;
                let (done, total) = parts.typing.preload_all_pages_progress();
                self.save_to_project_status = Some((
                    tf!("app.save.preparing_pages", done = done, total = total),
                    0.0,
                ));
            }
        }
    }

    /// Per-frame driver for a save deferred behind the whole-project layer preload. Advances the
    /// preload (the typing tab drives its own preload only while it is the active tab, so the save
    /// path must drive it here to stay correct on any tab), keeps repainting so the decode results
    /// drain, and dispatches the real `start_save_to_project` once the preload PASS has drained
    /// (`deferred_save_ready`, NOT full residency — see that fn for why). No-op unless
    /// `pending_save_after_preload`.
    pub(super) fn drive_pending_save_preload(&mut self, ctx: &egui::Context, parts: &mut SessionParts<'_>) {
        if !self.pending_save_after_preload {
            return;
        }
        // Advance the preload from the frame loop (independent of the active tab) and keep the frame
        // loop alive so decode results keep draining to completion.
        parts.typing.drive_page_preload();
        ctx.request_repaint();
        if deferred_save_ready(parts.typing.preload_all_pages_active()) {
            // The preload PASS has drained (every page that could decode is resident): run the real
            // save now. `flush_text_layers` flushes every resident page, so the merge is authoritative
            // for them and the committed `layers.json` becomes v3-complete. A page that genuinely
            // failed to decode never became resident; the merge keeps its committed text verbatim
            // (it is simply absent from `owned_text_pages`), so the save stays correct without hanging.
            self.pending_save_after_preload = false;
            self.start_save_to_project(parts);
        } else {
            // Still preparing: refresh the "Подготовка страниц N/M" status.
            let (done, total) = parts.typing.preload_all_pages_progress();
            self.save_to_project_status =
                Some((tf!("app.save.preparing_pages", done = done, total = total), 0.0));
        }
    }

    /// Snapshot dirty overlay pages, save them in a background thread, then merge the
    /// unsaved staging dir into the project folder and remove the staging dir on completion.
    ///
    /// ABORTS (reporting a save failure and leaving the staging dir and the dirty state untouched)
    /// whenever the typing text flush cannot be shown to have persisted the live state: the flush
    /// could not run, a resident page failed to enqueue, or the merge worker's barrier reports a
    /// failed text write for an owned page. The merge is destructive — it deletes the staging dir —
    /// and its `owned_text_pages` contract makes an unverified empty set indistinguishable from
    /// "this session owns nothing", so proceeding on one silently discards the user's newest text.
    fn start_save_to_project(&mut self, parts: &mut SessionParts<'_>) {
        if self.save_to_project_rx.is_some() {
            return;
        }
        // Push every canvas's pending bubble edits (debounced text included) into the bubbles model
        // first, so the worker's bubbles-saver barrier below covers the newest bubble text.
        parts.flush_canvas_bubble_edits();
        // Flush the PS editor's active-page raster layers into the staging dir so the merge below
        // picks them up. Other visited pages were already written on their page switch.
        parts.ps_editor.flush_layers(parts.project);
        // Flush the typing tab's text overlays (inline v3 payload) into the staging `layers.json` so a
        // legacy chapter that was only viewed (no edits → no placement save) still migrates its text
        // before the unsaved→committed merge. The typing tab owns text persistence now. The returned set
        // is the OWNED text pages (doc-resident this session) — the merge replaces those pages' text and
        // preserves committed text for pages NOT in it (no silent drop on a PS raster-only edit).
        //
        // The owned set is only meaningful if the flush RAN. Degrading an `Err` into an empty set is
        // not the safe outcome it looks like: an empty set tells the merge that this session owns no
        // page's text, so it preserves the COMMITTED text everywhere, drops whatever staging holds, and
        // then deletes the staging dir — and it reports success. Since text edits are deferred, the
        // newest text may exist ONLY in memory, and staging is the only on-disk copy of every earlier
        // deferred write (and of anything recovered from a crashed session). Abort instead: nothing is
        // merged, staging is untouched, the dirty state survives, and the user is told the save did not
        // happen.
        //
        // `NoLayersDir`/`NoLayerDoc` here would mean a project with no pages at all: any real save
        // wires the typing loader first — either `all_pages_loaded` was true (which requires the loader
        // to have run) or the deferred path called `begin_preload_all_pages`, which wires it. Refusing
        // such a save is harmless; weakening this guard to wave those variants through would re-open
        // the silent-destruction hole for a session that recovered a staging dir without opening the
        // Text tab.
        let owned_text_pages = match parts.typing.flush_text_layers() {
            Ok(outcome) if outcome.failed_pages == 0 => outcome.owned_pages,
            // Same class, one step earlier than the barrier check in the worker below: a resident page
            // whose enqueue FAILED is absent from `owned_pages`, so the merge would preserve that
            // page's stale committed text, delete staging, and report success — while the edit the
            // user just made is still only in memory.
            Ok(outcome) => {
                runtime_log::log_error(format!(
                    "[save_to_project] aborted: {} of {} resident page(s) could not enqueue their text \
                     to the layer saver (cause logged per page). Nothing was merged, '{}' is intact, \
                     and the session stays unsaved — merging now would have preserved stale committed \
                     text for those pages and then deleted the staging dir.",
                    outcome.failed_pages,
                    outcome.failed_pages + outcome.owned_pages.len(),
                    parts.project.paths.unsaved_dir.display()
                ));
                self.save_to_project_status = Some((
                    tf!("app.save.error", err = t!("app.save.text_not_written")),
                    0.0,
                ));
                return;
            }
            Err(err) => {
                runtime_log::log_error(format!(
                    "[save_to_project] aborted: the typing text flush could not run: {err}. Nothing \
                     was merged and '{}' is intact; the session's text edits are still live in memory \
                     and remain unsaved.",
                    parts.project.paths.unsaved_dir.display()
                ));
                self.save_to_project_status =
                    Some((tf!("app.save.error", err = tf!("app.save.text_flush_error", err = err)), 0.0));
                return;
            }
        };
        // Capture the background layer-saver handle on the GUI thread (cheap Sender clone; no doc lock
        // held during the merge). The PS `flush_layers` + typing `flush_text_layers` above now ENQUEUE
        // their page writes; the merge worker must BARRIER the saver before reading the staging
        // `layers.json` so every enqueued page is on disk first. The FIFO channel guarantees the
        // just-enqueued jobs precede the barrier. `None` ⇒ no saver enabled, so the flushes already
        // wrote synchronously and no barrier is needed.
        let saver_handle = parts
            .writers
            .layer_doc()
            .lock()
            .ok()
            .and_then(|guard| guard.saver_handle());
        // The worker barriers the bubble saver and holds post-barrier snapshots until the merge
        // finishes. This makes the committed bubbles file a precise snapshot while edits made during
        // the save remain queued for fresh staging after the destructive merge removes the old tree.
        // The revision read here (GUI thread, after the canvas flush above: every edit up to it has
        // already sent its snapshot, so the first barrier below writes it into the merged staging)
        // is the bubble state this save commits; `poll_save_to_project` compares the model with it.
        let (bubbles_saver_handle, saved_bubbles_revision) = match parts.writers.bubbles_model().lock() {
            Ok(model) => (model.saver_handle(), model.revision()),
            Err(_) => {
                runtime_log::log_error(
                    "[save_to_project] aborted: bubbles model lock poisoned; saver barrier unavailable",
                );
                self.save_to_project_status = Some((
                    tf!("app.save.error", err = t!("app.save.thread_crashed")),
                    0.0,
                ));
                return;
            }
        };
        let unsaved_dir = parts.project.paths.unsaved_dir.clone();
        let project_dir = parts.project.paths.project_dir.clone();
        let unsaved_clean_layers_dir = parts.project.paths.unsaved_clean_layers_dir.clone();
        let clean_overlays_model = Arc::clone(parts.writers.clean_overlays_model());
        // The dirty клин snapshots are taken INSIDE the worker, under an autosave pause: taking them
        // here, with the autosave running, let a pass that had just taken the dirty set land its pages
        // in a staging dir re-created after the merge deleted it, while the app reported "saved".
        let overlay_autosave_control = parts.writers.overlay_control();
        let (tx, rx) = std::sync::mpsc::channel::<Result<MergeDone, String>>();
        let wake_gui = self.job_waker("save-to-project");
        let spawned = thread::Builder::new().name("save-to-project".to_owned()).spawn(move || {
            // Declared first so it drops last: after the result was sent, on every return path.
            let _wake_gui = wake_gui;
            // Hold later bubble snapshots until every destructive staging operation below is done.
            // Dropping this guard on any return path resumes the saver.
            let bubbles_barrier = match bubbles_saver_handle.barrier_and_hold_blocking() {
                Ok(guard) => guard,
                Err(err) => {
                    runtime_log::log_error(format!(
                        "[save_to_project] aborted: bubbles saver barrier failed: {err}. Nothing was merged and '{}' is intact",
                        unsaved_dir.display()
                    ));
                    if tx.send(Err(t!("app.save.thread_crashed").to_string())).is_err() {
                        runtime_log::log_error(
                            "[save_to_project] result receiver dropped before the bubbles barrier failure could be reported",
                        );
                    }
                    return;
                }
            };
            // Drain the layer saver FIRST: block until every enqueued layer page write (rasters + text
            // + effects from the flushes above) is on disk, so the merge reads a complete staging
            // `layers.json`. This barrier runs in the worker, never on the GUI thread.
            let failed_pages = saver_handle.map_or_else(HashSet::new, |handle| handle.barrier_blocking());
            // The barrier reports the pages whose latest TEXT write FAILED. For a page this session
            // OWNS, neither merge option is correct: taking staging writes text that never landed, and
            // dropping the page from the owned set preserves the committed text the user just edited
            // away. Either way the merge would then delete the staging dir and the save would report
            // success, while the newer text — post-deferral, possibly only ever in memory — is gone.
            // Abort with staging intact instead of degrading silently.
            let mut unwritten_owned: Vec<usize> =
                owned_text_pages.intersection(&failed_pages).copied().collect();
            unwritten_owned.sort_unstable();
            if !unwritten_owned.is_empty() {
                runtime_log::log_error(format!(
                    "[save_to_project] aborted: the layer saver barrier reported a FAILED text write \
                     for {} owned page(s): {unwritten_owned:?} (cause logged per page by the saver). \
                     Nothing was merged and '{}' is intact, so the session keeps its edits and stays \
                     unsaved.",
                    unwritten_owned.len(),
                    unsaved_dir.display()
                ));
                if tx.send(Err(t!("app.save.text_not_written").to_string())).is_err() {
                    runtime_log::log_error(
                        "[save_to_project] result receiver dropped before the text-write abort could be reported",
                    );
                }
                return;
            }
            // Every owned page's text is on disk: the set is authoritative for the merge as it stands.
            let effective_owned_text_pages: HashSet<usize> = owned_text_pages;
            // Quiesce the клин autosave (waits out an in-flight pass) and keep it paused until the
            // merge has finished, so no autosave write can land in staging during or after the merge.
            // Blocking is fine: this is the save worker thread. The guard drops on every return path.
            let overlay_autosave_pause = overlay_autosave_control.pause_blocking();
            let dirty_overlay_snapshots = match clean_overlays_model.lock() {
                Ok(mut overlays) => overlays.take_dirty_save_snapshots(),
                Err(_) => {
                    runtime_log::log_error(format!(
                        "[save_to_project] aborted: clean-overlay model lock poisoned; dirty клин pages \
                         cannot be captured. Nothing was merged and '{}' is intact",
                        unsaved_dir.display()
                    ));
                    if tx.send(Err(t!("app.save.thread_crashed").to_string())).is_err() {
                        runtime_log::log_error(
                            "[save_to_project] result receiver dropped before the overlay lock failure could be reported",
                        );
                    }
                    return;
                }
            };
            if let Err(err) = save_overlay_snapshots_guarded(
                &unsaved_clean_layers_dir,
                &dirty_overlay_snapshots,
                &clean_overlays_model,
            ) {
                if let Ok(mut overlays) = clean_overlays_model.lock() {
                    overlays.restore_dirty_save_snapshots(&dirty_overlay_snapshots);
                }
                if tx.send(Err(format!(
                    "failed to flush dirty overlays into '{}': {err}",
                    unsaved_clean_layers_dir.display()
                ))).is_err() {
                    runtime_log::log_error(
                        "[save_to_project] result receiver dropped after overlay flush failure",
                    );
                }
                return;
            }
            // The generic chapter merge lives in `ms_project::save_merge`; the binary injects the
            // per-page layer-manifest merge because only it knows which pages' text it owns.
            let result = crate::project::save_merge::merge_unsaved_into_project(&unsaved_dir, &project_dir, |committed, staging| {
                crate::models::layer_model::persist::merge_unsaved_layers_into_committed(committed, staging, &effective_owned_text_pages).map(|_wrote| ())
            });
            // The merge is done: клин edits made meanwhile stayed dirty in the model and the resumed
            // autosave writes them to fresh staging.
            drop(overlay_autosave_pause);
            // Resume before reporting completion, then issue a second barrier so any edit accepted
            // during the merge is on fresh staging before the GUI clears/recomputes dirty state.
            drop(bubbles_barrier);
            let resumed_barrier = bubbles_saver_handle.barrier_and_hold_blocking();
            if let Ok(guard) = resumed_barrier {
                drop(guard);
            } else if result.is_ok() {
                runtime_log::log_error(
                    "[save_to_project] committed merge completed, but post-merge bubbles staging could not be verified",
                );
                if tx.send(Err(t!("app.save.thread_crashed").to_string())).is_err() {
                    runtime_log::log_error(
                        "[save_to_project] result receiver dropped after post-merge bubbles barrier failure",
                    );
                }
                return;
            }
            // Stat-ed after the re-barrier, so a bubble edit accepted during the merge is already on
            // the fresh staging it re-created (the answer the GUI used to stat for itself).
            let result = result.map(|()| MergeDone { saved_bubbles_revision, staging_exists: unsaved_dir.exists() });
            if tx.send(result).is_err() {
                runtime_log::log_error(
                    "[save_to_project] result receiver dropped before completion was reported",
                );
            }
        });
        // The handle is dropped: the worker is detached and answers only through `tx`.
        if let Err(err) = spawned {
            // Same outcome as the GUI-side aborts above: everything done so far only ENQUEUED
            // staging writes, so nothing was merged, no barrier is held, the gate and both models
            // are untouched and the session stays unsaved. No `rx` is stored, so the save is not
            // "running" and a save-then-close ends as failed (`save_then_close_end`).
            runtime_log::log_error(format!(
                "[save_to_project] aborted: the save worker thread could not be started: {err}. Possible cause: the OS thread limit or low memory. Nothing was merged and '{}' is intact; the session stays unsaved",
                parts.project.paths.unsaved_dir.display()
            ));
            self.save_to_project_status = Some((t!("app.save.worker_start_error").to_string(), 0.0));
            return;
        }
        self.save_to_project_rx = Some(rx);
        self.save_to_project_status = Some((t!("app.save.saving").to_string(), 0.0));
    }

    /// Polls the in-flight "save to project" job and reports whether it finished this frame, and
    /// how; a failure leaves its own error in the status line.
    pub(super) fn poll_save_to_project(&mut self, now: f64, parts: &mut SessionParts<'_>) -> SavePoll {
        let Some(rx) = self.save_to_project_rx.as_ref() else {
            return SavePoll::NotFinished;
        };
        match rx.try_recv() {
            Ok(Ok(done)) => {
                self.save_to_project_rx = None;
                // Everything held was forced out and merged: close the gate's window so the interval
                // and action count restart from the next edit. Abort paths leave the gate alone.
                parts.writers.gate().force_flush();
                // A bubble edit made after the save started is not in the merge, whether or not its
                // queued snapshot has reached staging yet: the revision decides, not file timing.
                let bubbles_advanced = match parts.writers.bubbles_model().lock() {
                    Ok(mut model) => {
                        model.mark_saved_to_project_as_of(done.saved_bubbles_revision, done.staging_exists);
                        model.revision() != done.saved_bubbles_revision
                    }
                    // Unknown: stay unsaved (conservative).
                    Err(_) => true,
                };
                if let Ok(mut model) = parts.writers.clean_overlays_model().lock() {
                    model.mark_saved_to_project();
                }
                // The merge rewrote the committed tree (`clean_layers/` among it): tabs that cache a
                // view of it (the Cleaning clean-folder status, the page manager's clean cards)
                // cannot see that by themselves. Latched here, drained by `StudioApp::ui` right
                // after this tick (`StudioTab::on_committed_tree_changed`), before any tab draws.
                self.committed_tree_changed = true;
                // Staging re-created during the merge, or a bubble edit since the save started, keeps
                // the session unsaved (a save-then-close then ends as `StillUnsaved`); otherwise this
                // frame's refresh re-checks the in-memory facts and asks the staging probe afresh.
                self.reset_unsaved_after_persist(post_save_unsaved(done, bubbles_advanced));
                self.save_to_project_status = Some((t!("app.save.saved").to_string(), now));
                runtime_log::log_info("[save_to_project] merge complete");
                SavePoll::Succeeded
            }
            Ok(Err(err)) => {
                self.save_to_project_rx = None;
                let msg = tf!("app.save.error", err = err);
                runtime_log::log_error(format!("[save_to_project] {msg}"));
                self.save_to_project_status = Some((msg, now));
                SavePoll::Failed
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => SavePoll::NotFinished,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.save_to_project_rx = None;
                let msg = t!("app.save.thread_crashed").to_string();
                self.save_to_project_status = Some((msg.clone(), now));
                runtime_log::log_error(format!("[save_to_project] {msg}"));
                SavePoll::Failed
            }
        }
    }
}

/// Whether the session is still unsaved right after a successful merge: the staging directory
/// was re-created during it, or the bubbles model moved past the revision the save committed.
fn post_save_unsaved(done: MergeDone, bubbles_advanced: bool) -> bool {
    done.staging_exists || bubbles_advanced
}

/// The user-visible reason for `blocker` (save status line, disabled save buttons' hover text);
/// `None` for a save already under way, whose own status line is shown instead.
#[must_use]
pub(super) fn save_blocker_text(blocker: SaveBlocker) -> Option<&'static str> {
    match blocker {
        SaveBlocker::PageOperation => Some(t!("app.save.blocked_by_page_operation")),
        SaveBlocker::CleanOperation => Some(t!("app.save.blocked_by_clean_operation")),
        SaveBlocker::ExitCleanup => Some(t!("app.save.blocked_by_exit_cleanup")),
        SaveBlocker::Export => Some(t!("app.save.blocked_by_export")),
        SaveBlocker::SaveInFlight => None,
    }
}

/// Decision for a project-save trigger given the current preload/save state — the pure core of
/// [`ProjectSession::request_save_to_project`], extracted so the gate transition is unit-testable without
/// constructing a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveGateDecision {
    /// A save is already running, or one is already pending on the preload — ignore the trigger.
    Ignore,
    /// Every page is resident — run the save immediately (fast path, unchanged behavior).
    SaveNow,
    /// Not every page is resident — start/continue the whole-project layer preload and keep the
    /// save pending until it drains.
    Defer,
}

/// Pure gate for a project-save TRIGGER. Save is deferred behind a full whole-project layer preload
/// so every page's lazy legacy→v3 text migration bakes into the committed `layers.json`. `Ignore`
/// keeps the trigger idempotent (a running or already-pending save is not restarted).
#[must_use]
fn save_trigger_decision(
    save_in_flight: bool,
    pending_after_preload: bool,
    all_pages_loaded: bool,
) -> SaveGateDecision {
    if save_in_flight || pending_after_preload {
        SaveGateDecision::Ignore
    } else if all_pages_loaded {
        SaveGateDecision::SaveNow
    } else {
        SaveGateDecision::Defer
    }
}

/// Pure gate for the per-frame deferred-save driver ([`ProjectSession::drive_pending_save_preload`]): true
/// iff the deferred save should now be dispatched — the whole-project preload PASS has fully drained
/// (`!preload_active`).
///
/// It gates on pass COMPLETION, NOT on full residency (`all_pages_loaded`), and this is required for
/// correctness: a page whose decode genuinely fails (corrupt on-disk layer data, or a worker panic
/// that drops the sender) is dropped from the pass and NEVER becomes resident, so a residency gate
/// would leave the save deferred forever (permanent spinner, disabled "Save project", no retry, exit
/// via save unable to close). Dispatching after the pass drains is safe: the unsaved→committed merge
/// preserves a non-resident page's committed text verbatim (it is simply absent from
/// `owned_text_pages`), so nothing is lost. The pending flag is set only on the deferred path (when
/// `!all_pages_loaded`), and `begin_preload_all_pages` makes `preload_active` true that same frame
/// when pages need loading, so the gate still waits for the worker to drain before dispatching.
#[must_use]
fn deferred_save_ready(preload_active: bool) -> bool {
    !preload_active
}

#[cfg(test)]
mod tests {
    use super::{SaveGateDecision, deferred_save_ready, save_trigger_decision};

    #[test]
    fn save_trigger_defers_until_all_pages_loaded_then_saves() {
        // Not all pages resident and nothing in flight → defer behind the preload.
        assert_eq!(
            save_trigger_decision(false, false, false),
            SaveGateDecision::Defer
        );
        // Every page resident → run the save immediately (fast path).
        assert_eq!(
            save_trigger_decision(false, false, true),
            SaveGateDecision::SaveNow
        );
        // A save already running is never restarted, regardless of residency.
        assert_eq!(
            save_trigger_decision(true, false, true),
            SaveGateDecision::Ignore
        );
        assert_eq!(
            save_trigger_decision(true, false, false),
            SaveGateDecision::Ignore
        );
        // A save already pending on the preload is idempotent (a second click is ignored).
        assert_eq!(
            save_trigger_decision(false, true, false),
            SaveGateDecision::Ignore
        );
        assert_eq!(
            save_trigger_decision(false, true, true),
            SaveGateDecision::Ignore
        );
    }

    #[test]
    fn deferred_save_dispatches_when_preload_pass_drains() {
        // Dispatches once the preload PASS has drained (worker inactive), regardless of full
        // residency — the give-up path (a page that failed to decode never becomes resident) must
        // still reach dispatch, not hang.
        assert!(deferred_save_ready(false));
        // While the worker is still applying pages, keep waiting one more frame.
        assert!(!deferred_save_ready(true));
    }

    #[test]
    fn deferred_save_does_not_hang_on_decode_error_giving_up() {
        // FINDING 1 regression guard: a page that genuinely fails to decode is dropped from the pass
        // WITHOUT becoming resident, so `all_pages_loaded` would stay false forever. The gate keys on
        // pass completion (`!preload_active`), so once the worker finishes draining — even with a
        // failed page still non-resident — the deferred save dispatches instead of hanging.
        let all_pages_loaded_after_giveup = false; // the failed page never became resident
        let preload_active_after_drain = false; // the worker finished the pass
        // Old residency-based gate would have returned false here (hang); the new gate dispatches.
        assert!(deferred_save_ready(preload_active_after_drain));
        assert!(
            !all_pages_loaded_after_giveup,
            "precondition: not every page is resident after giving up on a decode failure"
        );
    }
}
