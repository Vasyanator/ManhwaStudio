# Module: src/tabs/typing/render_next/formula

## Purpose
This directory implements formula-driven and custom-line text layout for the production
typing renderer. It turns parsed formula parameters, raster line paths, or vector line
paths into glyph placement on curves before delegating glyph sampling to the shared
raster helpers.

## Architecture
`mod.rs` re-exports the formula render boundary used by `pipeline.rs`. Formula rendering
is split into three responsibilities:

1. `parser.rs` tokenizes and parses ASCII math expressions into a small AST.
2. `eval.rs` compiles `TextFormulaLayoutParams` expressions and evaluates finite
   transforms or arc-length samples for runtime glyph variables.
3. `render.rs` shapes text, builds glyph seeds, maps glyphs to formula/custom-line
   positions, draws rotated glyphs, and reports when shape mode should fall back to the
   standard text path.

Custom raster-line and vector-line modes share this rendering path because they also
place glyphs by distance along a curve. `drawn_lines.rs` lives one level up and supplies
the line paths.

## Files and submodules
- `mod.rs`: private module wiring, smoke contract, and re-exports for renderer callers.
- `parser.rs`: tokenizer, AST types, recursive-descent parser, operator precedence, and
  function-call parsing for formula expressions.
- `eval.rs`: compiled formula bundle, runtime variable lookup, finite-value checks,
  transform evaluation, tangent rotation, and arc-length table generation.
- `render.rs`: formula/custom-line render requests, glyph seed collection, advance
  assignment, line-path mapping, glyph bounds, and fallback decisions. Its composite
  pass rasterizes each glyph's true font outline (`render_next/vector.rs`) via
  `glyph_outline_transform` + `rasterize_outline_into`; color glyphs (no monochrome
  outline) keep the legacy rotated bitmap blit. For `CustomVectorLines` lines set to
  `MinimumPreviousDistance`, it derives each glyph's ink contour from that outline
  (`vector::glyph_contour_from_outline` -> `render_next/glyph_contour.rs`, cached by
  cosmic-text `CacheKey`) and searches the arc-length position so the true ink-to-ink
  gap to the previous glyph reaches a kerning-driven target, instead of center-to-center
  distance.

## Contracts and invariants
- Formula input comes from `TextFormulaLayoutParams`; do not read panel state,
  `text_info.json`, project files, or GUI state in this module.
- Expressions are ASCII math with explicit variables/functions. Parser and evaluator
  errors must identify the failing field, token, variable, or function.
- All evaluated coordinates, rotations, and arc lengths must be finite. NaN or infinity
  is an error, not a clamped value.
- `t_start`, `t_end`, scale, offsets, user vars, glyph index variables, line variables,
  width, and font size are separate runtime inputs. Keep their meanings explicit.
- Formula and custom-line rendering must preserve inline style, inline font, kerning,
  glyph scale, glyph offset, and text color overrides supplied by the main pipeline.
- `FormulaRenderOutcome::FallbackToStandard` is an explicit layout decision for modes
  that cannot use a curve safely. Do not silently render a different mode.
- Rotated raster output must keep `RenderedTextImage.rgba` in unmultiplied RGBA order
  with a valid `width * height * 4` buffer.
- `TextRenderParams.raster_transform` (vector mesh warp) IS honored on BOTH functions
  here (`render_text_with_formula_layout_once` for Formula/Shape,
  `render_text_with_drawn_lines_layout_once` for custom raster/vector lines). Each
  captures a `warp_pre` (pre-global-rotation content box + centroid = mean of the
  drawable placement centers, gated on `raster_transform.is_some()`) BEFORE
  `rotate_placements_about_centroid` mutates the transforms, builds `MeshWarpContext`
  with that box/centroid + `global_rotation_rad`, passes `Some(&ctx)` at the outline
  seam, and grows bounds via `for_each_warped_bound_point`. For custom VECTOR lines a
  non-identity warp DROPS the fixed output canvas (like a global rotation) and grows to
  the warped bounds so nothing clips; its normalization box is the fixed canvas dims
  when honored, else the content bounds. `None`/identity is byte-identical; the
  color-glyph bitmap fallback does not warp.
- `TextRenderParams.extra_info` (optional mean/median centers) IS wired on BOTH
  render functions (all four modes: Formula, Shape, CustomRasterLines,
  CustomVectorLines). Each `_once` body builds its OWN `ExtraInfoAccumulator`, so the
  formula retry loop (`render_margin_pad` growth) naturally rebuilds a FRESH
  accumulator per iteration and the stored extras match the accepted image. The
  composite pass feeds it the final line-placed, rotated glyph box (the shared
  `extra_info::rotated_box_samples` at `dst_center`/`placed_center` with the
  transform's total `rotation_rad`) for BOTH the outline and bitmap-fallback glyphs,
  applies the mesh warp once via `map_points`, then `finish(x_offset, y_offset)`
  stores the centers into the returned image BEFORE the caller's trim/effects (both
  self-correct the centers). These paths never read `hanging_punctuation`. The
  default (no request) is a byte-identical no-op.
- The on-path glyph transform has a single source of truth (`drawn_line_transform_at` +
  `drawn_line_glyph_destination_center_raw`). The outline rasterizer, the ink-distance
  search, and `placed_contour_for_transform` all build the outline->world placement with
  the same `glyph_outline_transform` pivot, so a measured contour lands on exactly the
  pixels the glyph is rasterized to (zero shift versus the old bitmap placement).
- Perpendicular line placement (`TextRenderParams.line_placement_percent`) is applied by
  the shared `apply_line_placement` helper. For the drawn/vector-line path it is folded
  INTO `drawn_line_glyph_destination_center_raw`, which now places the glyph INK CENTER on
  the line at 0% (deliberate change from the old baseline-on-line placement, so both line
  modes share 0 = center) and then shifts by `line_frac * scaled_ink_height / 2` toward
  the top side. For the formula path the curve point already IS the ink center, so the
  same helper shifts `transform.center` in all three spots (bounds, outline draw, bitmap
  fallback). The effective `line_frac` is threaded in from the pipeline router
  (`FormulaRenderRequest.line_placement_frac` -> `CustomLineLayoutSettings`), gated to
  `0.0` for the HIDE siblings `Shape` and `CustomRasterLines`.
- Line placement REFERENCE (`TextRenderParams.line_placement_reference`, `CustomVectorLines`
  only, threaded via `CustomLineLayoutSettings.line_placement_reference` + `ascent_scaled`):
  `GlyphHeight` keeps the per-glyph ink-center anchoring above; `LineBox` anchors every glyph
  to one SHARED baseline (offset by the glyph's own scaled top bearing) and shifts the whole
  band by `line_frac * ascent_scaled / 2`, so glyphs no longer float by their own height. The
  shared `ascent_scaled` is the primary font ascent (first seed's font at `font_size_px`) ×
  base vertical stretch, computed once in `render_text_with_drawn_lines_layout_once`. All three
  `drawn_line_glyph_destination_center_raw` sites (bounds, draw, ink-distance contour) pass the
  same reference/ascent so the measured contour matches the drawn ink.

## Editing map
- To add formula syntax, edit `parser.rs`, then update `eval.rs` if the new syntax
  needs evaluation support and add parser/evaluator tests.
- To add variables or functions, update `eval.rs` and make error messages name unknown
  identifiers clearly.
- To change curve sampling, tangent rotation, or finite checks, edit `eval.rs` and
  verify formula render callers still receive useful errors.
- To change glyph placement along formula, raster-line, or vector-line paths, edit
  `render.rs`.
- To change the public formula parameter contract, start in `render_next/types.rs`,
  then update this module, the smoke anchor in `mod.rs`, and typing serialization.
