/*
File: src/studio/shutdown.rs

Purpose:
The studio root's blocking teardown, kept out of the GUI-facing `studio.rs`. `StudioApp` is moved
as a whole (`Box<StudioApp>`) onto a worker thread so this drain never blocks the GUI thread; the
permanent `Send` guard below keeps that move possible.

Key structures:
- `ShutdownReport`: what the teardown could not make durable (failed layer pages, bubbles saver
  error), for the caller to show; every item is also logged here.

Key functions:
- `StudioApp::shutdown_blocking()`: flush and stop every staging writer and debounced store in the
  order of the "Save / page-op / discard / exit choreography" (`src/studio/MODULE_README.md`). The writer
  steps themselves (клин autosave stop and join, typing flush, layer and bubbles saver drain) are
  `StagingWriters` methods (`studio/staging_writers.rs`); this file keeps their order, the all-canvas
  bubble flush and the PS-editor deferred layer flush before them (both skipped when discarding),
  the decode-pool release between them, and the exit-only stores.

Notes:
A child module of `studio` so it reaches `StudioApp`'s private fields and helpers without widening
their visibility. Its caller is the main window (`src/shell/closing.rs::teardown_blocking`): the
Closing screen's worker for every leave of the studio (exit, return to the launcher, reload), or
the window's `on_exit` when eframe ended the process with the studio open. The web entry never
runs it (it drops the app in place); the web port of the shell is deferred.
*/

use super::StudioApp;
use super::tabs::Tabs;
use crate::runtime_log;

/// What [`StudioApp::shutdown_blocking`] could not make durable. Empty (`Default`) on a clean
/// teardown. Each item is logged by the teardown as well; the report exists so a UI can show it.
#[derive(Debug, Default)]
pub(crate) struct ShutdownReport {
    /// Pages whose layer write the final layer-saver barrier reported as FAILED, ascending.
    pub failed_layer_pages: Vec<usize>,
    /// The bubbles saver's flush-and-join error, when it could not be drained.
    pub bubbles_error: Option<String>,
}

/// Compile-time proof that `StudioApp` may move to another thread (its teardown runs on a worker).
#[cfg(not(target_arch = "wasm32"))]
fn assert_send<T: Send>() {}

/// Permanent guard: adding a non-`Send` field (an `Rc`, a non-`Send` trait object) to anything
/// `StudioApp` owns breaks the build here instead of the off-GUI-thread teardown. Native only: the
/// worker-thread teardown is a native-shell path; the web build drops the app in place.
#[cfg(not(target_arch = "wasm32"))]
const _: fn() = assert_send::<StudioApp>;

impl StudioApp {
    /// Final shutdown drain for the background layer saver and the other writers: runs on real
    /// process exit, return-to-launcher, and the structural-operation reload. Unless discarding,
    /// flushes the canvases' bubble edits, the PS editor's deferred layer edits and the typing
    /// tab's deferred text edits, then blocks until every queued layer write is flushed and the
    /// worker thread is joined, so no pending raster/text/effects write is lost on exit. Must NOT hold
    /// the doc lock across the barrier: it clones the handle, drops the lock, then barriers; finally
    /// `shutdown_saver` drains the queue + joins the worker. Relying on Arc-drop ordering alone is
    /// unsafe (a lingering Arc clone in a worker could skip the join).
    ///
    /// Blocking: meant for a worker thread (or the process-shutdown hook). Idempotent, because every
    /// step it calls is (a saver already shut down is skipped). Returns what could not be made
    /// durable; each item is logged here too.
    #[cfg_attr(
        target_arch = "wasm32",
        expect(dead_code, reason = "the web entry drops StudioApp in place and never tears it down; remove when the deferred web port of the main-window shell (unified-window WP-6) runs the teardown")
    )]
    pub(crate) fn shutdown_blocking(&mut self) -> ShutdownReport {
        let mut report = ShutdownReport::default();
        // An image-set session always closes the DISCARD way (plan D13): its staging is a scratch
        // the shell's teardown deletes right after this app is dropped, so nothing below may write
        // into it (`SessionMode::before_shutdown`; no-op for a project, and when a dialog already
        // took that path).
        self.session.before_shutdown(&mut self.writers, &mut self.tabs.typing);
        // Push every canvas's pending bubble edits (the 1 s text debounce included) into the
        // bubbles model, so the bubbles-saver shutdown in `drain_for_shutdown` writes them; then
        // enqueue the PS editor's deferred layer edits (visibility / opacity / collapse through
        // `edit_doc_node`, otherwise written only on tab leave, page switch, save or page op) on the
        // layer saver, so the barrier in `drain_for_shutdown` covers them. Same relative order as
        // save-to-project and the page op (bubbles, PS layers, typing text) and as
        // `ViewSync::before_draw` (PS before typing): typing's text flush is the first step of
        // `drain_for_shutdown`, and nothing in between (клин stop, decode-pool release) touches
        // the layer saver. Never on the DISCARD path (an image set always takes it): those edits
        // are thrown away on purpose, and the PS fallback write would re-create deleted staging.
        if self.writers.is_discarding() {
            runtime_log::log_info("[app] on_exit: discard path — pending canvas bubble edits and PS-editor layer edits dropped, not flushed");
        } else {
            let (_, mut parts) = self.session_parts();
            parts.flush_canvas_bubble_edits();
            if parts.ps_editor.has_deferred_layer_edits() {
                runtime_log::log_info("[app] on_exit: flushing the PS editor's deferred layer edits");
            }
            parts.ps_editor.flush_layers_if_dirty(parts.project);
        }
        self.writers.stop_overlay_autosave_blocking();
        // Release decode workers parked on the look-ahead window: after this frame nothing
        // drains the loader channels, so the promotion frontier would never advance again and
        // parked workers (and the pool joiner) would leak. Workers blocked on `send` exit on
        // their own once the receivers drop with `self`.
        self.residency.shutdown();
        // 0-3) Typing deferred-edit flush (skipped when discarding), layer-saver barrier and
        // shutdown, bubbles-saver shutdown; failures are logged and written into `report`.
        self.writers.drain_for_shutdown(&mut self.tabs.typing, &mut report);

        // 4) Persist every tab's exit-only `user_config` keys (the Translation and Typing bottom-hint
        // collapsed flags, the two typing centering-assist checkboxes) in ONE locked read-modify-write;
        // each tab writes its own keys into the root (`StudioTab::write_exit_settings`). Exit-only:
        // teardown runs on both program close and return-to-launcher, so this is the one save point.
        persist_exit_settings(&self.tabs);

        // 5) Flush the tabs' debounced user-global stores (typing: the per-font settings write, then
        // the default local-preset set). Their debounced writers are detached threads that die with
        // the process, so an edit made a second before closing would otherwise be lost. Synchronous
        // by necessity: no GUI frame follows and no thread outlives this call. Why a deadline would
        // trade a visible hang for a silent loss is argued at
        // `local_presets::flush_pending_local_presets_save` and in
        // `crates/ms-tab-typing/src/panel/MODULE_README.md`.
        self.tabs.flush_debounced_stores_blocking();

        // 6) Flush the dockable-panel layouts. A last poll first, because the frame that raised
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

/// Writes every tab's exit-only `user_config` keys (`Tabs::write_exit_settings`) in ONE call of
/// `update_user_config_file`, the canonical locked read-modify-write (atomic replace), so unrelated
/// keys survive. A malformed existing document is NOT overwritten. Teardown-only: a failure is
/// logged and the file is left as it was, so a failed write never blocks shutdown.
fn persist_exit_settings(tabs: &Tabs) {
    let path = crate::config::user_config_path();
    if let Err(err) = crate::config::update_user_config_file(&path, |root| {
        tabs.write_exit_settings(root);
        Ok(())
    }) {
        runtime_log::log_error(format!(
            "[app] on_exit: failed to persist the tabs' exit settings; file left unchanged; path={}; cause={err:#}",
            path.display()
        ));
    }
}
