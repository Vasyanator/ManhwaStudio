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
  cosmic-text `CacheKey`) and moves the glyph along the path until the pair reproduces
  the gap it would have in a STRAIGHT line — see the mode's contract below.

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
- THE ARC-LENGTH STEP BETWEEN TWO ADJACENT GLYPHS IS DECIDED IN ONE PLACE AND ONLY
  ONE: `OnPathStepSpacing::step_px` in `render.rs`. All three consumers go through it —
  the alignment pre-pass (`drawn_line_start_offsets`), the custom-line walk
  (`drawn_line_seed_transform`) and the formula/shape accumulator
  (`render_text_with_formula_layout_once`). Each of them used to hold a verbatim copy
  of the expression and the copies had already drifted; do not re-fork them. The
  pre-pass in particular REPLAYS the walk, so a second copy misaligns every line
  silently. Build the value through `OnPathStepSpacing::for_custom_lines` /
  `for_formula` (they clamp the letter-spacing inputs), never as a struct literal.
  - Two floors with different jobs: the SEED floor guards the glyph's own advance
    before the letter-spacing multiplier scales it; `MIN_ON_PATH_STEP_PX` guards the
    final step so negative additive tracking cannot stall or reverse the cursor.
  - KNOWN DEFECT, preserved on purpose: the seed floor differs per path — `1.0` for
    custom raster/vector lines, `(font_size_px * 0.5).max(1.0)` for formula/shape. On a
    formula curve that spaces every narrow glyph (`i`, `l`, `.`) out to half an em and
    stops an authored negative kerning pair from tightening a step at all. Lowering it
    is a rendering change and must be done together with
    `detect_shape_layout_fallback_reason`, which estimates run length with the same
    floor and whose compression ratio decides the shape fallback.
  - `seed_metric_advance_px` is a SEPARATE, earlier floor
    (`max(raw_delta, glyph_w * 0.25, 1.0)`) applied in `assign_formula_seed_advances`.
    It turns the shaped pen delta into a positive magnitude, guarding against a
    non-monotonic (RTL) run and against font pair kerning below `-75%` of the left
    advance. It guards the METRIC advance only, so it does NOT clip user-authored
    kerning: an authored pair steps by the glyph's nominal advance plus the delta and
    can leave `FormulaGlyphSeed::advance_px` negative on purpose — clamping that is
    `OnPathStepSpacing::step_px`'s job alone.
- `MinimumPreviousDistance` (per `CustomVectorLine`) GIVES EVERY ADJACENT PAIR OF A LINE
  THE SAME INK-TO-INK DISTANCE. It is optical kerning on the glyph contours, applied
  along the curve: the run is spaced by what the ink actually does, not by the font's
  side bearings. That is a deliberate, user-requested trade — the font's side bearings
  are partly overruled and the same text set on a curve does NOT match the same text set
  in a straight line. The whole mode is six decisions, all in `render.rs`:
  - REFERENCE. Every placed glyph is ALSO placed in a virtual straight layout
    (`straight_reference_transform`: the path point `(x, 0)` with the constant tangent
    `(1, 0)`, walked by the NOMINAL along-path step read off the arc-length seed, so
    advances, letter spacing, authored kerning and inline offsets enter it without a
    second copy of the cursor arithmetic). It runs through the same
    `on_path_transform_from_sample` as the real placement, so normal offset, flip,
    static vs. tangent rotation and per-glyph rotation/offset all apply identically.
  - MEASURE. A pair's gap is `on_path_pair_gap`: `pair_gap::directional_pair_gap` on the
    CHORD between the two glyphs' ink placement centers (`pair_chord_axis`). The chord,
    not either tangent, so the measure is symmetric under swapping the pair; on a
    circular arc it is exactly the bisector of the two tangents and, unlike the
    bisector, it does not degenerate at large turn angles. Coincident centers yield
    `None` ("no advance direction"), treated as unmeasurably close.
  - TARGET. ONE number per LINE, not per pair: the MEDIAN of that line's
    reference-layout gaps (`straight_reference_target_gaps`, using the shared
    `optical::median_of_gaps`). Taken in the REFERENCE, never on the curve, so dragging a
    path node cannot re-space the rest of the line; it is also the same statistic
    `KerningMode::Optical` normalizes on. The per-pair target is that median passed
    through `optical::optical_delta`, so the +/- font-size sanity bound and the
    anti-collision floor are literally the horizontal/vertical optical ones. Nothing
    about side bearings, bounding boxes or advances is modelled.
  - WHICH PAIRS ARE NORMALIZED. Both glyphs must have ink: an inkless glyph (a space)
    ends the pair chain, so an inter-word distance neither enters the median nor gets
    normalized, and words cannot merge. A pair carrying a USER-AUTHORED kerning override
    (`custom_seed_pair_delta_px`) is EXEMPT — it keeps the distance its author gave it
    and is excluded from the median, mirroring `pipeline.rs`, where an authored pair
    cancels optical kerning outright. A pair whose inks do not face each other even when
    straight has nothing to normalize and keeps its arc-length seed.
  - WHAT IS NOT AN INVARIANT ANY MORE: "on a straight line the mode equals
    `ByLineLength`". It was, while the target was the pair's own reference gap; a uniform
    target necessarily re-spaces a straight run of mixed side bearings. What still holds
    is the degenerate case: when every pair of the line already has the same gap, the
    median IS that gap and the arc-length seeds stand — at any angle, because
    `directional_pair_gap` derives its band, sample count and sample offsets from dot
    products with the frame, so a rotation cancels exactly.
  - SEARCH. `solve_directional_gap_center_s` moves the glyph in EITHER direction (a bend
    tightens a pair on its concave side and loosens it on the convex side; a
    forward-only search leaves the second case visibly loose), bounded by the glyph's own
    advance and by the line's run start, so a backward move can never change what
    `drawn_line_drop_side` decides. The gap is NOT monotone in the arc position — a path
    is a polyline, so a glyph crossing a node changes rotation in a step — so the search
    does not bisect the whole range: the sign of the seed's error picks a direction, a
    coarse scan walks outward and stops at the FIRST sign change, and only that bracket
    is bisected. The result is the crossing NEAREST the seed, i.e. the minimal
    correction, and a distant non-monotone region cannot pull the glyph across it.
  - SAFETY NET. After the directional pass, `find_clearance_center_s` pushes forward
    until the glyph keeps an omnidirectional `min_placed_distance` floor from the last
    `ON_PATH_INK_NEIGHBOR_WINDOW` (4) INKED glyphs — not just its predecessor, because a
    sharp turn brings older ones within reach. Two caps make this harmless: the floor is
    `ON_PATH_INK_CLEARANCE_FLOOR_PX` (0.5) capped by what that pair achieves in the
    REFERENCE layout (`pair_clearance_floor`), so a pair the font or an authored negative
    kerning pair draws tighter than that keeps its own tightness; and it is a clearance,
    never a target. It exists because the directional metric compares the two glyphs
    within one scanline only and is therefore blind to an overhang reaching a neighbour
    on scanlines that neighbour has no ink on (a `Г` arm over the next letter's
    shoulder).
  - A glyph with no ink (a space) gets no directional target and does not enter the
    window, but does NOT clear it: the first glyph of the next word must still not
    collide with the last word on a sharp turn. The reference walk
    (`StraightReferenceWalk`, the single owner of that recurrence, shared by the median
    pre-pass and the real walk) follows every placed glyph, inkless ones included, so the
    two frames never drift apart.
  - There is no bitmap-measured ink extent any more. The target comes off the outline
    contour, so `SeedInkGeometry` carries the contour and the bitmap PLACEMENT only.
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
    the alignment and can still be clipped. EXACT for `ByLineLength`, and final there.
  - `MinimumPreviousDistance` cannot be predicted by that replay — each glyph's ink
    correction shifts all following ones, and on a path that bends the same way
    throughout the corrections do not cancel, so the run really is longer than the
    replay says. The replayed value is therefore only a STARTING ESTIMATE, and
    `refine_min_distance_start_offsets` replaces it: it re-walks the line
    (`measure_min_distance_run_len_px`, the real placement walk with every correction),
    recomputes the offset from the MEASURED run length through the same
    `drawn_line_start_offset`, and iterates until the offset settles (at most
    `ON_PATH_ALIGN_REFINE_PASSES`, tolerance `ON_PATH_ALIGN_REFINE_TOLERANCE_PX`).
    The measurement walk sets `DrawnLinePlacementState::measure_only`, which makes a
    past-the-end glyph advance the cursor by its nominal step instead of freezing it —
    otherwise an overflowing run could never measure longer than its line and the
    offset could never go negative. Only lines that are BOTH in this mode AND
    alignment-sensitive (`on_path_align_fraction != 0.0`) pay for it; a start-aligned
    line, which is the default for custom lines and for justify, is skipped entirely.
    Without this refinement an end-aligned run on a one-sided bend lost its last glyph.
  - Formula/shape: the arc-length accumulator is one continuous run for the whole text,
    so only the block-level `params.align` applies (no per-line override). It splits the
    free curve length in `map_formula_target_arc_length`. On OVERFLOW that function keeps
    compressing the run onto the curve and ignores alignment — there is no free space to
    distribute and `detect_shape_layout_fallback_reason` is defined against exactly that
    compression ratio.
- The on-path glyph transform has a single source of truth
  (`on_path_transform_from_sample`, reached through `drawn_line_transform_at` for a real
  path sample and through `straight_reference_transform` for the straight reference
  layout, plus `drawn_line_glyph_destination_center_raw`). The outline rasterizer, the
  ink search, and `placed_contour_for_transform` all build the outline->world placement
  with the same `glyph_outline_transform` pivot, so a measured contour lands on exactly
  the pixels the glyph is rasterized to (zero shift versus the old bitmap placement).
  `place_seed_ink_for_transform` is the one place that turns a `SeedInkGeometry` plus a
  transform into a placed ink (contour + placement anchor); candidate placements,
  reference placements and the stored final placement all go through it.
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
- To change how far apart two adjacent glyphs sit along a path (letter spacing, the
  per-path floors), edit `OnPathStepSpacing` in `render.rs` and nothing else — it is the
  sole owner of the NOMINAL step.
- To change uniform ink spacing (`MinimumPreviousDistance`), edit the pieces named in
  its contract above: the reference (`straight_reference_transform` /
  `StraightReferenceWalk`), the target (`straight_reference_target_gaps`, whose
  statistics and clamps belong to `optical.rs`), the measure
  (`pair_chord_axis` / `on_path_pair_gap`), the search
  (`solve_directional_gap_center_s`) or the safety net (`pair_clearance_floor` /
  `find_clearance_center_s`). The gap MEASUREMENT itself belongs to `pair_gap.rs`; do
  not grow a second one here.
- To change where a line's run STARTS on its path (alignment), edit
  `drawn_line_start_offset` / `drawn_line_start_offsets` for the arc-length estimate and
  `refine_min_distance_start_offsets` / `measure_min_distance_run_len_px` for the
  `MinimumPreviousDistance` measurement that replaces it. The per-line cursor recurrence
  itself is owned by `LineArcCursor`; the three walks that replay it (alignment pre-pass,
  median pre-pass, real placement) must all go through that type.
- To change the public formula parameter contract, start in `render_next/types.rs`,
  then update this module, the smoke anchor in `mod.rs`, and typing serialization.
