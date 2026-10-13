/*
File: src/app/shutdown.rs

Purpose:
The studio root's blocking teardown, kept out of the GUI-facing `app.rs`. `MangaApp` is moved
as a whole (`Box<MangaApp>`) onto a worker thread so this drain never blocks the GUI thread; the
permanent `Send` guard below keeps that move possible.

Key structures:
- `ShutdownReport`: what the teardown could not make durable (failed layer pages, bubbles saver
  error), for the caller to show; every item is also logged here.

Key functions:
- `MangaApp::shutdown_blocking()`: flush and stop every staging writer and debounced store in the
  order of the "Save / page-op / discard / exit choreography" (`src/MODULE_README.md`).

Notes:
A child module of `app` so it reaches `MangaApp`'s private fields and helpers without widening
their visibility. Callers today: `MangaApp`'s `eframe::App::on_exit` and the structural-operation
reload in `studio_bootstrap.rs`, both still on the GUI thread until the shell's Closing screen
runs this on a worker.
*/

use super::{MangaApp, persist_canvas_hint_collapsed_state, persist_typing_centering_assist_state};
use crate::runtime_log;
use crate::tabs::typing::TypingSaveFlushReason;
use std::sync::PoisonError;
use std::sync::atomic::Ordering as AtomicOrdering;

/// What [`MangaApp::shutdown_blocking`] could not make durable. Empty (`Default`) on a clean
/// teardown. Each item is logged by the teardown as well; the report exists so a UI can show it.
#[derive(Debug, Default)]
pub(crate) struct ShutdownReport {
    /// Pages whose layer write the final layer-saver barrier reported as FAILED, ascending.
    pub failed_layer_pages: Vec<usize>,
    /// The bubbles saver's flush-and-join error, when it could not be drained.
    pub bubbles_error: Option<String>,
}

/// Compile-time proof that `MangaApp` may move to another thread (its teardown runs on a worker).
#[cfg(not(target_arch = "wasm32"))]
fn assert_send<T: Send>() {}

/// Permanent guard: adding a non-`Send` field (an `Rc`, a non-`Send` trait object) to anything
/// `MangaApp` owns breaks the build here instead of the off-GUI-thread teardown. Native only: the
/// worker-thread teardown is a native-shell path; the web build drops the app in place.
#[cfg(not(target_arch = "wasm32"))]
const _: fn() = assert_send::<MangaApp>;

impl MangaApp {
    /// Final shutdown drain for the background layer saver and the other writers: runs on real
    /// process exit, return-to-launcher, and the structural-operation reload. Flushes the typing
    /// tab's deferred text edits, then blocks until every queued layer write is flushed and the
    /// worker thread is joined, so no pending raster/text/effects write is lost on exit. Must NOT hold
    /// the doc lock across the barrier: it clones the handle, drops the lock, then barriers; finally
    /// `shutdown_saver` drains the queue + joins the worker. Relying on Arc-drop ordering alone is
    /// unsafe (a lingering Arc clone in a worker could skip the join).
    ///
    /// Blocking: meant for a worker thread (or the process-shutdown hook). Idempotent, because every
    /// step it calls is (a saver already shut down is skipped). Returns what could not be made
    /// durable; each item is logged here too.
    pub(crate) fn shutdown_blocking(&mut self) -> ShutdownReport {
        let mut report = ShutdownReport::default();
        // A single-image session always closes the DISCARD way (plan D13): its staging is a scratch
        // `run_main` deletes once the window is gone, so nothing below may write into it. No-op when
        // a dialog already took that path.
        if self.single_image.is_some() {
            self.quiesce_writers_discarding();
        }
        self.stop_overlay_autosave();
        // Release decode workers parked on the look-ahead window: after this frame nothing
        // drains the loader channels, so the promotion frontier would never advance again and
        // parked workers (and the pool joiner) would leak. Workers blocked on `send` exit on
        // their own once the receivers drop with `self`.
        self.loader_shutdown.store(true, AtomicOrdering::Release);
        // 0) Write the typing tab's DEFERRED text edits — necessarily BEFORE the barrier below.
        // Typing does not write an edit as it happens (that would write every drag frame); it writes at
        // a focus-loss point, and closing the app is a focus loss no in-tab flush point observes. The
        // barrier is what guarantees bytes on disk, but it can only cover writes already ENQUEUED, so a
        // still-deferred edit must be enqueued first or it is lost on close. This flush enqueues INLINE
        // on this thread (not via the detached placement-save worker, which has no quiescence handle and
        // would race the barrier), so step 1 covers it. No-op when nothing is dirty.
        //
        // SKIPPED on the discard path: `start_exit_cleanup` deletes the staging dir on purpose and has
        // already dropped the pending edits, so a flush here would write back exactly what the user
        // asked to throw away — and, with the saver already shut down, `enqueue_page_text_save`'s
        // synchronous fallback would re-create the deleted `_unsaved/layers/` dir to do it.
        if self.discarding_unsaved_changes {
            runtime_log::log_info(
                "[app] on_exit: discard path — pending typing text edits dropped, not flushed",
            );
        } else {
            self.typing_tab
                .flush_text_layers_if_dirty(TypingSaveFlushReason::Exit);
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

        // 4) Persist per-tab canvas bottom-hint collapsed state (exit-only, single read-modify-write).
        // `on_exit` fires on both program close and return-to-launcher, so this is the one save point.
        persist_canvas_hint_collapsed_state(
            self.canvas.bottom_hint_collapsed(),
            self.typing_tab.hint_collapsed(),
        );

        // 5) Persist the two typing centering-assist checkboxes (exit-only, single locked RMW). The
        // bound-center KIND is deliberately session-only, so it is not written here.
        persist_typing_centering_assist_state(
            self.typing_tab.centering_assist_enabled(),
            self.typing_tab.centering_show_center(),
        );

        // 6) Flush a still-pending per-font-settings write. A parameter edit stores the font's
        // default profile through a multi-second DEBOUNCE (one document write per burst instead of
        // one per keystroke), and the debounced writer is a detached thread that dies with the
        // process — so changing a parameter and closing the app a second later used to lose the
        // edit outright. Synchronous by necessity: no GUI frame follows this call and no thread
        // outlives it. No-op when nothing is pending, which is the normal case.
        if crate::tabs::typing::font_admin::flush_pending_saves() {
            runtime_log::log_info("[app] on_exit: flushed a pending fonts_data.json write");
        }

        // 6a) Flush a still-pending DEFAULT local-preset set. Same shape and same reason as
        // the step above, for the other debounced typing store: a local preset renamed or
        // re-parameterised inside its debounce window would otherwise die with the detached
        // writer thread. No-op when nothing is owed. Why this write is synchronous here — and
        // why a deadline would trade a visible hang for a silent loss — is argued at
        // `local_presets::flush_pending_local_presets_save` and in
        // `crates/ms-tab-typing/src/panel/MODULE_README.md`.
        if self.typing_tab.flush_pending_local_presets_save() {
            runtime_log::log_info(
                "[app] on_exit: flushed a pending fonts/presets.json local-preset write",
            );
        }

        // 7) Flush the dockable-panel layouts. A last poll first, because the frame that raised
        // the dirty flag may be the one the close arrived in and no further frame will poll it;
        // then the writer's own debounce window is drained synchronously and its thread joined.
        // Both steps are no-ops when nothing changed.
        if let Some(layouts) = self.panel_dock.take_dirty_layouts() {
            self.panel_layout_writer.store(layouts);
        }
        self.panel_layout_writer.flush_and_join();
        report
    }
}
