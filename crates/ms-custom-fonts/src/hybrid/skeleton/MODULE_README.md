# Module: crates/ms-custom-fonts/src/hybrid/skeleton

## Purpose
Outline blend of the `Skeleton` algorithm: per glyph, reduce every source to its strokes
(medial axis + radius profile), pair strokes across the designs, blend the pairs and rebuild an
outline from the blended strokes. Strokes one design lacks grow or shrink from where they
attach instead of fading (the distance-field path's limit). Called only from
`hybrid::blend_glyph`; any `SkeletonFallback` makes that glyph take `sdf::blend` and is counted.

## Architecture
```text
blend (mod.rs)
  sdf::choose_alignment + flatten, one grid (SKELETON_CELL_UNITS) over the aligned union
  per source  graph::extract
                winding mask -> thin.rs (Zhang–Suen, staircase cleanup, vanished dots)
                -> pixel graph (node clusters, chains, junction-free cycles)
                -> prune spurs by reconstruction gain -> decorations (short spurs)
                -> corner split -> strokes: resample, smooth, ridge-centre, exact radii,
                   free-end extension + Cap (round .. square, per side)
  harmonize.rs  split strokes where the other design has a corner node
  correspond.rs reference (highest shape weight) vs each other source: greedy one-to-one
                pairs by sample distance; Unmatched when < MIN_MATCHED_FRACTION of length
  place.rs      matched groups: weighted mean (positions, radii, caps), junctions welded;
                single-holder strokes: similarity / grow from attachment / scale in place by
                weight, radius x min(1, 2w); decorations relative to their blended anchor
  rebuild.rs    tapered-capsule union field + cap corner quads -> sdf::marching_squares
                -> trace::fit_loops
```

## Files and submodules
- `mod.rs`: entry `blend`, `SkeletonFallback` (reason keys), the shared constants.
- `geometry.rs`: `Stroke` / `Cap` / `Sampler`; `SegmentIndex` (exact distance to the boundary
  of the contours' nonzero union, bucket grid, bounded); index/count conversions.
- `thin.rs`: raster thinning and the 8-neighbourhood addressing.
- `graph.rs`: one source glyph -> `Skeleton` (`Major` strokes, `Decoration`s).
- `harmonize.rs`: makes corner splits agree between the reference and the other sources.
- `correspond.rs`: `Correspondence` / `Pairing` / `ParamMap` (direction, cyclic phase).
- `place.rs`: blending and placement of every stroke.
- `rebuild.rs`: strokes -> outline (`capsule_distance` is the closed-form disc-union distance).
- `tests.rs`: synthetic and bundled-font tests (`fonts/ui`, skip when absent).

## Contracts and invariants
- Pure per glyph and deterministic: fixed scan orders, sorted greedy assignment with total
  tie-breaks, sources summed in recipe order, no hashing, no cross-glyph state.
- Bounded on hostile input: flattened points (`MAX_SKELETON_POINTS`, enforced while
  flattening through `sdf::flatten_checked`) and contours (`MAX_SKELETON_CONTOURS`) are checked
  before any raster work; the overlap classification has a work bound (`MAX_OVERLAP_WORK`);
  grid via `sdf::grid_for` (`MAX_GRID_NODES`), segment index entries
  (`MAX_INDEX_ENTRIES`), raw graph edges (`8 x MAX_STROKES`), strokes per source
  (`MAX_STROKES`), samples per stroke (`MAX_EDGE_SAMPLES`), reconstruction segments
  (`MAX_TOTAL_SEGMENTS`), radii (`MAX_RADIUS_UNITS`), every walk / merge / march / search loop
  by a fixed count. Non-finite or oversized glyphs are `Extent`; nothing panics.
- Fallback reasons (stats keys): `too_complex` (points, contours, index or overlap work over
  their bound), `too_many_strokes` (also a raw graph or reconstruction over its bound),
  `no_skeleton`, `unmatched`, `extent`. The distance-field fallback flattens with its own
  budget (`sdf::MAX_FLATTENED_POINTS`), so it cannot redo unbounded work either.
- Constants here are `Skeleton` output semantics only: changing one bumps `ENGINE_VERSION`
  but can never change `Auto` / `Sdf` / `NoBlend` output.
- Output contours follow the crate orientation (outer clockwise), like `sdf.rs`.
- Reuses `sdf.rs` (alignment, grid, winding mask, marching squares) and `trace.rs`; never
  copies them.

## Known limits
- Topologies without a stroke correspondence (single- vs double-storey "g") fail `unmatched`
  and take the distance field.
- Terminal caps model a cut perpendicular to the stroke; a slanted cut (Noto's horizontal
  feet on diagonals) becomes a partly square, partly round end.
- Measured (2026-10, 12 threads, release): Source Han Sans K x HanaMinA full face 49.4 s
  (Auto 15.2 s), 215 of 31 460 blended glyphs fall back (0.7 %); Noto Sans x Source Han
  4 of 462; Noto Regular x Bold 0 of 15.

## Editing map
- Thinning artefacts: `thin.rs`. Graph shape (pruning, decorations, corners, caps): `graph.rs`.
- Wrong pairs or too many `unmatched`: `correspond.rs` (costs, gates) and `harmonize.rs`.
- Blend rules (weights, unmatched growth, welding): `place.rs`.
- Outline smoothness, terminals: `rebuild.rs` (and `trace.rs`, shared with the SDF path).
