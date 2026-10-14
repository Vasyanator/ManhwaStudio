# Module: crates/ms-text-render/examples/render_gallery

## Purpose
Deterministic golden-image regression harness for the production text renderer
(`render_next::render_text_to_image`). Renders a fixed feature-matrix of cases to
PNGs so the vector-engine refactor (`render_next/VECTOR_ENGINE_REFACTOR.md`) can
be diffed visually / by tolerance before and after each phase.

This is a test/tooling example of the `ms-text-render` package. It renders the REAL engine; it does not copy or
reimplement any rendering logic.

## Architecture
The production renderer is the `ms-text-render` crate itself, so this example simply
links it (`ms_text_render::render_text_to_image` + `::types::*`). It needs no
`#[path]` module mounts and pulls in no egui/app/GUI code.

## Files and submodules
- `main.rs`: the fixed `all_cases()` set, the per-family case builders, PNG
  writing, the generated custom-raster-line layout source
  (`raster_line_pixels` / `write_raster_line_source`), the pure
  `rgba_diff`/`DiffStats` compare helper, and self-check `#[test]`s.

## Contracts and invariants
- Fully deterministic: no randomness, no time, fixed text/params/geometry.
- Uses the repo font `test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf`
  (Latin + Cyrillic), resolved via `CARGO_MANIFEST_DIR` (the crate directory) + `../../`.
- Run: `cargo run -p ms-text-render --example render_gallery -- <outdir> [aa_mode]`;
  its self-check tests run with `cargo test -p ms-text-render` (`test = true` on the
  `[[example]]` table in the crate manifest).
- `main` writes `<argv[1]>/<case>.png` and prints one `name: WxH` line per case.
- `rgba_diff` is pure: equal dimensions required, returns `Result`; never panics.
- Harness code stays clippy-clean.
- CASE NAMES ARE A REGRESSION BASELINE. Never rename or repurpose an existing
  case: a golden PNG is compared by name against a run from before a change.
  Add a new name instead, and when a change is meant to alter one case's output,
  say which one — every other PNG must stay byte-identical.
- On-path cases come in `<name>` / `<name>_by_length` pairs that share their
  geometry and text and differ only in `distance_mode`, so the difference between
  the two spacing modes is readable from one diff.
- ALL FOUR on-path layout modes are covered, because they share one step owner
  (`OnPathStepSpacing`) and a change to it must not be able to hide in an
  uncovered mode: `CustomVectorLines` (the `onpath_*` cases), `Formula`
  (`formula_wave`) and `CustomRasterLines` (`raster_line_arc`). `Shape` is
  covered by `shape_oval`/`shape_rect`.
- INPUTS ARE NOT GOLDENS. `main` generates the layout image the
  `CustomRasterLines` case traces into `<argv[1]>/inputs/raster_lines.png`, a
  SUBDIRECTORY, so a `<outdir>/*.png` comparison never picks it up. It is
  generated rather than committed (closed-form geometry, so still deterministic)
  and its line is exactly ONE pixel per column: the raster tracer walks
  8-connected neighbours and doubles back to collect leftovers if the chain is
  thicker anywhere, which folds the path.

## Editing map
- To add/adjust a golden case, edit `all_cases()` in `main.rs`.
- To change the custom-raster-line geometry, edit `raster_line_pixels`; keep the
  one-pixel-per-column property or the traced path folds.
- To change how the engine is reached, edit the `use ms_text_render::...` imports at
  the top of `main.rs` (the example links its own package, no manifest edge needed).
- To change comparison semantics, edit `rgba_diff` / `DiffStats` and their tests.
