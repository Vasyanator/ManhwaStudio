# Module: crates/ms-text-render/src/formula/render

## Purpose
The on-path render passes: `Formula` and `Shape` (glyphs along an evaluated formula
curve) and `CustomRasterLines` / `CustomVectorLines` (glyphs along user-drawn line
paths), including the `MinimumPreviousDistance` ink spacing. The detailed contracts
(alignment pre-pass, the one on-path step owner, ink spacing in six parts, mesh warp,
extra info) are written in the header of `mod.rs` and in `../MODULE_README.md`.

## Architecture
`mod.rs` owns the three `pub(crate)` entry points that `formula/mod.rs` re-exports, the
shared types and the tuning constants. Each entry point retries its `_once` pass:

1. `seeds::collect_formula_glyph_seeds` turns the shaped buffer into detached
   `FormulaGlyphSeed`s; `seeds::assign_formula_seed_advances` gives each its advance
   (pair decision from `crate::pair_advance`).
2. Formula / shape: `formula_layout::render_text_with_formula_layout_once` maps seeds
   onto the curve through its arc-length table.
3. Custom lines: `drawn_lines_layout::render_text_with_drawn_lines_layout_once` computes
   per-line start offsets, then `line_transforms::build_drawn_line_transforms` walks
   each line; `MinimumPreviousDistance` lines measure ink (`seed_ink`) and solve for the
   arc position (`path_geometry` solvers).
4. Both passes composite outlines through `crate::glyph_blit` and reuse the per-glyph
   inline lookups, wrap hyphen and line tables imported from `crate::pipeline`.

## Files and submodules
- `mod.rs`: entry points, `OnPathStepSpacing` (the ONE answer to "how far along the
  path is the next glyph"), seeds / transforms / caches / placement-state types,
  `apply_line_placement`, `resolve_glyph_outline`, `image_has_alpha_on_edge`.
- `drawn_lines_layout.rs`: the custom raster / vector line pass and its alignment
  pre-pass (`drawn_line_start_offsets`, `on_path_align_fraction`, `LineArcCursor` impl).
- `line_transforms.rs`: one custom line's transforms — straight reference walk and
  median target gaps, drop side, `drawn_line_seed_transform`, measured-run refinement.
- `seed_ink.rs`: cached glyph ink, its placement for a transform, on-path / straight
  reference transforms, chord pair gap and the clearance floor.
- `path_geometry.rs`: custom-line settings, flip, offset-group rotations, path sampling
  in the text direction, `solve_directional_gap_center_s`, `find_clearance_center_s`.
- `formula_layout.rs`: the formula / shape pass, shape fallback decision, curve
  arc-length table.
- `seeds.rs`: seed collection, `assign_formula_seed_advances` (a CUSTOM KERNING site),
  `seed_metric_advance_px`, the optical pair adjustment and glyph ink profiles.
- `tests.rs`: unit tests of the directory, including the seed characterization matrix
  `seed_advances_match_the_recorded_characterization` (fixture
  `../../../tests/fixtures/pair_advance_seed_matrix.json`).

## Contracts and invariants
- Types and constants stay in `mod.rs`; a child item used outside its file is
  `pub(super)`. Children reach `formula/mod.rs` items as `crate::formula::…`.
- `OnPathStepSpacing::step_px` is the only step expression; the alignment pre-pass, the
  custom-line walk and the formula accumulator all call it. Never fork it.
- `StraightReferenceWalk` is the single owner of the reference recurrence shared by the
  median pre-pass and the real walk.
- No copies of `pipeline` lookups, hyphen or line tables here.
- Gap measurement is `crate::pair_gap`; the optical formula is `crate::optical`.

## Editing map
- On-path step / seed floors: `OnPathStepSpacing` in `mod.rs`.
- Seed advances, user-authored kerning on paths: `seeds.rs`.
- Custom-line alignment and drop at path ends: `drawn_lines_layout.rs`, `line_transforms.rs`.
- `MinimumPreviousDistance` measure / target / search: `seed_ink.rs`,
  `line_transforms.rs`, `path_geometry.rs`.
- Formula curve mapping and the shape fallback: `formula_layout.rs`.
