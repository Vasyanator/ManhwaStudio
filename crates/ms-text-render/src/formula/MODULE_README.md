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
