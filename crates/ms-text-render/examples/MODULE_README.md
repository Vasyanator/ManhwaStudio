# Module: crates/ms-text-render/examples

## Purpose
Development diagnostics of the text renderer, built as Cargo examples of the
`ms-text-render` package. They are not production entry points and are never shipped or
signed (the root `build.rs` signs only the `manhwastudio_rs` binary).

## Architecture
Each example is its own crate root that links the `ms-text-render` library and the
package's `[dev-dependencies]` (`eframe`, `ms-theme`, `ms-platform`, `anyhow`); the
library itself stays GUI-free. Heavy rendering in a GUI example runs on a worker thread,
as it would in the application.

## Files and submodules
- `render_gallery/`: deterministic golden-image regression harness over the REAL renderer
  (`render_text_to_image`); `test = true` on its `[[example]]` table, so its self-check
  tests run under `cargo test -p ms-text-render`. See `render_gallery/MODULE_README.md`.
- `text_render_test.rs` + `text_render_test/`: egui tester for cosmic-text rendering and
  text effects over an example-local experimental renderer (`text_render_test/render.rs`,
  mounted with `#[path]`). See `text_render_test/MODULE_README.md`.
- `test_text_shape.rs`: isolated egui tester of shape-aware wrapping in character-width
  units (a standalone copy of the wrap DP; no font measurement).

## Contracts and invariants
- No fake behaviour reaches the library: production behaviour lives in `src/`, an example
  only calls it or prototypes next to it.
- GUI examples report startup errors to stderr; missing fixtures (fonts, images) fail
  visibly instead of producing placeholder output.
- Fixture paths are resolved from the repository root (current directory) or, for
  `render_gallery`, from `CARGO_MANIFEST_DIR` + `../../`.

## Editing map
- Run one: `cargo run -p ms-text-render --example <name>` from the repository root.
- To add a renderer diagnostic, add `<name>.rs` (or `<name>/main.rs`) here; declare a new
  dependency under `[dev-dependencies]` (workspace table rules of the root manifest).
