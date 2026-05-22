# Module: src/bin

## Purpose
Standalone binaries used for focused UI, rendering, and algorithm testing outside the main
application flow. They are development diagnostics, not production entry points.

## Architecture
Each binary owns its test app state and should call shared modules only when it is testing their
real behavior. Heavy work must be moved to a background thread just as it would be in the main
application.

Some binaries include shared source with `#[path = ...]` because the project has no library target.
That is acceptable for diagnostics, but production behavior must still live in normal `src/`
modules used by the main application.

## Files and submodules
- `text_edit_plus_test.rs`: focused egui tester for `TextEditPlus` text colors and ordered
  rounded background highlights.
- `text_render_test.rs` and `text_render_test/`: GUI tester for cosmic-text rendering and text
  effects, including a local render module for experimental renderer behavior.
- `test_text_shape.rs`: isolated tester for shape-aware text wrapping in character-width units.
- `test_center_find.rs`: focused utility/test entry point for center-finding behavior.

## Contracts and invariants
- Test binaries must not introduce fake behavior into runtime modules.
- Binaries may include shared source files with `#[path = ...]` when no library target exists, but
  the included module must remain usable by the main application.
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
