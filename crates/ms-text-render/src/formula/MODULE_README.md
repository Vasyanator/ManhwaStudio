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
- USER-AUTHORED KERNING PAIRS are applied here exactly as on the horizontal path (see
  the CUSTOM KERNING contract in `../MODULE_README.md`): a matching pair REPLACES the
  font's own value under every `KerningMode`, stepping by
  `nominal_glyph_advance_px(left)` plus the authored delta
  (`assign_formula_seed_advances`). Because a `FormulaGlyphSeed` is DETACHED from the
  `LayoutRun` it came from, the source character is captured at seed time as
  `FormulaGlyphSeed::cluster_char` (`None` for a multi-char cluster and for the
  synthesized wrap hyphen, both of which can never carry an authored pair). One thing
  does NOT carry over: `FormulaGlyphSeed::advance_px` is a MAGNITUDE along the drawn
  line (floored positive at every consumer), not a signed x step, so the RTL sign
  mirroring `pipeline::custom_pair_step_px` performs has no counterpart here. Formula
  and drawn lines walk seeds in logical order regardless of script direction — a
  limitation of that whole path, not of the overrides.
- `FormulaRenderOutcome::FallbackToStandard` is an explicit layout decision for modes
  that cannot use a curve safely. Do not silently render a different mode.
- Line stacking is NOT re-implemented here. `pipeline::line_baseline_advance_table`,
  `compute_horizontal_line_baselines` and `horizontal_run_baseline_y` are imported, so
  the inline line-spacing and grow-only `<stretching>` rules are identical to the
  horizontal path — upward-only, real-ink, max-not-sum (see the parent
  `MODULE_README.md`). This module used to carry
  verbatim copies of all three; never re-fork them.
  `default_extra_line_spacing_px` is threaded down to
  `render_text_with_formula_layout_once` / `detect_shape_layout_fallback_reason` as
  an explicit parameter. Do not re-derive it as `line_extra_spacing_table.first()`:
  entry 0 can carry the grow-only inline height room, which would then be
  subtracted from every later gap in the `has_inline_size_overrides` branch.
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
  self-correct the centers). These paths never HANG punctuation visually — their line
  origin uses the raw `run.line_w` — but `collect_formula_glyph_seeds` still marks the
  line's leading/trailing hanging runs `hanging_excluded` once the hanging STRENGTH
  reaches `HANGING_EXTRA_INFO_THRESHOLD`, so the reported centers agree with the
  horizontal paths on what hangs. The default (no request) is a byte-identical no-op.
- Horizontal alignment (`TextRenderParams.align`) positions the run ALONG the path on
  both on-path layouts, via the shared `on_path_align_fraction`. `justify` never reads
  `bias` (the slider is hidden in the UI while justify is on) and instead keeps each
  path's historical default: `0.0` (start of line) for custom lines, `0.5` (centered)
  for formula/shape.
  - Custom raster/vector lines: each line's arc-length cursor starts at
    `drawn_line_start_offset(path.total_len_px, run_len_px, line_align)`, using that
    line's own align so inline `<align=...>` overrides keep working. The free space is
    NOT clamped to `>= 0`, so an overlong run gets a negative start and is clipped at
    the line START instead of its end.
  - `drawn_line_drop_side` is the single decision point for out-of-range glyphs
    (`sample_drawn_line_path` clamps, which would pile them onto the first/last point).
    Past the path end is always a drop. Before the path start is a drop ONLY when the
    line's start offset is negative, i.e. alignment-induced overflow; a negative position
    coming from a plain inline `<offset>` nudge still clamps and DRAWS, as it always did.
    Both drops feed the same skipped-glyph warning; the before-start drop still advances
    the line cursor, the past-end drop deliberately does not.
  - `run_len_px` is the line's FINAL CURSOR from replaying the `ByLineLength` recurrence
    (`drawn_line_center_s`/`drawn_line_next_cursor`): the advance sum plus the
    `shift_following` bumps, floored at `0.0`. A plain inline `line_px` offset moves its
    own glyph only and is deliberately NOT counted, so one nudged glyph never drags the
    line's alignment — at the price that a glyph nudged past the line end is invisible to
    the alignment and can still be clipped. EXACT for `ByLineLength`. For
    `MinimumPreviousDistance` it is a lower bound whose error ACCUMULATES and is unbounded
    in principle: every forward push of the ink search shifts all following glyphs, so an
    end-aligned ink-spaced run can lose a clipped suffix past its line end.
  - Formula/shape: the arc-length accumulator is one continuous run for the whole text,
    so only the block-level `params.align` applies (no per-line override). It splits the
    free curve length in `map_formula_target_arc_length`. On OVERFLOW that function keeps
    compressing the run onto the curve and ignores alignment — there is no free space to
    distribute and `detect_shape_layout_fallback_reason` is defined against exactly that
    compression ratio.
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
