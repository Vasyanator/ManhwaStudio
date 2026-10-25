/*
File: src/studio/session/project/page_op.rs

Purpose:
The project session's structural page operation (move, insert, blank, delete, stitch, split, crop
from the Page Manager): the gated start sequence, the worker that quiesces the staging writers and
runs the journalled transaction, the poll that requests the rebuild from disk, and the progress
veil / failure dialog.

Key structures:
- `PageOpTextQuiesce` + `page_op_text_quiesce`: the pure typing-text precondition.

Key functions:
- `ProjectSession::start_page_op`: ONE method, fixed order: busy gate -> clean-op gate -> bubble
  flush of all three canvases (debounced text included) -> PS flush -> typing quiesce (REFUSES before anything moves) ->
  worker thread started parked (a refused thread REFUSES the op here, before any writer stops) ->
  `StagingWriters::quiesce_for_page_op` -> worker released (join клин autosave, клин snapshots,
  bubbles pause, layer barrier, staging writes, `execute_page_op`).
- `ProjectSession::poll_page_op` (driven by the worker's `WakeGuiOnDrop`, no timer poll),
  `ProjectSession::draw_page_op_overlays`.

Notes:
Unreachable in an image-set session (the Page Manager tab is hidden there; `SessionMode::start_page_op`
refuses it). Order and rationale: "Save / page-op / discard / exit choreography" in
`src/studio/MODULE_README.md`.
*/

use super::ProjectSession;
use crate::studio::session::SessionParts;
use crate::models::bubbles_model::write_bubbles_snapshot_to;
use crate::models::clean_overlays_model::save_overlay_snapshots_guarded;
use crate::page_ops::{PageOpKind, execute_page_op};
use crate::runtime_log;
use crate::tabs::typing::TypingTextFlushError;
use eframe::egui;
use ms_thread as thread;
use std::sync::Arc;
use std::sync::mpsc::TryRecvError;

impl ProjectSession {
    /// Starts a structural page operation after quiescing every writer that owns page-indexed data.
    ///
    /// The typing-text quiesce is a hard PRECONDITION, not best-effort: this transaction remaps the
    /// page-keyed trees and is followed by a project reload that replaces the in-memory document, so a
    /// deferred text edit that is not in staging when it starts is lost or re-keyed to an obsolete
    /// page index. If it cannot be established (`page_op_text_quiesce`), the request is REFUSED with a
    /// user-visible message and nothing is started, shut down, or moved on disk.
    pub(in crate::studio) fn start_page_op(&mut self, op: PageOpKind, parts: &mut SessionParts<'_>) {
        let activity = self.activity(parts);
        if activity.blocks_page_op() {
            runtime_log::log_info("[page_op] request ignored while another persistence operation is active");
            return;
        }
        // A clean attach/detach worker holds page indices and an Arc of the CURRENT model;
        // reloading the project under it would misapply or lose the clean operation.
        if activity.clean_op_in_flight() {
            runtime_log::log_info("[page_op] request ignored while a clean-layer operation is in flight");
            return;
        }
        parts.flush_canvas_bubble_edits();
        parts.ps_editor.flush_layers(parts.project);
        // Quiesce the typing text writer. The owned-page set is irrelevant here (no merge follows —
        // the page-op transaction remaps both trees), but the quiesce SUCCEEDING is not: this
        // transaction renames the page-keyed trees and the project reload behind it replaces the
        // in-memory document, so a text edit that is not in staging when it starts is lost or re-keyed
        // to an obsolete page index. Deferral makes that the normal state of a fresh edit, and the
        // tab's own dirty state does not survive the reload. So gate the whole operation on it — and
        // gate it HERE, before any worker is shut down and before anything on disk moves, so an abort
        // leaves the session exactly as it was.
        //
        // `had_pending_edits` is read BEFORE the flush: a successful flush clears the dirty state.
        let had_pending_text_edits = parts.typing.has_pending_text_edits();
        let flush_result = parts.typing.flush_text_layers();
        let (flush_error, failed_pages) = match &flush_result {
            Ok(outcome) => (None, outcome.failed_pages),
            Err(err) => (Some(*err), 0),
        };
        match page_op_text_quiesce(flush_error, failed_pages, had_pending_text_edits) {
            PageOpTextQuiesce::Quiesced => {}
            PageOpTextQuiesce::EditsUnwritten => {
                runtime_log::log_error(format!(
                    "[page_op] request aborted: the typing tab still owes text to the staging dir \
                     (flush_error={flush_error:?}, failed_pages={failed_pages}, \
                     had_pending_edits={had_pending_text_edits}). The transaction would have remapped \
                     the page-keyed trees without those edits and the reload behind it would drop \
                     them, so nothing was started and the chapter is untouched."
                ));
                // Reported on the app-wide persistence status line rather than through `page_op_error`:
                // that dialog says files on disk may have changed and offers only "Reload project",
                // which would discard the very in-memory edits this abort exists to protect. Nothing
                // has changed and nothing must be reloaded.
                self.save_to_project_status =
                    Some((t!("page_manager.op_blocked_text_unwritten").to_string(), 0.0));
                return;
            }
            PageOpTextQuiesce::DocUnreadable => {
                runtime_log::log_error(
                    "[page_op] request aborted: the shared layer document lock is poisoned, so no \
                     tab's layer state could be staged or even inspected. A structural transaction \
                     against an unreadable document cannot be made safe; nothing was started.",
                );
                self.save_to_project_status =
                    Some((t!("page_manager.op_blocked_doc_unreadable").to_string(), 0.0));
                return;
            }
        }

        let saver_handle = parts.writers.layer_doc().lock().ok().and_then(|guard| guard.saver_handle());
        let overlays_model = Arc::clone(parts.writers.clean_overlays_model());
        let bubbles_model = Arc::clone(parts.writers.bubbles_model());
        let paths = parts.project.paths.clone();
        let pages = parts.project.pages.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        // The worker is started PARKED, before any writer is quiesced, and released below with the
        // клин autosave thread: a thread the OS refuses then refuses the operation while the session
        // is still exactly as it was (no writer stopped, nothing on disk moved).
        let (start_tx, start_rx) = std::sync::mpsc::channel::<Option<thread::JoinHandle<()>>>();
        let wake_gui = self.job_waker("page-op");
        let spawned = thread::Builder::new().name("page-op".to_owned()).spawn(move || {
            // Declared first so it drops last, on every return path and on a panic: the GUI polls
            // `rx` only on that wake, never on a timer. `tx` is rebound right after it so the
            // sender is gone (a lost worker reads as `Disconnected`) before the wake fires —
            // a moved-in capture would otherwise drop with the closure, after every local.
            let _wake_gui = wake_gui;
            let tx = tx;
            // A closed start channel means `start_page_op` dropped the worker without releasing it;
            // nothing was quiesced for it, so it must not touch the staging trees. Dropping `tx`
            // reports that as a lost worker.
            let Ok(overlay_autosave_thread) = start_rx.recv() else {
                runtime_log::log_error("[page_op] worker released without a start signal; nothing was changed");
                return;
            };
            if let Some(handle) = overlay_autosave_thread
                && handle.join().is_err()
            {
                runtime_log::log_error("[page_op] overlay autosave worker panicked during shutdown");
            }
            // Snapshot only after autosave has stopped: otherwise it can take or restore the same
            // dirty indexes concurrently with this transaction's final staging flush.
            let dirty_overlay_snapshots = match overlays_model.lock() {
                Ok(mut overlays) => overlays.take_dirty_save_snapshots(),
                Err(err) => {
                    runtime_log::log_error(format!("[page_op] CleanOverlaysModel lock poisoned before the staging flush; aborting; error={err}"));
                    if tx.send(Err(t!("page_manager.op_lock_error").to_string())).is_err() {
                        runtime_log::log_error("[page_op] result receiver dropped after overlay lock failure");
                    }
                    return;
                }
            };
            let bubbles_snapshot = match bubbles_model.lock() {
                Ok(mut bubbles) => bubbles.pause_saver_for_page_op(),
                Err(err) => {
                    runtime_log::log_error(format!("[page_op] BubblesModel lock poisoned before the staging flush; aborting; error={err}"));
                    if let Ok(mut overlays) = overlays_model.lock() {
                        overlays.restore_dirty_save_snapshots(&dirty_overlay_snapshots);
                    }
                    if tx.send(Err(t!("page_manager.op_lock_error").to_string())).is_err() {
                        runtime_log::log_error("[page_op] result receiver dropped after bubbles lock failure");
                    }
                    return;
                }
            };
            // The saver FIFO barrier follows the UI-side flushes, making the staging manifest
            // authoritative before the transaction reads and remaps it.
            if let Some(handle) = saver_handle {
                let failed_pages = handle.barrier_blocking();
                if !failed_pages.is_empty() {
                    runtime_log::log_warn(format!(
                        "[page_op] layer saver reported failed staging pages; pages={failed_pages:?}"
                    ));
                }
            }
            if let Err(err) = save_overlay_snapshots_guarded(
                &paths.unsaved_clean_layers_dir,
                &dirty_overlay_snapshots,
                &overlays_model,
            ) {
                if let Ok(mut overlays) = overlays_model.lock() {
                    overlays.restore_dirty_save_snapshots(&dirty_overlay_snapshots);
                }
                if tx.send(Err(format!("failed to flush dirty overlays: {err}"))).is_err() {
                    runtime_log::log_error("[page_op] result receiver dropped after overlay flush failure");
                }
                return;
            }
            if let Err(err) = write_bubbles_snapshot_to(&paths.unsaved_bubbles_file, bubbles_snapshot.as_slice()) {
                if let Ok(mut overlays) = overlays_model.lock() {
                    overlays.restore_dirty_save_snapshots(&dirty_overlay_snapshots);
                }
                if tx.send(Err(format!("failed to flush translation bubbles: {err}"))).is_err() {
                    runtime_log::log_error("[page_op] result receiver dropped after bubbles flush failure");
                }
                return;
            }
            let result = execute_page_op(&paths, &pages, &op).map_err(|err| err.to_string());
            if tx.send(result).is_err() {
                runtime_log::log_error("[page_op] result receiver dropped before transaction completed");
            }
        });
        // The handle is dropped: the worker is detached and answers only through `tx`.
        if let Err(err) = spawned {
            runtime_log::log_error(format!(
                "[page_op] request aborted: the page operation worker thread could not be started: {err}. Possible cause: the OS thread limit or low memory. No writer was stopped and nothing on disk moved"
            ));
            // The status line, like the other refusals above: nothing changed, so the failure dialog
            // (which offers only a reload) does not apply.
            self.save_to_project_status = Some((t!("page_manager.op_worker_start_error").to_string(), 0.0));
            return;
        }
        // Force the gate and stop-now the клин autosave; its thread is joined in the worker, never
        // here (`StagingWriters::quiesce_for_page_op` documents why stop-now, not flush).
        let overlay_autosave_thread = parts.writers.quiesce_for_page_op();
        if let Err(unsent) = start_tx.send(overlay_autosave_thread) {
            // Unreachable while the worker is parked on `recv` (it returns only after receiving); if
            // it happens anyway, `rx` is disconnected and `poll_page_op` shows the lost-worker
            // dialog. The клин thread was told to stop-now and writes nothing more, so dropping its
            // handle detaches a thread that is already exiting.
            runtime_log::log_error(format!(
                "[page_op] worker gone before its start signal; the operation is reported as failed (autosave handle present={})",
                unsent.0.is_some()
            ));
        }
        self.page_op_rx = Some(rx);
    }

    /// Polls the structural-operation worker and requests a safe bootstrap reload on success.
    ///
    /// No timer poll: the worker wakes the GUI when it ends ([`super::WakeGuiOnDrop`]), so an
    /// `Empty` channel requests no repaint. The operation reports no intermediate progress; the
    /// veil's spinner animates itself.
    pub(super) fn poll_page_op(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.page_op_rx.as_ref() else { return; };
        match rx.try_recv() {
            Ok(Ok(outcome)) => {
                runtime_log::log_info(format!(
                    "[page_op] completed; old_pages={}; pages={}",
                    outcome.old_to_new.len(), outcome.new_page_count
                ));
                self.page_op_rx = None;
                self.pending_project_reload = true;
                ctx.request_repaint();
            }
            Ok(Err(err)) => {
                runtime_log::log_error(format!("[page_op] failed: {err}"));
                self.page_op_rx = None;
                self.page_op_error = Some(err);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.page_op_rx = None;
                self.page_op_error = Some(t!("page_manager.op_thread_crashed").to_string());
            }
        }
    }

    /// Draws the blocking progress veil and the failure dialog for a structural page operation.
    pub(in crate::studio) fn draw_page_op_overlays(&mut self, ctx: &egui::Context, parts: &SessionParts<'_>) {
        if self.activity(parts).structural_op_active() {
            let screen = ctx.viewport_rect();
            egui::Area::new(egui::Id::new("page_op_progress_blocker"))
                .order(egui::Order::Foreground).fixed_pos(screen.min).constrain(false)
                .movable(false).interactable(true).show(ctx, |ui| {
                    ui.set_clip_rect(screen);
                    ui.painter().rect_filled(screen, 0.0, ms_theme::canvas::MODAL_SCRIM);
                    ui.allocate_rect(screen, egui::Sense::click_and_drag());
                    ui.vertical_centered(|ui| { ui.add_space(screen.height() * 0.45); ui.spinner(); ui.label(t!("page_manager.op_in_progress")); });
                });
        }
        if let Some(error) = self.page_op_error.as_ref() {
            let mut reload = false;
            egui::Window::new(t!("page_manager.op_failed_title")).id(egui::Id::new("page_op_failed_dialog"))
                .order(egui::Order::Foreground)
                .collapsible(false).resizable(false).show(ctx, |ui| {
                    ui.label(t!("page_manager.op_failed_message"));
                    ui.colored_label(ms_theme::status::ERROR, error);
                    if ui.button(t!("page_manager.op_reload_button")).clicked() { reload = true; }
                });
            if reload { self.page_op_error = None; self.pending_project_reload = true; }
        }
    }
}

/// Verdict of the typing-text quiesce that must precede a structural page operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageOpTextQuiesce {
    /// The typing tab owes nothing to the disk: the transaction may start.
    Quiesced,
    /// Text edits exist that did not reach the staging dir. Starting the transaction would strand them.
    EditsUnwritten,
    /// The shared layer document could not be read at all, so nothing about it can be trusted.
    DocUnreadable,
}

/// Pure gate for the typing-text quiesce in [`ProjectSession::start_page_op`]: may the structural page
/// transaction start?
///
/// A page operation remaps the page-keyed trees on disk and is followed by a project RELOAD that
/// replaces the in-memory document. Anything the typing tab still owes the disk when it starts is
/// therefore lost, or resurfaces keyed to an obsolete page index — and since text edits are deferred,
/// that can be everything the user typed since the last flush point. The quiesce is a PRECONDITION,
/// not a courtesy.
///
/// Arguments are the flush attempt's outcome: `flush_error` is its failure (if any), `failed_pages` is
/// `TypingTextFlushOutcome::failed_pages`, and `had_pending_edits` is `has_pending_text_edits()` read
/// BEFORE the flush (a successful flush clears the dirty state, so reading it after would always say
/// "nothing pending").
///
/// The `NoLayersDir`/`NoLayerDoc` arm is why this is not a blanket "abort on `Err`": those mean the
/// typing text store was never wired this session — the user never opened the Text tab — so the tab
/// has written nothing and owes nothing, and page operations must keep working for them. Only an
/// unwritten edit makes an unwired store an abort, and the tab cannot hold one without having been
/// drawn (which wires the store). `DocLockPoisoned` is not in that arm: it means the shared document
/// itself is unreadable, so the PS editor's text is equally unaccounted for and `had_pending_edits`
/// cannot speak for it.
#[must_use]
fn page_op_text_quiesce(
    flush_error: Option<TypingTextFlushError>,
    failed_pages: usize,
    had_pending_edits: bool,
) -> PageOpTextQuiesce {
    match flush_error {
        Some(TypingTextFlushError::DocLockPoisoned) => PageOpTextQuiesce::DocUnreadable,
        Some(TypingTextFlushError::NoLayersDir | TypingTextFlushError::NoLayerDoc) => {
            if had_pending_edits {
                PageOpTextQuiesce::EditsUnwritten
            } else {
                PageOpTextQuiesce::Quiesced
            }
        }
        // The flush ran, so it is authoritative for every resident page it enqueued. A page it could
        // NOT enqueue is a real write failure whose text is not in staging for the remap to carry.
        None => {
            if failed_pages == 0 {
                PageOpTextQuiesce::Quiesced
            } else {
                PageOpTextQuiesce::EditsUnwritten
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PageOpTextQuiesce, TypingTextFlushError, page_op_text_quiesce};

    #[test]
    fn page_op_quiesce_proceeds_when_the_flush_enqueued_every_resident_page() {
        // The normal path: the flush ran and nothing failed, so the transaction may remap the trees.
        // Whether an edit was pending before the flush is irrelevant — it has just been staged.
        assert_eq!(
            page_op_text_quiesce(None, 0, true),
            PageOpTextQuiesce::Quiesced
        );
        assert_eq!(
            page_op_text_quiesce(None, 0, false),
            PageOpTextQuiesce::Quiesced
        );
    }

    #[test]
    fn page_op_quiesce_aborts_when_a_resident_page_failed_to_enqueue() {
        // The flush ran but a page's text never reached the saver: it is not in staging for the remap
        // to carry, so the transaction must not start regardless of the tab's dirty flags.
        assert_eq!(
            page_op_text_quiesce(None, 1, false),
            PageOpTextQuiesce::EditsUnwritten
        );
        assert_eq!(
            page_op_text_quiesce(None, 3, true),
            PageOpTextQuiesce::EditsUnwritten
        );
    }

    #[test]
    fn page_op_quiesce_aborts_when_a_pending_edit_could_not_be_flushed() {
        // The flush could not run at all AND the tab was holding an unwritten edit: the transaction
        // would remap the page-keyed trees without it and the reload behind it would drop it.
        for err in [
            TypingTextFlushError::NoLayersDir,
            TypingTextFlushError::NoLayerDoc,
        ] {
            assert_eq!(
                page_op_text_quiesce(Some(err), 0, true),
                PageOpTextQuiesce::EditsUnwritten,
                "{err:?} with a pending edit must abort"
            );
        }
    }

    #[test]
    fn page_op_quiesce_proceeds_when_the_text_store_was_never_wired() {
        // Regression guard: the typing text store is wired lazily, by the Text tab's first draw. A
        // user who goes straight to the page manager without ever opening Text gets `NoLayersDir` on
        // every page operation. The tab has written nothing and owes nothing there, so a blanket
        // "abort on Err" would break page operations outright for them.
        for err in [
            TypingTextFlushError::NoLayersDir,
            TypingTextFlushError::NoLayerDoc,
        ] {
            assert_eq!(
                page_op_text_quiesce(Some(err), 0, false),
                PageOpTextQuiesce::Quiesced,
                "{err:?} with nothing pending must not block the page operation"
            );
        }
    }

    #[test]
    fn page_op_quiesce_aborts_on_a_poisoned_doc_lock_even_with_nothing_pending() {
        // A poisoned lock is not "the store was never wired": the shared document is unreadable, so
        // the PS editor's text is unaccounted for too and the typing tab's dirty flags cannot speak
        // for it. Never start a structural transaction against it.
        assert_eq!(
            page_op_text_quiesce(Some(TypingTextFlushError::DocLockPoisoned), 0, false),
            PageOpTextQuiesce::DocUnreadable
        );
        assert_eq!(
            page_op_text_quiesce(Some(TypingTextFlushError::DocLockPoisoned), 0, true),
            PageOpTextQuiesce::DocUnreadable
        );
    }
}
