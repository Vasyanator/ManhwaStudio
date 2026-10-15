# Module: src/studio/session/project

## Purpose
`ProjectSession`, the project (chapter) arm of `SessionMode`: the persistence state that exists
only while a real chapter is open, and the flows that act on it — save-to-project (with its
deferral behind the typing tab's whole-project layer preload), structural page operations, the
DISCARD exit, the project exit dialog, and the cached "unsaved changes" flag.

## Architecture
```text
ProjectSession
  save_to_project_rx / _status, pending_save_after_preload, pending_save_completion_action  (save.rs)
  page_op_rx, page_op_error, pending_project_reload                                         (page_op.rs)
  pending_exit_cleanup (delete job + its close action)                                       (discard.rs)
  has_unsaved_changes_cached, next_unsaved_dir_check_s                                       (mod.rs)
tick(ctx, now, parts): drive preload -> poll save -> poll page op -> poll cleanup ->
                       refresh unsaved cache -> status expiry -> save-then-close
```
Every flow takes `&mut SessionParts` (`../mod.rs`): the project, `StagingWriters` (gate, клин
control and thread, DISCARD latch, the three shared models), `CloseFlow`, and the tab states it
flushes or notifies. Workers get Arc clones of the models from `StagingWriters`.

## Files and submodules
- `mod.rs`: the struct, `new` (one stat of the staging dir), `tick`, `draw_top_bar`, the busy jobs
  (`busy_jobs` / `activity`, read by `studio/busy.rs` predicates), `refresh_unsaved_changes_cache`.
- `save.rs`: `request_save_to_project`, `drive_pending_save_preload`, `start_save_to_project`,
  `poll_save_to_project`; the pure gates `save_trigger_decision` / `deferred_save_ready` + tests.
- `page_op.rs`: `start_page_op`, `poll_page_op`, `draw_page_op_overlays`; the pure
  `page_op_text_quiesce` + tests.
- `discard.rs`: `start_exit_cleanup`, the delete job, `poll_pending_exit_cleanup`,
  `abort_discard_after_failed_cleanup`.
- `exit_dialog.rs`: the project exit dialog (Exit / ReturnToLauncher / OpenIncoming variants) and
  its open-incoming body.

## Contracts and invariants
- Each sequence stays in ONE method, in the order of "Save / page-op / discard / exit
  choreography" (`src/studio/MODULE_README.md`): a save aborts on any unverified text flush or barrier; a
  page op refuses before anything moves when the typing text quiesce fails or its worker thread
  cannot start (the page-op and discard workers are started parked BEFORE any writer is quiesced
  and released through a start channel, so a refused thread leaves the session untouched); a
  discard latches first and never flushes; a failed discard restarts every writer and re-reports unsaved.
- No I/O on the GUI thread except the documented per-second staging-dir probe of
  `refresh_unsaved_changes_cache` (and one stat in `new`); merges, deletes and page ops run on
  `ms_thread` workers.
- The cached unsaved flag is sticky: only a successful save or discard clears it; it is forced to
  false while the DISCARD latch is set.
- The save-then-close (`pending_save_completion_action`) is armed ONLY by an exit dialog's Save,
  cleared by its Cancel, and taken by `tick` when that save ends (`save_then_close_end`): a clean
  success closes; a failed, refused or aborted save, or one that left unsaved work, closes the
  dialog and keeps the session running with the save's own error in the status line.
- A save starts only when no `SaveBlocker` holds (`PersistenceActivity::save_blocker`: the page-op
  set plus the clean worker): never during a page op, a clean op, the DISCARD cleanup or a typing
  export. The toolbar button is disabled on any blocker; the exit dialog's Save on every blocker
  but a save already under way; a refused trigger shows the blocker's text.

## Editing map
- Save abort rules or worker order: `save.rs::start_save_to_project`; trigger gating: `save.rs`
  pure fns and their tests.
- Page-op preconditions or worker order: `page_op.rs::start_page_op` / `page_op_text_quiesce`.
- Discard / failed-discard handling: `discard.rs`; the writer half: `src/studio/staging_writers.rs`.
- Exit dialog buttons: `exit_dialog.rs`; which dialog is open: `src/studio/close_flow.rs`; how a
  save-then-close ends: `mod.rs::save_then_close_end` + `CloseFlow::end_save_then_close`.
- What counts as unsaved: `mod.rs::refresh_unsaved_changes_cache`; which tab-held edits count:
  `session::DeferredTabEdits` (`../mod.rs`).
