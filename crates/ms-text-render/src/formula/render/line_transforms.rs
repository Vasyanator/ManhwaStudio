/*
File: crates/ms-text-render/src/formula/render/line_transforms.rs

Purpose:
Per-glyph transforms of one custom line: the straight reference walk and its median
target gaps (MinimumPreviousDistance), the drop side past the path ends, the line walk
itself (`drawn_line_seed_transform`) and the measured-run refinement of aligned
MinimumPreviousDistance lines.

Key functions:
- build_drawn_line_transforms(), drawn_line_seed_transform()
- straight_reference_target_gaps(), `StraightReferenceWalk` (impl)
- drawn_line_drop_side(), refine_min_distance_start_offsets(),
  measure_min_distance_run_len_px(), `DrawnLinePlacementState` (impl)
*/

use crate::drawn_lines::DrawnLinePath;
use crate::font_registry::CustomKerningMap;
use crate::optical::{median_of_gaps, optical_delta};
use crate::pair_advance::PairAdvanceModel;
use crate::types::{TextRenderParams, TextVectorLineDistanceMode};
use cosmic_text::FontSystem;
use std::collections::HashMap;
use super::{
    CustomLineLayoutSettings, DrawnLineDropSide, DrawnLinePlacementCtx, DrawnLinePlacementState,
    DrawnLineTransform, FormulaGlyphSeed, LineArcCursor, ON_PATH_ALIGN_REFINE_PASSES,
    ON_PATH_ALIGN_REFINE_TOLERANCE_PX, ON_PATH_INK_NEIGHBOR_WINDOW, ON_PATH_INK_SEARCH_MAX_RANGE_PX,
    ON_PATH_INK_SEARCH_MIN_RANGE_PX, OnPathInkCaches, OnPathStepSpacing, PlacedNeighbor,
    StraightReferenceGaps, StraightReferenceWalk,
};
use super::drawn_lines_layout::{
    drawn_line_aligns, drawn_line_start_offset, drawn_line_start_offsets, on_path_align_fraction,
};
use super::seed_ink::{
    drawn_line_transform_at, on_path_pair_gap, pair_clearance_floor, place_seed_ink_at,
    place_seed_ink_for_transform, place_seed_ink_straight, seed_ink_geometry,
};
use super::path_geometry::{
    apply_drawn_line_group_rotations, find_clearance_center_s, solve_directional_gap_center_s,
    vector_line_distance_mode,
};

impl StraightReferenceWalk {
    /// Reference coordinate of a glyph whose arc-length SEED is `center_s_seed`.
    ///
    /// Pure: the walk only moves on [`Self::commit`], so a caller may ask for a
    /// candidate's reference coordinate before deciding to place it.
    #[must_use]
    pub(super) fn reference_x(&self, center_s_seed: f32) -> f32 {
        self.x_px + self.previous_center_s.map_or(0.0, |prev| center_s_seed - prev)
    }

    /// Record a placed glyph: its reference coordinate `x_px` (from
    /// [`Self::reference_x`]) and its FINAL arc-length center.
    pub(super) fn commit(&mut self, x_px: f32, final_center_s: f32) {
        self.x_px = x_px;
        self.previous_center_s = Some(final_center_s);
    }
}

/// Target ink gap of every line that uses `MinimumPreviousDistance`: the MEDIAN
/// straight-reference gap of that line's kernable pairs, keyed by `line_idx`.
///
/// This is the contract of the mode (see the file header): every adjacent pair
/// of a line is re-spaced to ONE ink-to-ink distance, so the run reads evenly
/// along the curve. The median is taken in the STRAIGHT REFERENCE, never on the
/// curve, so dragging a path node cannot re-space the whole line — and it is the
/// same statistic `KerningMode::Optical` normalizes on, via the shared
/// [`median_of_gaps`].
///
/// Which pairs contribute:
/// - both glyphs must have ink — an inkless glyph (a space) ends the pair chain,
///   so inter-word distance is never normalized and words cannot merge;
/// - the pair must not carry a USER-AUTHORED kerning override
///   ([`FormulaGlyphSeed::pair_glyph`] through `PairAdvanceModel::is_overridden`): an authored pair keeps the spacing its
///   author gave it and must not drag the target either;
/// - non-finite gaps (inks that do not face each other at all) are dropped by
///   [`median_of_gaps`] itself.
///
/// A line with no contributing pair is absent from the map and is left on its
/// plain arc-length walk. Lines on any other distance mode are never even
/// measured, so `ByLineLength` (and every non-vector layout mode) pays nothing
/// here — not even glyph-ink extraction.
///
/// The replay must use the SAME `spacing` as the real walk, for the same reason
/// [`drawn_line_start_offsets`] must.
pub(super) fn straight_reference_target_gaps(
    params: &TextRenderParams,
    seeds: &[FormulaGlyphSeed],
    layout: &CustomLineLayoutSettings,
    spacing: OnPathStepSpacing,
    custom_kerning: &CustomKerningMap,
    font_system: &mut FontSystem,
    caches: &mut OnPathInkCaches,
) -> HashMap<usize, f32> {
    let mut lines = HashMap::<usize, StraightReferenceGaps>::new();
    for (seed_idx, seed) in seeds.iter().enumerate() {
        if vector_line_distance_mode(params, seed.line_idx)
            != TextVectorLineDistanceMode::MinimumPreviousDistance
        {
            continue;
        }
        let advance = spacing.step_px(seed.advance_px);
        let line = lines.entry(seed.line_idx).or_default();
        // Uncorrected replay: in the reference there is nothing to correct, so
        // the seed center IS the final center.
        let center_s = line.cursor.seed_center_s(seed, advance);
        let x = line.walk.reference_x(center_s);
        line.walk.commit(x, center_s);
        line.cursor.commit(seeds, seed, advance, center_s);

        let Some(geom) = seed_ink_geometry(seed, font_system, caches)
        else {
            // Inkless glyph (a space): it ends the pair chain, so no inter-word
            // distance ever enters the target.
            line.previous = None;
            continue;
        };
        let ink = place_seed_ink_straight(params, seed, &geom, layout, x);
        if let Some((prev_idx, prev_ink)) = line.previous.as_ref()
            // An authored pair is exempt from the normalization, so it must not
            // influence the target the rest of the line is normalized to either.
            && !PairAdvanceModel::new(custom_kerning)
                .is_overridden(seeds[*prev_idx].pair_glyph(), seed.pair_glyph())
            && let Some(gap) = on_path_pair_gap(prev_ink, &ink)
        {
            line.gaps.push(gap);
        }
        line.previous = Some((seed_idx, ink));
    }

    lines
        .into_iter()
        .filter_map(|(line_idx, line)| Some((line_idx, median_of_gaps(line.gaps.as_slice())?)))
        .collect()
}

/// Decide whether a glyph centered at `center_s_px` must be dropped, and on
/// which side of its line path.
///
/// `sample_drawn_line_path` CLAMPS outside `[0, total_len_px]`, so an
/// out-of-range glyph would be piled onto the first or last path point instead
/// of being placed. Past the end that is always wrong, so it is always a drop.
///
/// Before the start it is a drop ONLY when the line's own `start_offset_s_px` is
/// negative — which happens exactly when the run overflows its line and an
/// end-biased ALIGNMENT pushed the leading glyphs off, where the clipping is
/// what the user asked for. With a non-negative start offset a negative center
/// can only come from a manual inline `<offset>` nudge on a leading glyph, whose
/// historical (and still expected) behaviour is to clamp onto the first path
/// point and DRAW; dropping it there would silently delete a hand-placed glyph.
pub(super) fn drawn_line_drop_side(
    center_s_px: f32,
    start_offset_s_px: f32,
    total_len_px: f32,
) -> Option<DrawnLineDropSide> {
    if center_s_px > total_len_px {
        return Some(DrawnLineDropSide::PastEnd);
    }
    if center_s_px < 0.0 && start_offset_s_px < 0.0 {
        return Some(DrawnLineDropSide::BeforeStart);
    }
    None
}

/// Compute the per-glyph on-path transforms for every seed of a custom line
/// layout.
///
/// `layout_settings` is the mode's `custom_line_layout_settings`. `font_system` and
/// `caches` are only touched for lines that use `MinimumPreviousDistance` spacing,
/// which needs the glyph's ink contour (derived from its outline); `ByLineLength`
/// lines never extract here.
/// The returned vector is index-aligned with `seeds`; `None` means the glyph
/// could not be placed (outside its line path or missing sample) and is dropped
/// by callers.
///
/// Each line's cursor starts at [`drawn_line_start_offsets`], so the alignment
/// bias positions the run ALONG its line: bias `< 0` starts at the line start
/// (overflow cut at the end), bias `> 0` ends at the line end (cut at the
/// start), bias `== 0` cuts evenly at both ends. On a `MinimumPreviousDistance`
/// line that offset is then REFINED against the run's real length
/// ([`refine_min_distance_start_offsets`]), because the pre-pass can only
/// estimate it there.
pub(super) fn build_drawn_line_transforms(
    params: &TextRenderParams,
    seeds: &[FormulaGlyphSeed],
    paths: &[Option<DrawnLinePath>],
    custom_kerning: &CustomKerningMap,
    font_system: &mut FontSystem,
    caches: &mut OnPathInkCaches,
    layout_settings: CustomLineLayoutSettings,
) -> Vec<Option<DrawnLineTransform>> {
    let mut line_offsets = HashMap::<usize, DrawnLinePlacementState>::new();
    let spacing = OnPathStepSpacing::for_custom_lines(
        layout_settings.letter_spacing_mul,
        layout_settings.letter_spacing_px,
    );
    // Pre-pass: every line's run length must be known before the first glyph is
    // placed, because the alignment bias decides where that line's cursor starts.
    // It replays the walk below, so it must be given the SAME spacing.
    let mut start_offsets = drawn_line_start_offsets(seeds, paths, spacing);
    // Pre-pass: the ONE ink gap every pair of a `MinimumPreviousDistance` line is
    // normalized to. Must also precede the walk — the first pair already needs
    // the whole line's median. A no-op for every other distance mode.
    let line_target_gaps = straight_reference_target_gaps(
        params,
        seeds,
        &layout_settings,
        spacing,
        custom_kerning,
        font_system,
        caches,
    );
    let mut ctx = DrawnLinePlacementCtx {
        params,
        seeds,
        layout: layout_settings,
        spacing,
        custom_kerning,
        line_target_gaps,
        font_system,
        caches,
    };
    // The estimate above is EXACT for `ByLineLength` and only an estimate for
    // `MinimumPreviousDistance`; replace it there by the measured run length.
    refine_min_distance_start_offsets(&mut ctx, paths, &mut start_offsets);
    let mut transforms: Vec<Option<DrawnLineTransform>> = Vec::with_capacity(seeds.len());
    for (seed_idx, seed) in seeds.iter().enumerate() {
        let Some(path) = paths.get(seed.line_idx).and_then(Option::as_ref) else {
            transforms.push(None);
            continue;
        };
        let state = line_offsets
            .entry(seed.line_idx)
            .or_insert_with(|| {
                DrawnLinePlacementState::starting_at(
                    start_offsets.get(&seed.line_idx).copied().unwrap_or(0.0),
                )
            });
        transforms.push(drawn_line_seed_transform(
            &mut ctx, seed_idx, seed, path, state,
        ));
    }
    apply_drawn_line_group_rotations(seeds, transforms.as_mut_slice());
    transforms
}

/// Replace the ESTIMATED alignment start offset of every alignment-sensitive
/// `MinimumPreviousDistance` line by one derived from its run's REAL length.
///
/// Why it exists: [`drawn_line_start_offsets`] replays the plain arc-length
/// walk, which is exact for `ByLineLength` but only an estimate here — each
/// glyph's ink correction shifts every following one, and on a path that bends
/// the same way throughout those corrections do NOT cancel. The run then ends up
/// longer than predicted, and an end-biased alignment pushed its tail past the
/// path end, where [`drawn_line_drop_side`] deleted the last glyph. Measuring
/// beats padding: no reserve is right for every geometry, and a reserve that is
/// too large mis-aligns every line that does not need it.
///
/// Which lines pay: only those whose distance mode is `MinimumPreviousDistance`
/// AND whose alignment actually depends on the run length. A start-aligned line
/// ([`on_path_align_fraction`] `== 0.0`, the default for custom lines and for
/// justify) starts at `0.0` whatever the run measures, so it is skipped
/// entirely and costs nothing — which is also why the cost of this pass is
/// invisible in the common case.
///
/// How: [`measure_min_distance_run_len_px`] re-walks the line from the current
/// offset and reports where the cursor really ends up; the offset is recomputed
/// from that through the same [`drawn_line_start_offset`] the pre-pass uses, and
/// the two steps repeat until the offset moves by less than
/// [`ON_PATH_ALIGN_REFINE_TOLERANCE_PX`] or
/// [`ON_PATH_ALIGN_REFINE_PASSES`] passes are spent. A run that overflows its
/// line still yields a NEGATIVE offset, i.e. the clipping keeps moving to the
/// side the bias points away from.
///
/// Lines are refined independently and share only content-keyed caches, so the
/// arbitrary iteration order of the align map cannot make the output differ
/// between runs.
fn refine_min_distance_start_offsets(
    ctx: &mut DrawnLinePlacementCtx<'_>,
    paths: &[Option<DrawnLinePath>],
    start_offsets: &mut HashMap<usize, f32>,
) {
    let params = ctx.params;
    let aligns = drawn_line_aligns(ctx.seeds);
    let mut refined = Vec::<(usize, f32)>::new();
    for (&line_idx, &align) in &aligns {
        if vector_line_distance_mode(params, line_idx)
            != TextVectorLineDistanceMode::MinimumPreviousDistance
        {
            continue;
        }
        // A start-aligned run's offset does not depend on its length at all.
        if on_path_align_fraction(align, 0.0) == 0.0 {
            continue;
        }
        let Some(path) = paths.get(line_idx).and_then(Option::as_ref) else {
            continue;
        };
        let mut start_offset = start_offsets.get(&line_idx).copied().unwrap_or(0.0);
        for _ in 0..ON_PATH_ALIGN_REFINE_PASSES {
            let run_len_px = measure_min_distance_run_len_px(ctx, line_idx, path, start_offset);
            let next = drawn_line_start_offset(path.total_len_px, run_len_px, align);
            let settled = (next - start_offset).abs() <= ON_PATH_ALIGN_REFINE_TOLERANCE_PX;
            start_offset = next;
            if settled {
                break;
            }
        }
        refined.push((line_idx, start_offset));
    }
    start_offsets.extend(refined);
}

/// Arc length (px) one `MinimumPreviousDistance` line's run really occupies when
/// walked from `start_offset_s_px`.
///
/// Replays the REAL walk ([`drawn_line_seed_transform`]) for that line's seeds —
/// ink extraction, directional search and clearance net included, so the
/// measured length carries the very corrections the estimate cannot predict —
/// and returns how far the line cursor moved. The replay runs with
/// [`DrawnLinePlacementState::measure_only`] set, so a glyph past the path end
/// still advances the cursor by its nominal step instead of freezing it: an
/// overflowing run must measure LONGER than its line, or the offset it feeds
/// could never become negative and move the clipping to the other end.
///
/// The result is floored at `0.0` for the same reason
/// [`drawn_line_start_offsets`] floors its own: a line whose `shift_following`
/// bumps run backwards past the origin has no negative length.
///
/// Costs one full placement pass over that line. The caches in `ctx` are shared
/// with the real walk, so glyph outlines and contours are extracted once for
/// both.
fn measure_min_distance_run_len_px(
    ctx: &mut DrawnLinePlacementCtx<'_>,
    line_idx: usize,
    path: &DrawnLinePath,
    start_offset_s_px: f32,
) -> f32 {
    // Copied out so the loop's shared borrow of the seeds does not collide with
    // the mutable borrow of `ctx` the placement needs.
    let seeds = ctx.seeds;
    let mut state = DrawnLinePlacementState::starting_at(start_offset_s_px);
    state.measure_only = true;
    for (seed_idx, seed) in seeds.iter().enumerate() {
        if seed.line_idx != line_idx {
            continue;
        }
        // The placement itself is thrown away on purpose: this pass only exists
        // to learn where the line's cursor ENDS UP, and the real walk recomputes
        // every transform from the refined offset anyway.
        drawn_line_seed_transform(ctx, seed_idx, seed, path, &mut state);
    }
    (state.cursor.s_px - start_offset_s_px).max(0.0)
}

/// Place one glyph seed along its line path and advance the line's state.
///
/// For `ByLineLength` spacing this reproduces the original arc-length walk
/// unchanged. For `MinimumPreviousDistance` it seeds the same arc-length
/// position, then (1) moves the glyph either way until the pair's directional
/// chord gap matches the line's ONE target gap
/// ([`straight_reference_target_gaps`]), and (2) pushes it forward if that
/// leaves it inside any windowed neighbour's clearance floor. The target is a
/// property of the LINE, not of the pair, so the mode re-spaces the run even on
/// a straight path — see the file header. Returns the
/// blit-ready transform, or `None` when [`drawn_line_drop_side`] rejects the
/// glyph: past the path end, or before its start when an end-biased alignment
/// gave the line a negative start offset. A negative position caused by a plain
/// inline `<offset>` nudge is NOT a drop — it keeps clamping onto the first path
/// point, as it always did. The before-start drop still advances the line cursor
/// (the following glyphs must keep walking); the past-end drop does not need to,
/// since every later glyph on the line is past the end as well — EXCEPT under
/// [`DrawnLinePlacementState::measure_only`], where the cursor must keep walking
/// through every drop so the run's real length can be measured past the path end.
fn drawn_line_seed_transform(
    ctx: &mut DrawnLinePlacementCtx<'_>,
    seed_idx: usize,
    seed: &FormulaGlyphSeed,
    path: &DrawnLinePath,
    state: &mut DrawnLinePlacementState,
) -> Option<DrawnLineTransform> {
    let params = ctx.params;
    let layout = ctx.layout;
    let all_seeds = ctx.seeds;
    let custom_kerning = ctx.custom_kerning;
    let line_target_gap = ctx.line_target_gaps.get(&seed.line_idx).copied();
    let advance = ctx.spacing.step_px(seed.advance_px);
    // Arc-length seed: identical to ByLineLength placement.
    let mut center_s = state.cursor.seed_center_s(seed, advance);
    match drawn_line_drop_side(center_s, state.start_offset_s_px, path.total_len_px) {
        Some(DrawnLineDropSide::BeforeStart) => {
            // An end-biased alignment pushed this glyph before the line start;
            // it goes into the same skipped-glyph warning as the end drop.
            // The cursor MUST still walk: unlike the end drop (where every later
            // glyph is past the end anyway) a frozen cursor here would collapse
            // the whole line onto one position and drop all of it.
            state.cursor.commit(all_seeds, seed, advance, center_s);
            return None;
        }
        Some(DrawnLineDropSide::PastEnd) => {
            // The cursor is deliberately left frozen: every later glyph on this
            // line is past the end as well, so they all drop anyway. A
            // MEASUREMENT pass is the exception — it exists to learn the run's
            // real length, and a frozen cursor would report the run as ending
            // at the path end however far past it the text actually reaches.
            if state.measure_only {
                state.cursor.commit(all_seeds, seed, advance, center_s);
            }
            return None;
        }
        None => {}
    }

    let use_ink = vector_line_distance_mode(params, seed.line_idx)
        == TextVectorLineDistanceMode::MinimumPreviousDistance;
    // Only the ink-distance mode needs the glyph's rasterized contour.
    let geom = if use_ink {
        seed_ink_geometry(seed, ctx.font_system, ctx.caches)
    } else {
        None
    };

    // Straight reference coordinate of this glyph, from the single owner of that
    // recurrence. `center_s` is still the arc-length SEED here, so this glyph's
    // own correction never feeds back into the reference.
    let straight_x = state.straight.reference_x(center_s);
    let straight_ink = geom
        .as_ref()
        .map(|g| place_seed_ink_straight(params, seed, g, &layout, straight_x));

    if let (Some(g), Some(cur_straight)) = (geom.as_ref(), straight_ink.as_ref())
        && !state.neighbors.is_empty()
    {
        // 1. Give the pair the line's ONE ink gap. The target is the median of
        //    the line's straight-reference gaps, so the run reads evenly along
        //    the curve and the geometry of the curve does not decide the
        //    spacing; measured with the SAME metric on the SAME outlines.
        if state.previous_has_ink
            && let Some(target) = line_target_gap
            && let Some(prev) = state.neighbors.last()
            // A user-authored pair keeps the spacing its author gave it: the
            // override already sits in the arc-length seed, and normalizing on
            // top of it would silently overrule the user.
            && state.previous_seed_idx.is_none_or(|prev_idx| {
                !PairAdvanceModel::new(custom_kerning)
                    .is_overridden(all_seeds[prev_idx].pair_glyph(), seed.pair_glyph())
            })
            && let Some(straight_gap) = on_path_pair_gap(&prev.straight, cur_straight)
            // A pair that does not face itself even when straight (`.` under an
            // apostrophe) has no gap to normalize; the arc-length seed stands.
            && straight_gap.is_finite()
        {
            // Reuse of the optical kerning clamp, so this path cannot drift from
            // the horizontal/vertical ones: the +/- font-size sanity bound on a
            // single pair's correction and the hard anti-collision floor on the
            // resulting gap are `optical_delta`'s, applied to the pair's own
            // straight gap.
            let target_gap =
                straight_gap + optical_delta(straight_gap, target, seed.glyph.font_size);
            let range =
                advance.clamp(ON_PATH_INK_SEARCH_MIN_RANGE_PX, ON_PATH_INK_SEARCH_MAX_RANGE_PX);
            // A backward move may not reach past the line's run start: that
            // would either drop a glyph the alignment meant to keep or pile one
            // onto the first path point. `.min(center_s)` keeps the interval
            // valid when the seed itself already sits before that bound.
            let min_s = (center_s - range)
                .max(state.start_offset_s_px.max(0.0))
                .min(center_s);
            let max_s = (center_s + range).min(path.total_len_px);
            let solved = solve_directional_gap_center_s(
                center_s,
                target_gap,
                min_s,
                max_s,
                &prev.world,
                |s| place_seed_ink_at(params, seed, g, path, s, &layout),
            );
            let Some(solved) = solved else {
                // The path could not be sampled at a trial position, so the
                // glyph is dropped. A measurement pass must still walk its
                // cursor past it; see [`DrawnLinePlacementState::measure_only`].
                if state.measure_only {
                    state.cursor.commit(all_seeds, seed, advance, center_s);
                }
                return None;
            };
            center_s = solved;
        }

        // 2. Safety net: the chord metric cannot see a diagonal approach, and a
        //    sharp turn also brings glyphs older than the predecessor within
        //    reach. Each floor is capped by the straight reference's own
        //    clearance, so an authored overlap is never undone.
        let mut floors = [0.0f32; ON_PATH_INK_NEIGHBOR_WINDOW];
        for (floor, neighbor) in floors.iter_mut().zip(state.neighbors.iter()) {
            *floor = pair_clearance_floor(&neighbor.straight.contour, &cur_straight.contour);
        }
        let cleared = find_clearance_center_s(
            path,
            center_s,
            &state.neighbors,
            &floors[..state.neighbors.len()],
            |s| place_seed_ink_at(params, seed, g, path, s, &layout).map(|ink| ink.contour),
        );
        let Some(cleared) = cleared else {
            // No position up to the path end clears every neighbour, so the
            // glyph is dropped exactly as the arc-length walk would drop it. A
            // measurement pass must still walk its cursor past it.
            if state.measure_only {
                state.cursor.commit(all_seeds, seed, advance, center_s);
            }
            return None;
        };
        center_s = cleared;
    }

    // A backward move is bounded by the line's run start above, so only
    // `PastEnd` is reachable here; the shared helper keeps the two guards from
    // ever disagreeing.
    if drawn_line_drop_side(center_s, state.start_offset_s_px, path.total_len_px).is_some() {
        if state.measure_only {
            state.cursor.commit(all_seeds, seed, advance, center_s);
        }
        return None;
    }
    // The cursor walks past the glyph's FINAL (corrected) center, which is what
    // makes the run's real length differ from the alignment pre-pass estimate.
    state.cursor.commit(all_seeds, seed, advance, center_s);
    let transform = drawn_line_transform_at(params, seed, path, center_s, &layout)?;

    if use_ink {
        // The reference walk follows EVERY placed glyph, inkless ones included,
        // so a space keeps the straight reference aligned with the real walk.
        state.straight.commit(straight_x, center_s);
        state.previous_seed_idx = Some(seed_idx);
        match (geom.as_ref(), straight_ink) {
            (Some(g), Some(straight)) => {
                // Store the glyph in both frames at its FINAL position, so the
                // next glyph measures against the very ink the blit will draw
                // and against the reference that ink is judged by.
                let world = place_seed_ink_for_transform(seed, g, &layout, &transform);
                state.neighbors.push(PlacedNeighbor { world, straight });
                if state.neighbors.len() > ON_PATH_INK_NEIGHBOR_WINDOW {
                    state.neighbors.remove(0);
                }
                state.previous_has_ink = true;
            }
            _ => {
                // Empty/space glyph: it carries no ink, so the next glyph gets
                // no directional target from it. The window is deliberately
                // KEPT — the next word still must not collide with this one.
                state.previous_has_ink = false;
            }
        }
    }

    Some(transform)
}

impl DrawnLinePlacementState {
    /// Fresh state for a line whose run starts at `start_offset_s_px` (its
    /// alignment offset): the cursor and the recorded start offset are the same
    /// value, and no glyph has been placed yet.
    #[must_use]
    pub(super) fn starting_at(start_offset_s_px: f32) -> Self {
        Self {
            cursor: LineArcCursor::starting_at(start_offset_s_px),
            start_offset_s_px,
            ..Self::default()
        }
    }
}
