# Module: crates/ms-custom-fonts/src/hybrid

## Purpose
Compiler of the hybrid kind: a face whose glyphs, metrics and kerning are weighted averages of
2..=4 real fonts.

## Architecture
```text
compile (mod.rs)
  prepare: validate recipe, match sources by position + identity, parse (source.rs)
  plan.rs: per character -> included with a participating-source mask, or disabled (report)
  glyph jobs: one per distinct combination of source glyphs, in first-character order
  blend_glyph (rayon, chunks of 64; progress after each chunk, cancel per glyph):
      one source with ink    -> copy
      compatible.rs          -> direct point interpolation (same point structure, exact)
      matched.rs + trace.rs  -> contour correspondence (DTW) interpolation (same topology)
      else sdf.rs + trace.rs -> distance-field blend, re-traced into curves
  metrics.rs: font-wide metrics;  kerning.rs: inherited pairs
  -> sfnt::FontBuild -> write_font
```

## Files and submodules
- `mod.rs`: the pipeline, glyph jobs, per-glyph dispatch, naming, `.notdef`.
- `plan.rs`: the missing-glyph rule, the `CoverageReport` and `included_chars` (one owner for
  all three, so neither the report nor a caller's coverage classification can disagree with
  the compiled cmap).
- `compatible.rs`: correspondence (contour pairing, start rotation, plausibility) and point
  interpolation.
- `matched.rs`: same-topology path — contour pairing, arc-length + DTW correspondence,
  dense interpolation, plausibility (orientation, area, no crossing edges).
- `sdf.rs`: per-glyph alignment choice, signed distance fields, weighted average, marching
  squares.
- `trace.rs`: corner split and least-squares cubic fitting of the level-set polylines.
- `metrics.rs`: weighted vertical metrics / style; `units` rounding helper.
- `kerning.rs`: weighted average of the sources' effective pairs.
- `tests.rs`: end-to-end tests on the bundled `fonts/ui` fonts (skip when absent).

## Contracts and invariants
- Missing glyphs (D9): for each character the sources lacking it are checked with their
  effective policy (override, else default); any `Disable` -> absent; else blended from the
  sources that have it, weights renormalized. A character no source has is not counted.
- Empty outline in a source that has the character: shapes come from the inked sources only;
  every participating source still counts for the advance.
- Units: everything is normalized to 1000 units/em; CFF sources are reversed to TrueType
  orientation on read; output outer contours are clockwise.
- Engine constants (SDF `SDF_CELL_UNITS` = 4, seed band, margin, alignment lattice; matched
  spacings, band, tangent weight, plausibility ratios; fit tolerance 0.75 units, corner
  window/threshold) are output semantics: changing one bumps `ENGINE_VERSION`.
- Path order is quality order: exact compatible interpolation, then the matched path (strokes
  move, nothing fades), then the SDF path only for topology mismatches. Measured on two
  unrelated comic fonts, the matched path removed every island / broken-stroke artefact the
  SDF path produced for same-topology letters.
- Known limits: a feature only one design has (a foot on "1", a tail on "l") is interpolated
  to a partial feature; in the SDF path (different contour counts: single- vs double-storey
  "g", CJK strokes that overlap in one design and not in the other) strokes that do not
  overlap fade instead of moving and can leave small partial features at intermediate
  weights, and corners whose position differs between sources come out slightly rounded.
- Kerning (D6): contributors of a pair are the sources having both characters (weights
  renormalized over them); a contributor without the pair adds 0; a pair no contributor has is
  absent; results in font units (= per-mille at 1000 units/em), rounded, zeros dropped.
- No cross-glyph state: every glyph job is a pure function of its sources; output is assembled
  by glyph index.
- Bounded work on hostile sources: the SDF path rejects an aligned glyph wider or taller than
  `MAX_BLEND_EXTENT_UNITS` (8x the storable 16-bit range) or with non-finite coordinates as
  `CompileError::GlyphTooComplex` (alignment candidates beyond it are skipped first); grid node
  counts are constant-time and the coarsening loop is bounded.

## Editing map
- Change which characters exist: `plan.rs` (and its tests in `tests.rs`).
- Improve exactness for same-structure glyphs: `compatible.rs`; same-topology glyphs:
  `matched.rs`.
- Blend quality / speed: `sdf.rs` (grid, alignment), `trace.rs` (fitting); measure with
  `examples/hybrid_gallery.rs` and bump `ENGINE_VERSION`.
- Inherited kerning rules: `kerning.rs`; built-in pair reading lives in `../kerning.rs`.
