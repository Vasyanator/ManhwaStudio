# Module: src/app

## Purpose
Child modules of the editor root `MangaApp` (`src/app.rs`). They exist so responsibilities can
leave `app.rs` (over the 5000-line gate) while still reaching `MangaApp`'s private fields and
helpers without widening their visibility. The architecture of the root itself, and the
save / page-op / discard / exit choreography, are documented in `src/MODULE_README.md`.

## Files and submodules
- `shutdown.rs`: `MangaApp::shutdown_blocking() -> ShutdownReport`, the studio's blocking
  teardown (клин autosave stop and join, typing deferred-edit flush, layer-saver barrier and
  shutdown, bubbles-saver shutdown, exit-only settings, debounced typing stores, panel layouts),
  plus the permanent compile-time `MangaApp: Send` guard.
- `incoming_open.rs`: the session's answer to images another app delivered
  (`request_close_for_incoming`, `take_incoming_declined`): the unsaved-changes dialog's
  `OpenIncoming` variant (its body, `draw_incoming_unsaved_dialog_body`) or the one-line
  confirmation (`draw_incoming_confirm`), and the pure `incoming_decision` policy.

## Contracts and invariants
- `shutdown_blocking` blocks: it runs from the window host's `on_exit`, before a structural
  reload, or on a worker that owns the moved `Box<MangaApp>` — never in a regular frame.
- Every failure it observes is logged with the `[app] on_exit:` prefix AND returned in
  `ShutdownReport`; it never claims a drain it did not observe.
- `MangaApp: Send` must hold on native targets: a non-`Send` field anywhere under `MangaApp`
  (including a `Box<dyn CleaningTool / AiEngine / PsTool>` implementor) is a build error here.

- `request_close_for_incoming` never touches the images (the window host keeps them) and never
  closes anything itself: it ends in `PendingCloseAction::OpenIncoming` through the regular close
  flow, or in a decline. A close decided or asked for another reason wins (refused, `false`). A
  clean confirmation re-checks unsaved changes on «Открыть» and switches to the unsaved dialog.

## Editing map
- Change the teardown order or what it reports: `shutdown.rs`, and the "exit choreography"
  bullet in `src/MODULE_README.md`.
- Change how a running session answers delivered images: `incoming_open.rs` (the hooks in
  `app.rs`: `ExitDialogKind::OpenIncoming`, `draw_exit_dialog`, the `ui` call).
