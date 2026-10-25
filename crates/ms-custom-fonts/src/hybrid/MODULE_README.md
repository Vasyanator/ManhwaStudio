# Module: crates/ms-custom-fonts/src/hybrid

## Purpose
Compiler of the hybrid kind: a face whose glyphs, metrics and kerning are weighted averages of
2..=4 real fonts, with the per-glyph outline path chosen by the recipe's `BlendAlgorithm`.

## Architecture
```text
compile (mod.rs)
  prepare: validate recipe, match sources by position + identity, parse (source.rs)
  plan.rs: per character -> included with a participating-source mask, or disabled (report)
  effective_weights: shape / spacing / kerning per source (recipe.rs owns the rule)
  glyph jobs: one per distinct combination of source glyphs, in first-character order
  transfer only: transfer::measure every source once, transfer::targets (before the loop)
  blend_glyph (rayon, chunks of 64; progress after each chunk, cancel per glyph):
      no source with ink -> empty (copied)
      Transfer: transfer::apply on the base source's glyph, also when only one source has
                ink (base without ink -> the heaviest inked source, with that source's
                parameters; counted as substituted)
      exactly one source with ink -> copy (every other algorithm)
      Auto:     compatible.rs -> matched.rs (+trace) -> sdf.rs (+trace)
      Sdf:      sdf.rs
      NoBlend:  copy of the inked source with the highest shape weight (ties: recipe order)
      Skeleton: compatible.rs -> skeleton/ -> sdf.rs on SkeletonFallback
  -> (GlyphData, BlendPath) per glyph, summed in index order into BlendStats
  metrics.rs: font-wide metrics (spacing weights);  kerning.rs: inherited pairs (kerning weights)
  -> sfnt::FontBuild -> write_font
```

## Files and submodules
- `mod.rs`: the pipeline, glyph jobs, per-glyph algorithm dispatch (`GlyphAlgorithm`,
  `BlendPath` -> `BlendStats`), naming, `.notdef`.
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
- `skeleton/`: stroke-skeleton blend of the `Skeleton` algorithm (thinning, stroke graph,
  correspondence, placement, rebuild) and its typed `SkeletonFallback` (`Display` = stats
  reason key); see `skeleton/MODULE_README.md`.
- `transfer.rs`: `Transfer` algorithm — `FontParams` measurement per face from fixed reference
  glyph chains, `Targets` (shape-weighted means), per-glyph `apply` (zone scale -> shear ->
  width -> thickness via `sdf::offset_outline`).
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
  window/threshold; transfer chains, clamps and skip thresholds; the skeleton constants) are
  output semantics: changing one bumps `ENGINE_VERSION`.
- Weights: shape weights (renormalized over the inked participants) drive the outline paths,
  the `NoBlend` winner and the transfer substitute; spacing weights (over the participants)
  the advance and (over all sources) the metrics; kerning weights the pairs. With property
  weights equal to the plain weights every glyph table is byte-identical to the baseline
  recipe's (only the key-derived names differ; tested).
- `BlendStats`: every compiled glyph is counted in exactly one path; the sum is the glyph
  count without `.notdef`. Skeleton fallbacks are counted as `skeleton_fallback` (with reason:
  `too_complex`, `too_many_strokes`, `no_skeleton`, `unmatched`, `extent`), not as `sdf`;
  transfer base substitutions are a subset of `transfer` (`transfer_base_substituted`).
- Algorithms and when each path runs (two or more inked sources unless noted):
  `Auto` exact compatible -> matched -> SDF; `Sdf` always SDF; `NoBlend` no blend at all, the
  heaviest-shape source's glyph; `Skeleton` compatible, else strokes are paired and blended so
  a stroke one design lacks grows / shrinks instead of fading, else SDF; `Transfer` keeps the
  base font's glyph shapes and moves only its measured parameters (stems, width, x / cap
  height, slant) to the shape-weighted target, for every glyph including single-source ones.
- Transfer measurement: font-wide, read before the parallel loop from fixed reference chains
  (Latin / Cyrillic `l I і І`, `H Н`, `n o` / `п о`, `x х`; CJK / Hangul `ㅣ 十 丨`,
  `ㅡ 十 一`, `口 ㅁ`; OS/2 `sxHeight` / `sCapHeight` when the height glyphs are missing,
  `italicAngle` when the slant probe fails). Stem and width values are averaged only among
  faces measured on the same chain level (Latin or CJK); a reference glyph over the flattening
  budget is an unusable measurement (the chain moves on). Each transform step is skipped below
  its threshold; all skipped = the base outline bit for bit.
- Path order is quality order: exact compatible interpolation, then the matched path (strokes
  move, nothing fades), then the SDF path only for topology mismatches. Measured on two
  unrelated comic fonts, the matched path removed every island / broken-stroke artefact the
  SDF path produced for same-topology letters.
- Known limits: a feature only one design has (a foot on "1", a tail on "l") is interpolated
  to a partial feature; in the SDF path of `Auto` / `Sdf` (different contour counts: single-
  vs double-storey "g", CJK strokes that overlap in one design and not in the other) strokes
  that do not overlap fade instead of moving and can leave small partial features at
  intermediate weights, and corners whose position differs between sources come out slightly
  rounded — `Skeleton` and `Transfer` are the remedies. `Skeleton`: single- vs double-storey
  "g" has no stroke correspondence and falls back (`unmatched`); terminals are modeled as
  perpendicular cuts; small notches can appear at some 口 corners near 75 %. `Transfer`:
  corners round slightly when the thickness changes (distance-field re-trace); the x / cap
  zone map also applies to CJK glyphs; a curve straddling a zone anchor can get a small kink.
- Cost (release, 12 threads, full faces): Source Han Sans K x HanaMinA (49,175 glyphs) `Auto`
  ~15 s, `Transfer` ~18 s, `Skeleton` ~49 s (~3.3x `Auto`, 0.7 % fallbacks); Noto Sans Regular
  x Bold (3,094 glyphs) `Transfer` ~0.5 s, `Skeleton` 0.13 s.
- Kerning (D6): contributors of a pair are the sources having both characters (weights
  renormalized over them); a contributor without the pair adds 0; a pair no contributor has is
  absent; results in font units (= per-mille at 1000 units/em), rounded, zeros dropped.
- No cross-glyph state: every glyph job is a pure function of its sources; output is assembled
  by glyph index. The one font-wide input is the transfer measurement: fixed reference glyphs
  of each face, read once per source before the parallel loop and independent of the compile
  scope (a preview subset equals the full face).
- `sdf.rs` exposes its raster kit crate-wide (`flatten_checked` / `flatten_checked_at`
  (`flatten` is the unbudgeted test reference), `bounds` / `checked_bounds`, `choose_alignment` + `AxisMap::apply`,
  `grid_for`, `signed_distance_field`, `winding_inside`, `marching_squares`, and
  `offset_outline` for the transfer thickness step); other paths reuse it rather than copy it.
- Bounded work on hostile sources: every blend path that flattens (SDF, matched, transfer
  measurement and offset, skeleton) goes through `sdf::flatten_checked` / `flatten_checked_at`
  (`outline::flatten_contour_within`) with a point budget — SDF / matched / transfer
  `sdf::MAX_FLATTENED_POINTS` = 32768 (matched over budget -> no match, so Auto reaches the SDF,
  which rejects it) (4x the largest bundled
  glyph, 8302 points in PlangothicP2), skeleton `MAX_SKELETON_POINTS` = 16384 plus `MAX_SKELETON_CONTOURS` = 512 and
  `MAX_OVERLAP_WORK` = 2^26 (over budget -> `SkeletonFallback::TooComplex`, then SDF). The SDF
  path rejects an aligned glyph wider or taller than `MAX_BLEND_EXTENT_UNITS` (8x the storable
  16-bit range) or with non-finite coordinates (`checked_bounds`); alignment candidates beyond
  it are skipped first; grid node counts are constant-time and the coarsening loop is bounded.
  A rejection of the SDF / transfer path (`BlendRejection::{ExtentTooLarge, TooManyPoints}`)
  is `CompileError::GlyphTooComplex`, which fails the WHOLE compile (no glyph is dropped
  silently).

## Editing map
- Change which characters exist: `plan.rs` (and its tests in `tests.rs`).
- Change which weight applies to which property: `HybridParams::effective_weights`
  (`../recipe.rs`); the dispatch per algorithm: `mod.rs::blend_glyph`.
- Skeleton blend: `skeleton/` (its own editing map); parameter transfer: measurement chains,
  thresholds and transform steps in `transfer.rs`.
- Improve exactness for same-structure glyphs: `compatible.rs`; same-topology glyphs:
  `matched.rs`.
- Blend quality / speed: `sdf.rs` (grid, alignment), `trace.rs` (fitting); measure with
  `examples/hybrid_gallery.rs` and bump `ENGINE_VERSION`.
- Inherited kerning rules: `kerning.rs`; built-in pair reading lives in `../kerning.rs`.
