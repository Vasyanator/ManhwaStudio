/*
File: src/studio/staging_writers.rs

Purpose:
The three `{chapter}_unsaved` staging writers of one studio session, as ONE owned struct: the
autosave gate they share, the клин overlay autosave thread and its quiescence control, Arc clones
of the three models whose savers it drives (layer document, bubbles, clean overlays), the staging
клин directory, and the DISCARD latch. Every writer-side sequence of the "Save / page-op / discard
/ exit choreography" (`src/studio/MODULE_README.md`) lives in ONE method here, in its fixed order.

Key structures:
- `StagingWriters`

Key functions:
- `StagingWriters::spawn`: starts the клин autosave thread on the session's gate.
- `StagingWriters::quiesce_discarding`: the writer half of a DISCARD (latch first).
- `StagingWriters::restart_after_failed_discard`: un-latch and restart all three writers.
- `StagingWriters::quiesce_for_page_op`: force the gate, stop-now the клин autosave, hand its
  thread to the page-op worker.
- `StagingWriters::stop_overlay_autosave_blocking` / `drain_for_shutdown`: the writer steps of
  `StudioApp::shutdown_blocking` (the decode-pool release sits between them there).

Notes:
Owned struct with a narrow API: it never reaches `StudioApp`'s fields; a sequence that must call
into a tab in the middle takes that tab as a parameter, so no sequence is split across two
owners. The project session (`studio/session/project/`) reads the three models through the accessors
here (`layer_doc`, `bubbles_model`, `clean_overlays_model`: the same Arcs `StudioApp` holds) and takes
the gate, the клин control or the клин thread for its save, page-op and discard workers.
*/

use super::shutdown::ShutdownReport;
use crate::canvas::{OverlayAutosaveControl, spawn_overlay_autosave_thread};
use crate::models::autosave_gate::AutosaveGate;
use crate::models::bubbles_model::BubblesModel;
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::models::layer_model::layer_doc::LayerDoc;
use crate::runtime_log;
use crate::tabs::typing::{TypingSaveFlushReason, TypingTabState};
use ms_thread::JoinHandle;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

/// The staging writers of one session and the DISCARD latch that silences them.
///
/// Lives exactly as long as its `StudioApp`; a project reload builds a new app and therefore a new
/// gate. Not `Clone`: the клин thread handle has one owner.
pub(super) struct StagingWriters {
    /// The session's autosave write gate, shared by all three writers (layer saver, bubbles saver,
    /// клин autosave). Forced on save-to-project success and before a page operation.
    gate: Arc<AutosaveGate>,
    /// Background thread writing dirty клин overlays into staging when the gate is due. `None`
    /// once a page op, the discard delete job or the teardown took it.
    overlay_thread: Option<JoinHandle<()>>,
    /// Quiescence handle of `overlay_thread`: stop-now before a page operation or a discard,
    /// flush-and-stop at non-discard teardown, pause held by the save-to-project worker. Replaced by
    /// a fresh one when a failed discard restarts the thread.
    overlay_control: Arc<OverlayAutosaveControl>,
    layer_doc: Arc<Mutex<LayerDoc>>,
    bubbles_model: Arc<Mutex<BubblesModel>>,
    clean_overlays_model: Arc<Mutex<CleanOverlaysModel>>,
    /// Staging клин directory the autosave thread writes into (`ProjectPaths::unsaved_clean_layers_dir`).
    unsaved_clean_layers_dir: PathBuf,
    /// Latched by `quiesce_discarding`: the user chose to DISCARD (or a single-image session is
    /// closing). While set every remaining persistence path goes quiet — the teardown must not
    /// flush (it would re-create the deleted staging dir with the discarded edits) and the
    /// unsaved-changes cache must not re-latch (it would re-open the exit dialog).
    ///
    /// It survives a SUCCESSFUL cleanup (that branch dispatches the close in the same call, so the
    /// few remaining frames must stay quiet), but a FAILED cleanup hands control back to a
    /// still-running app, which must clear it via `restart_after_failed_discard` — left set, it
    /// would silence the unsaved-changes prompt and the teardown text flush for the rest of the
    /// session.
    discarding: bool,
}

impl StagingWriters {
    /// Starts the клин overlay autosave thread on `gate` (it writes the dirty pages when the gate is
    /// due) and keeps Arc clones of the three models. The caller has already attached `gate` to the
    /// models (bubbles saver, overlay model, layer saver) — that wiring precedes the tabs' clones.
    #[must_use]
    pub(super) fn spawn(
        gate: Arc<AutosaveGate>,
        layer_doc: &Arc<Mutex<LayerDoc>>,
        bubbles_model: &Arc<Mutex<BubblesModel>>,
        clean_overlays_model: &Arc<Mutex<CleanOverlaysModel>>,
        unsaved_clean_layers_dir: PathBuf,
    ) -> Self {
        let overlay_control = OverlayAutosaveControl::new();
        let overlay_thread = Some(spawn_overlay_autosave_thread(
            Arc::clone(clean_overlays_model),
            unsaved_clean_layers_dir.clone(),
            Arc::clone(&overlay_control),
            Some(Arc::clone(&gate)),
        ));
        Self {
            gate,
            overlay_thread,
            overlay_control,
            layer_doc: Arc::clone(layer_doc),
            bubbles_model: Arc::clone(bubbles_model),
            clean_overlays_model: Arc::clone(clean_overlays_model),
            unsaved_clean_layers_dir,
            discarding: false,
        }
    }

    /// The session's autosave gate (the single-image dirty baseline reads its action count).
    pub(super) fn gate(&self) -> &Arc<AutosaveGate> {
        &self.gate
    }

    /// A clone of the клин autosave control, for the save-to-project worker's `pause_blocking`.
    #[must_use]
    pub(super) fn overlay_control(&self) -> Arc<OverlayAutosaveControl> {
        Arc::clone(&self.overlay_control)
    }

    /// The shared layer document (the same Arc `StudioApp` and the tabs hold), for the project
    /// session's save / page-op workers and its unsaved-changes probe.
    pub(super) fn layer_doc(&self) -> &Arc<Mutex<LayerDoc>> {
        &self.layer_doc
    }

    /// The shared bubbles model (same Arc as `StudioApp`'s), for the project session's flows.
    pub(super) fn bubbles_model(&self) -> &Arc<Mutex<BubblesModel>> {
        &self.bubbles_model
    }

    /// The shared clean-overlay model (same Arc as `StudioApp`'s), for the project session's flows.
    pub(super) fn clean_overlays_model(&self) -> &Arc<Mutex<CleanOverlaysModel>> {
        &self.clean_overlays_model
    }

    /// Whether the DISCARD latch is set (see the field).
    pub(super) fn is_discarding(&self) -> bool {
        self.discarding
    }

    /// Takes the клин autosave thread handle for a job that joins it off the GUI thread (the
    /// project discard's delete job joins it BEFORE deleting staging). `None` when already taken.
    #[must_use]
    pub(super) fn take_overlay_thread(&mut self) -> Option<JoinHandle<()>> {
        self.overlay_thread.take()
    }

    /// The writer half of a structural page operation, called after the UI-side flushes and the
    /// typing quiesce succeeded: forces the gate, tells the клин autosave to stop now and returns its
    /// thread for the page-op worker to join (never joined here).
    #[must_use]
    pub(super) fn quiesce_for_page_op(&mut self) -> Option<JoinHandle<()>> {
        // Every change the autosave gate is still holding must reach `_unsaved` (NOT the committed
        // tree) before the transaction remaps it; the reload behind the op then builds a fresh app and
        // gate. The worker below is that force point for all three writers: the layer saver barrier
        // writes its held bucket, the bubbles snapshot is the model's CURRENT state (pause returns it,
        // the worker writes it), and the клин dirty set — the клин writer's held state — is taken and
        // written after the autosave thread joined. `force_flush` closes the gate's window so a writer
        // that wakes before being quiesced writes rather than keeps holding (and records the intent).
        self.gate.force_flush();
        // Stop-now (not flush): stop-now leaves the dirty клин set in the model, and the transaction
        // below takes and writes it into `_unsaved` itself after the join, so an autosave pass must
        // not race it. Joined in the worker, never here.
        self.overlay_control.request_stop_now();
        self.overlay_thread.take()
    }

    /// The writer half of a DISCARD: latches `discarding`, drops typing's deferred edits, shuts the
    /// layer saver down without writing (joined), pauses the bubbles saver and tells the клин
    /// autosave to stop without writing. The клин thread handle is left in place: the project
    /// discard hands it to its delete job (`take_overlay_thread`), every other caller lets the
    /// teardown join it. Idempotent (a second call finds the latch set and does nothing).
    ///
    /// A single-image session closes through this ALWAYS (plan D13): its staging is a scratch that
    /// the shell's teardown deletes after the app is dropped, so nothing may be flushed into it.
    pub(super) fn quiesce_discarding(&mut self, typing: &mut TypingTabState) {
        if self.discarding {
            return;
        }
        // This is the DISCARD path: latch it before anything else. The teardown must not flush after
        // this (its typing flush would re-create the staging dir being deleted, with the very edits
        // the user discarded), and the unsaved-changes cache must stop recomputing (a re-latch would
        // re-open the exit dialog and make discard-and-exit impossible).
        self.discarding = true;
        // Drop the typing tab's DEFERRED text edits. Nothing else clears them, and every remaining
        // writer path would otherwise resurrect them: the saver is shut down just below, so a pending
        // write re-dispatched afterwards takes `enqueue_page_text_save`'s SYNC fallback and re-creates
        // `_unsaved/layers/` after the delete job removed it. Keeping them also keeps
        // `has_pending_text_edits` true, which re-latches the unsaved-changes cache. Deliberately a
        // DROP, not a flush: `ARCHITECTURE.md` "Key invariants" — DISCARD never flushes.
        typing.discard_pending_text_edits();
        // Stop the layer saver BEFORE deleting the unsaved dir. The discard path removes the staging
        // folder the saver writes into; a still-running saver could recreate files mid/post deletion,
        // leaving the staging dir partially present. `shutdown_saver_discarding` stops the saver then JOINS the
        // worker, guaranteeing it is no longer writing before the delete job spawns — closing that
        // race. The shutdown DROPS queued and gate-held jobs instead of writing them: this is the
        // DISCARD path (the user chose not to save), so they target the dir being deleted; only the
        // join matters. The join waits at most for one in-progress write and runs only at teardown.
        if let Ok(mut guard) = self.layer_doc.lock() {
            guard.shutdown_saver_discarding();
        }
        // Quiesce bubble persistence before removing staging. The pause gate waits for any selected
        // write to finish, drops all later publications, and makes shutdown drain-and-join without
        // writing held snapshots. This is deliberately permanent on the successful discard path.
        // Poison is RECOVERED, not reported: a poisoned lock here must not leave the saver running.
        // Logging and continuing would spawn the delete job with an unpaused saver, which is exactly
        // the resurrection this pause exists to prevent. The `()`-payload model stays usable after a
        // panic, and every other holder recovers the same way.
        self.bubbles_model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pause_saver_for_discard();
        // Stop the клин autosave without writing (discard). The project discard joins it on the delete
        // job before the delete (left running, a pass could re-create the staging dir after its
        // removal); otherwise the teardown joins it.
        self.overlay_control.request_stop_now();
    }

    /// Un-latches the DISCARD path after its staging cleanup FAILED and restarts every writer the
    /// discard stopped, on the SAME gate: the layer saver (`shutdown_saver_discarding` left the doc
    /// without one, so every later layer write would take the synchronous fallback and bypass the
    /// gate), the bubbles saver and the клин autosave (fresh control + thread). The caller re-reports
    /// the session as unsaved (`StudioApp::abort_discard_after_failed_cleanup`).
    pub(super) fn restart_after_failed_discard(&mut self) {
        self.discarding = false;
        // Same call as `StudioApp::new`. Consumers never cache a `LayerSaverHandle` across frames
        // (typing/PS go through the doc funnels or take `saver_handle()` per use), so they all pick
        // the new saver up. Poison is RECOVERED: a doc without a saver would silently fall back to
        // synchronous, ungated writes for the rest of the session.
        self.layer_doc
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .enable_background_saver(Some(Arc::clone(&self.gate)));
        // Poison is RECOVERED, not reported: leaving the saver paused in a session that keeps running
        // would silently drop every later bubble edit for the rest of the process.
        self.bubbles_model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resume_saver_after_failed_discard();
        // The discard stopped the клин autosave; restart it with a fresh control so the running
        // session keeps autosaving (dirty pages stayed in the model: stop-now never takes them).
        // The old worker was joined by the delete job before it reported.
        self.overlay_control = OverlayAutosaveControl::new();
        self.overlay_thread = Some(spawn_overlay_autosave_thread(
            Arc::clone(&self.clean_overlays_model),
            self.unsaved_clean_layers_dir.clone(),
            Arc::clone(&self.overlay_control),
            Some(Arc::clone(&self.gate)),
        ));
    }

    /// Stops and joins the клин autosave during final teardown.
    ///
    /// Non-discard: requests one final pass so every dirty клин page reaches the staging dir before
    /// the process ends (the exit dialog already offered to save; staging is what a later session
    /// recovers). Discard: stops without writing — the project discard normally took the handle
    /// already and joins it in the delete job.
    ///
    /// Blocks for the final pass (one PNG encode per dirty page), or for a save-to-project merge
    /// that still holds a pause. That is acceptable only from `shutdown_blocking`, the same class as
    /// the layer-saver barrier there; structural operations take the handle to their worker.
    pub(super) fn stop_overlay_autosave_blocking(&mut self) {
        if self.discarding {
            self.overlay_control.request_stop_now();
        } else {
            self.overlay_control.request_flush_and_stop();
        }
        if let Some(handle) = self.overlay_thread.take()
            && handle.join().is_err()
        {
            runtime_log::log_error("[app] overlay autosave worker panicked during shutdown");
        }
    }

    /// Teardown steps 0-3 of `StudioApp::shutdown_blocking`: enqueue typing's deferred text edits
    /// INLINE (skipped when discarding), barrier the layer saver and record its failed pages, shut
    /// the layer saver down, then drain and join the bubbles saver. Failures are logged with the
    /// `[app] on_exit:` prefix AND written into `report`. Blocking; idempotent (a saver already shut
    /// down is skipped).
    pub(super) fn drain_for_shutdown(&mut self, typing: &mut TypingTabState, report: &mut ShutdownReport) {
        // 0) Write the typing tab's DEFERRED text edits — necessarily BEFORE the barrier below.
        // Typing does not write an edit as it happens (that would write every drag frame); it writes at
        // a focus-loss point, and closing the app is a focus loss no in-tab flush point observes. The
        // barrier is what guarantees bytes on disk, but it can only cover writes already ENQUEUED, so a
        // still-deferred edit must be enqueued first or it is lost on close. This flush enqueues INLINE
        // on this thread (not via the detached placement-save worker, which has no quiescence handle and
        // would race the barrier), so step 1 covers it. No-op when nothing is dirty.
        //
        // SKIPPED on the discard path: the discard quiesce deletes the staging dir on purpose and has
        // already dropped the pending edits, so a flush here would write back exactly what the user
        // asked to throw away — and, with the saver already shut down, `enqueue_page_text_save`'s
        // synchronous fallback would re-create the deleted `_unsaved/layers/` dir to do it.
        if self.discarding {
            runtime_log::log_info(
                "[app] on_exit: discard path — pending typing text edits dropped, not flushed",
            );
        } else {
            typing.flush_text_layers_if_dirty(TypingSaveFlushReason::Exit);
        }
        // 1) Barrier without holding the lock (clone the handle, drop the guard, then block).
        let handle = self
            .layer_doc
            .lock()
            .ok()
            .and_then(|guard| guard.saver_handle());
        // Pages the barrier reported as FAILED writes. Teardown has no project merge left to reconcile
        // them with, but the result must still be inspected and reported: since typing text edits are
        // deferred, this barrier is the ONLY durability signal a deferred edit ever gets, and exiting
        // with a cheerful "drained" line while pages silently failed to write hides real data loss.
        let mut barrier_failed_pages: Vec<usize> = Vec::new();
        if let Some(handle) = handle {
            barrier_failed_pages = handle.barrier_blocking().into_iter().collect();
            barrier_failed_pages.sort_unstable();
        }
        // 2) Drain remaining queue + join the worker thread (idempotent; safe if already shut down).
        if let Ok(mut guard) = self.layer_doc.lock() {
            guard.shutdown_saver();
        } else {
            runtime_log::log_error("[app] on_exit: shared doc lock poisoned, saver not joined");
        }
        if barrier_failed_pages.is_empty() {
            runtime_log::log_info("[app] on_exit: layer saver drained and shut down");
        } else {
            // Exit still proceeds — nothing can retry a write at teardown — but it must not claim success.
            runtime_log::log_error(format!(
                "[app] on_exit: layer saver shut down, but the teardown barrier reported {} page(s) \
                 whose layer write FAILED: {:?}. Their layer data (including any deferred typing text \
                 edit flushed above) is NOT in the staging dir. Cause is logged per page by the saver; \
                 exit continues because no retry is possible at teardown.",
                barrier_failed_pages.len(),
                barrier_failed_pages
            ));
        }
        report.failed_layer_pages = barrier_failed_pages;

        // 3) Drain and join the bubbles saver. Normally its barrier flushes queued snapshots. The
        // discard path paused its gate before deleting staging, so this only drains and joins there:
        // queued snapshots are intentionally dropped and cannot recreate `_unsaved/`.
        // Poison is RECOVERED here too: a poisoned lock must not cost the user their last edits by
        // skipping the flush entirely. A shutdown FAILURE is still reported — exit proceeds (no retry
        // is possible at teardown), but it must not claim success it did not observe.
        if let Err(err) = self
            .bubbles_model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .shutdown_saver()
        {
            runtime_log::log_error(format!(
                "[app] on_exit: bubbles saver could not be flushed and joined: {err}"
            ));
            report.bubbles_error = Some(err);
        } else {
            runtime_log::log_info("[app] on_exit: bubbles saver drained and shut down");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StagingWriters;
    use crate::studio::shutdown::ShutdownReport;
    use crate::models::autosave_gate::AutosaveGate;
    use crate::models::bubbles_model::BubblesModel;
    use crate::models::clean_overlays_model::CleanOverlaysModel;
    use crate::models::layer_model::layer_doc::LayerDoc;
    use crate::tabs::typing::TypingTabState;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// Unique scratch directory under the system temp dir, removed by the test.
    fn scratch_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        std::env::temp_dir().join(format!("ms_staging_writers_{label}_{}_{nanos}", std::process::id()))
    }

    /// A discard quiesces all three writers (latch, layer saver gone, клин thread stopped and
    /// joinable at once), and a failed discard restarts them all on the same gate (latch released,
    /// layer saver back, bubbles saver resumed and republishing, a fresh клин thread). Real models;
    /// every write lands under a unique temp dir that the test removes.
    #[test]
    fn discard_then_restart_after_failed_cleanup_brings_every_writer_back() {
        let root = scratch_dir("discard_restart");
        let unsaved = root.join("ch_unsaved");
        let gate = Arc::new(AutosaveGate::new());
        let layer_doc = Arc::new(Mutex::new(LayerDoc::new()));
        let Ok(mut doc) = layer_doc.lock() else {
            panic!("fresh layer doc lock poisoned");
        };
        doc.enable_background_saver(Some(Arc::clone(&gate)));
        drop(doc);
        let bubbles_model = Arc::new(Mutex::new(BubblesModel::new(
            Vec::new(),
            root.join("ch").join("bubbles"),
            unsaved.join("bubbles"),
            Default::default(),
            Some(Arc::clone(&gate)),
        )));
        let clean_overlays_model = Arc::new(Mutex::new(CleanOverlaysModel::new_from_pages(&[])));
        let mut typing = TypingTabState::default();
        let has_saver = |doc: &Arc<Mutex<LayerDoc>>| doc.lock().map(|doc| doc.saver_handle().is_some()).unwrap_or(false);
        let bubbles_unsaved = |model: &Arc<Mutex<BubblesModel>>| model.lock().map(|model| model.has_unsaved_changes()).unwrap_or(false);

        let mut writers = StagingWriters::spawn(Arc::clone(&gate), &layer_doc, &bubbles_model, &clean_overlays_model, unsaved.join("clean_layers"));
        assert!(!writers.is_discarding());
        assert!(Arc::ptr_eq(writers.gate(), &gate));

        writers.quiesce_discarding(&mut typing);
        assert!(writers.is_discarding());
        assert!(!has_saver(&layer_doc), "the discard shuts the layer saver down");
        let Some(stopped) = writers.take_overlay_thread() else {
            panic!("the discard leaves the клин thread for the delete job");
        };
        assert!(stopped.join().is_ok(), "stop-now lets the клин thread exit");
        // Idempotent: a second quiesce changes nothing.
        writers.quiesce_discarding(&mut typing);
        assert!(writers.is_discarding());

        writers.restart_after_failed_discard();
        assert!(!writers.is_discarding());
        assert!(has_saver(&layer_doc), "the layer saver is back");
        assert!(bubbles_unsaved(&bubbles_model), "the resumed bubbles saver republishes the model");
        assert!(Arc::ptr_eq(writers.gate(), &gate), "every writer restarts on the same gate");

        // A non-discard teardown then drains everything, with nothing failed.
        writers.stop_overlay_autosave_blocking();
        assert!(writers.take_overlay_thread().is_none(), "the restarted клин thread was joined");
        let mut report = ShutdownReport::default();
        writers.drain_for_shutdown(&mut typing, &mut report);
        assert!(report.failed_layer_pages.is_empty());
        assert!(report.bubbles_error.is_none());
        assert!(!has_saver(&layer_doc));

        drop(writers);
        if root.exists() {
            assert!(std::fs::remove_dir_all(&root).is_ok(), "scratch dir {} not removed", root.display());
        }
    }
}
