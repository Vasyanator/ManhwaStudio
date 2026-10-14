# Module: crates/ms-widgets/examples

## Purpose
Focused widget demos, built as Cargo examples of the `ms-widgets` package. Development
diagnostics only: not production entry points, never shipped or signed.

## Files and submodules
- `text_edit_plus_test.rs`: egui tester for `TextEditPlus` per-range text colours and
  ordered rounded background highlights.

## Contracts and invariants
- A demo uses the crate's real widgets, never a fork, so the demo and the application
  compile the same code.
- A demo does not call `ui_fonts::install` (only product `run_native` creators do).
- Startup errors go to stderr.

## Editing map
- Run: `cargo run -p ms-widgets --example text_edit_plus_test`.
- To add an isolated widget demo, add a small `eframe::App` example here.
