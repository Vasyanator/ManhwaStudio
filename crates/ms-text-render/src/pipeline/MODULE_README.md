# Module: crates/ms-text-render/src/pipeline

## Purpose
The render pipeline of the crate: `render_text_to_image` (the production entry,
re-exported from the crate root), `apply_effects_to_image`, the smoke helper, and the
whole horizontal text path (pen, glyph passes, line stacking). It routes formula / shape /
custom-line layouts to `formula::render` and vertical text to `layout::vertical`. The
crate-level contracts it implements (CUSTOM KERNING, HANGING PUNCTUATION, GLYPH HEIGHT
SCALE, UNSERVICEABLE-ATTRS GUARD, FAUX STYLE) are written in `../MODULE_README.md`.

## Architecture
`mod.rs` owns the orchestration and every type the children share; the children are
cohesive groups of free functions and `impl` blocks over those types. Data flow of one
horizontal render:

1. `mod.rs::render_text_to_image` prepares the text (`text_prep`), degrades
   unserviceable italic/bold (`faux`), shapes, and builds the line tables
   (`line_metrics`, `text_prep` alignment).
2. Each `LayoutRun` is laid out by the pen (`run_layout::horizontal_run_layout`), which
   reads the per-glyph inline overrides (`inline_lookup`) and faux growth (`faux`) and
   takes every pair decision from `crate::pair_advance`.
3. The unrotated pass collects and draws placements (`horizontal_draw`); text with
   inline rotations goes through `rotated::render_horizontal_rotated`.
4. The wrap hyphen (`hyphen`) and hanging punctuation (`hanging`) are shared with the
   formula / custom-line path.

The wrap prepass measures candidate lines with the same pen (`PenRunWidth` in `mod.rs`,
implementing `wrap::RunWidthMeasure`), so a line breaks where it is drawn.

## Files and submodules
- `mod.rs`: entry points, the wrap / shape prepass, routing by `TextLayoutMode`, and the
  shared types (`GlyphScaleSettings`, `GlyphCaches`, `GlyphStyle`, `RunPen`,
  `RunShaping`, `HorizontalRunLayout`, `PenRunWidth`, `HorizontalGlyphPlacement`,
  `RotatedGlyphPlacement`, `FauxGlyphStyle`, `InlineHeightRoom`), plus the re-exports
  that keep `crate::pipeline::x` working for the vertical and formula paths.
- `run_layout.rs`: the horizontal pen — `horizontal_run_layout` (metric `Auto` /
  `Fixed`), `optical_horizontal_run_layout` (ink-to-ink re-spacing),
  `place_optical_horizontal_contour`, `faux_floored_advance`,
  `anchor_rtl_run_at_its_left_edge`. A CUSTOM KERNING site.
- `horizontal_draw.rs`: unrotated glyph placement collection and draw, extra-info
  samples of a placement.
- `rotated.rs`: inline-rotated horizontal path (placements, group / global rotation,
  `RigidPlacement` impl, rotated draw, extra-info samples).
- `text_prep.rs`: `prepare_source_text` (ellipsis expansion, uppercase, trimming,
  sentence newlines — in that order), justify mapping, per-line alignment, layout-line
  byte offsets.
- `inline_lookup.rs`: the ONE owner of the per-glyph inline lookups
  (`inline_*_at_offset` / `inline_*_for_glyph`).
- `faux.rs`: faux bold / italic resolution, advance growth and bounds pads, the
  whole-overlay and inline degrade of real italic / bold, `SYNTHESIZED_ITALIC_SLANT_DEG`.
- `line_metrics.rs`: per-line extra spacing, `InlineHeightRoom` measurement, baseline
  advance tables.
- `hyphen.rs`: the ONE owner of the synthesized hard / wrap hyphen and its attrs.
- `hanging.rs`: hanging-punctuation classification, edge-run widths and bounds.
- `tests.rs`: unit tests of the whole directory, including the pen characterization
  matrix `pen_layout_matches_the_recorded_characterization` (fixture
  `../../tests/fixtures/pair_advance_pen_matrix.json`, helpers in `crate::characterization`).

## Contracts and invariants
- Types stay in `mod.rs`; children never declare a type another child needs. A child
  item used outside its file is `pub(super)`; an item other modules import is
  `pub(crate)` and re-exported by `mod.rs` (`pub(crate) use child::item`).
- Children import the crate by `crate::` paths, never `super::<sibling module of
  pipeline>` (inside a child `super` is `pipeline` itself).
- The pen in `run_layout.rs` accumulates `PairStep`s only; the pair decision is
  `pair_advance`'s. Never re-implement any part of it here.
- `inline_lookup.rs`, `hyphen.rs` and the line tables are imported by
  `formula::render` and `layout::vertical`: keep them the single implementation.
- Pure rendering: no GUI, project, storage or filesystem access (fonts come through
  `FontProvider`).

## Editing map
- Pen spacing, tracking, optical re-spacing, RTL anchoring: `run_layout.rs`.
- How a horizontal glyph is drawn or bounded: `horizontal_draw.rs` (rotated: `rotated.rs`).
- Text normalization order, alignment, line offsets: `text_prep.rs`.
- A new inline per-glyph override lookup: `inline_lookup.rs`.
- Faux bold / italic or the italic / bold degrade: `faux.rs`.
- Line spacing and `<stretching>` height room: `line_metrics.rs`.
- Wrap hyphen glyph: `hyphen.rs`; hanging punctuation: `hanging.rs`.
- Routing, the wrap prepass, cancellation, effects: `mod.rs`.
