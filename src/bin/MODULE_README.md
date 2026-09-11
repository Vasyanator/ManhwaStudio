# Module: src/bin

## Purpose
Standalone binaries used for focused UI, rendering, and algorithm testing outside the main
application flow. They are development diagnostics, not production entry points.

## Architecture
Each binary owns its test app state and should call shared modules only when it is testing their
real behavior. Heavy work must be moved to a background thread just as it would be in the main
application.

Shared UI code is reached through a normal crate dependency (`ms-widgets` and the other `ms-*`
crates), NOT through `#[path = ...]`. The one remaining `#[path]` mount is
`tutorial_test` -> `crates/ms-settings-ui/src/tutorial/engine.rs`: that module is gated behind the
`tutorial` feature there, which the demo must build WITHOUT, and it still has
no library target. Production behavior must live in a crate or in a normal `src/` module used by
the main application, never in a binary-local fork.

## Files and submodules
- `text_edit_plus_test.rs`: focused egui tester for `TextEditPlus` text colors and ordered
  rounded background highlights.
- `text_render_test.rs` and `text_render_test/`: GUI tester for cosmic-text rendering and text
  effects, including a local render module for experimental renderer behavior.
- `test_text_shape.rs`: isolated tester for shape-aware text wrapping in character-width units.
- `test_center_find.rs`: focused utility/test entry point for center-finding behavior.

## Contracts and invariants
- Test binaries must not introduce fake behavior into runtime modules.
- Shared code comes from a workspace crate. `#[path = ...]` is allowed only for a module that
  cannot be reached as a normal item (today: `crates/ms-settings-ui/src/tutorial/engine.rs`, which
  is behind a feature the demo does not enable), and the mounted module
  must remain usable by the main application unchanged.
- GUI test binaries should report startup errors to stderr.
- Diagnostic code may use fixed fixture paths, but missing fixtures must fail visibly instead of
  producing placeholder data.
- Long image analysis or text rendering from a diagnostic GUI must run on a worker thread.

## Editing map
- To add an isolated widget demo, create a small `eframe::App` binary here.
- To test production render behavior, prefer calling the real render module rather than copying
  algorithms.
- To change experimental renderer diagnostics, edit `text_render_test.rs` and
  `text_render_test/render.rs`.
