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

## Contracts and invariants
- `shutdown_blocking` blocks: it runs from the window host's `on_exit`, before a structural
  reload, or on a worker that owns the moved `Box<MangaApp>` — never in a regular frame.
- Every failure it observes is logged with the `[app] on_exit:` prefix AND returned in
  `ShutdownReport`; it never claims a drain it did not observe.
- `MangaApp: Send` must hold on native targets: a non-`Send` field anywhere under `MangaApp`
  (including a `Box<dyn CleaningTool / AiEngine / PsTool>` implementor) is a build error here.

## Editing map
- Change the teardown order or what it reports: `shutdown.rs`, and the "exit choreography"
  bullet in `src/MODULE_README.md`.
