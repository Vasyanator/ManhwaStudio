/*
File: crates/ms-text-render/src/pipeline/run_layout.rs

Purpose:
The horizontal pen: lays out one shaped `LayoutRun` into per-glyph pen positions
(`HorizontalRunLayout`). One of the CUSTOM KERNING sites (`MODULE_README.md`): the pair
DECISION comes from `pair_advance`; this file only accumulates its `PairStep`s plus
tracking, faux growth and the optical normalization.

Key functions:
- horizontal_run_layout(): metric pen (`Auto` / `Fixed`), dispatches `Optical`
- optical_horizontal_run_layout(): ink-to-ink re-spacing of a run
- place_optical_horizontal_contour(): the draw-pass transform for measured ink
- faux_floored_advance(), anchor_rtl_run_at_its_left_edge()
*/

use crate::glyph_blit::{
    GlyphBox, glyph_outline_transform, glyph_subpixel_offset, hash_font_id,
    resolve_outline_for_glyph,
};
use crate::glyph_contour::PlacedContour;
use crate::pair_advance::{
    KerningSettings, PairAdvanceModel, PairGlyph, PairStep, RunOrientation, StepDirection,
    TrackingBasis,
};
use crate::optical::{
    OPTICAL_CONTOUR_SIMPLIFY_TOLERANCE_PX, OpticalAxis, median_of_gaps, optical_base_advance,
    optical_delta, optical_pair_gap,
};
use crate::vector::glyph_contour_from_outline;
use crate::types::KerningMode;
use cosmic_text::{FontSystem, LayoutGlyph, LayoutRun};
use super::{
    FauxGlyphStyle, GlyphCaches, GlyphScaleSettings, GlyphStyle, HorizontalRunLayout, RunPen,
};
use super::inline_lookup::{inline_glyph_scale_for_glyph, inline_kerning_for_glyph};
use super::faux::{faux_advance_extra_px_for_glyph, faux_style_for_glyph, resolve_faux_counter_flag};
use super::hanging::hanging_metrics_for_layout;

/// Combine a glyph's shaped advance with its faux-bold advance delta without
/// letting the delta REVERSE the pen.
///
/// `faux_extra` is signed (`faux_advance_extra_px_for_glyph`): at the minimum
/// strength it is `-0.10 * em` uniformly and `-0.20 * em` for a compensated
/// counter-bearing glyph in the `outward_only` mode. A glyph whose own advance
/// is smaller than that contraction — a zero-advance combining mark is the clear
/// case, narrow punctuation the marginal one — would otherwise step the pen
/// BACKWARDS and place the following glyph before the current one. The sum is
/// therefore floored at zero in the direction the run actually advances:
/// cosmic-text walks the pen LEFTWARDS in an RTL run (`shape.rs`: `x -= x_advance`
/// before the glyph is pushed), so `base_advance` is negative there and the floor
/// mirrors instead of flipping the run around.
///
/// Explicitly requested tracking/kerning is deliberately NOT part of the floor:
/// the user asking for negative letter-spacing is asking for overlap. Returns
/// `base_advance` unchanged for `faux_extra == 0.0`, so a run without faux bold
/// keeps bit-identical pen positions.
#[must_use]
pub(super) fn faux_floored_advance(base_advance: f32, faux_extra: f32) -> f32 {
    if faux_extra == 0.0 {
        return base_advance;
    }
    let stepped = base_advance + faux_extra;
    if base_advance < 0.0 {
        stepped.min(0.0)
    } else {
        stepped.max(0.0)
    }
}

/// Compute per-glyph pen positions (`glyph_xs`) and hanging metrics for one
/// horizontal layout run, honoring inline tracking and the selected kerning mode.
///
/// `Auto` mode is byte-identical to cosmic-text's shaped positions plus optional
/// manual tracking (font pair kerning applied). `Fixed` steps by each glyph's OWN
/// nominal (un-kerned) advance (`nominal_glyph_advance_px`) so font pair kerning
/// is dropped, plus manual tracking. When
/// `params.kerning_mode == KerningMode::Optical`, adjacent inked glyphs are
/// re-spaced by measuring true ink-to-ink gaps (`optical_horizontal_run_layout`)
/// and normalizing them toward the run's median gap; a run with fewer than one
/// finite gap (e.g. a single inked glyph) falls back to the metric accumulation.
///
/// `pen` carries the params, inline spans, line offsets, user-authored kerning and
/// font size of the render. `font_system` is also read by `Fixed` (nominal
/// own-advance lookup); `caches` are used only by the optical path (outline
/// extraction + ink measurement) and are untouched for `Auto`/`Fixed`.
/// Never panics; a glyph without a fillable outline is treated as non-kernable
/// (delta 0) rather than an error.
///
/// Faux bold: on the metric branches (`Auto`/`Fixed`, and the `Optical`
/// fallback) each pen step additionally grows by the SPANNING glyph's
/// `2*d + expand_px` (`faux_advance_extra_px_for_glyph`; the previous glyph
/// left-to-right, the current one right-to-left, signed onto the run axis so the
/// pair always widens), and `line_width_px`
/// includes each glyph's own extra so the trailing faux glyph is part of the
/// logical (alignment) width; a run with any faux glyph never takes the
/// byte-identical shaped-position fast path. BOTH places combine the two terms
/// through `faux_floored_advance`, so a THINNING delta larger than the glyph's
/// own advance cannot walk the pen backwards and cannot make the alignment
/// width disagree with the accumulated pen. The optical branch instead
/// measures ink from the offset outlines and adds only the normalization
/// delta. A `LayoutRun` is one whole laid-out line in cosmic-text 0.14, so
/// per-run accumulation already spans every inline size/font/style boundary.
///
/// Custom kerning: a pair listed in the glyph font's
/// `font_provider::CustomKerningTable` REPLACES the font's own value for that pair
/// under EVERY mode — the step becomes `nominal_glyph_advance_px(prev)` (the raw
/// `hmtx` advance `Fixed` already uses) plus the authored delta, so the font's
/// GPOS/`kern` contribution is dropped and the user's number stands in its place.
/// That is what makes an authored pair indistinguishable from a built-in one.
/// Uniform tracking (`extra_spacing_px`) is a separate setting and is still added
/// on top, unchanged.
pub(super) fn horizontal_run_layout(
    pen: RunPen<'_>,
    run: &LayoutRun<'_>,
    font_system: &mut FontSystem,
    caches: &mut GlyphCaches,
) -> HorizontalRunLayout {
    let RunPen {
        params,
        inline_style_spans,
        layout_line_offsets,
        custom_kerning,
        font_size_px,
    } = pen;
    if run.glyphs.is_empty() {
        return HorizontalRunLayout {
            glyph_xs: Vec::new(),
            line_width_px: run.line_w,
            leading_hang_px: 0.0,
            trailing_hang_px: 0.0,
        };
    }

    let model = PairAdvanceModel::new(custom_kerning);
    let glyph_kernings = run
        .glyphs
        .iter()
        .map(|glyph| {
            // Takes the run off the byte-identical fast path when this glyph's own
            // face carries overrides; which PAIRS are overridden is decided below.
            model.kerning_for_glyph(
                inline_kerning_for_glyph(
                    params,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                glyph,
            )
        })
        .collect::<Vec<_>>();

    // Faux-bold advance growth per glyph (`2*d + expand_px`; all zeros when
    // faux bold is off anywhere in the run). The step from glyph `i-1` to `i`
    // grows by the PREVIOUS glyph's extra: thickening a glyph widens its own
    // ink, so it pushes the following glyph away. No cross-run carry is
    // needed: a cosmic-text 0.14 `LayoutRun` is one WHOLE laid-out line
    // (`LayoutRun.glyphs` spans every attrs/size/font boundary of the line,
    // see cosmic-text `buffer.rs`), so this accumulation already covers inline
    // boundaries; the trailing glyph's extra is folded into `line_width_px`
    // below instead.
    let mut faux_extras = Vec::with_capacity(run.glyphs.len());
    for glyph in run.glyphs {
        // The counter flag must be resolved BEFORE the advance is derived from
        // it, otherwise a counter-bearing glyph in the `outward_only` mode would
        // step by `2*d` while its ink grew by `4*d` and the next letter would
        // collide with it. The lookup is skipped entirely outside that mode.
        let style = resolve_faux_counter_flag(
            faux_style_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            ),
            font_system,
            &mut caches.outlines,
            glyph,
        );
        faux_extras.push(faux_advance_extra_px_for_glyph(
            params,
            inline_style_spans,
            layout_line_offsets,
            run.line_i,
            glyph,
            style,
        ));
    }

    if glyph_kernings
        .iter()
        .all(|kerning| kerning.uses_default_metric_layout())
        && faux_extras.iter().all(|extra| *extra == 0.0)
    {
        let glyph_xs = run.glyphs.iter().map(|glyph| glyph.x).collect::<Vec<_>>();
        let (leading_hang_px, trailing_hang_px) =
            hanging_metrics_for_layout(run, glyph_xs.as_slice(), run.line_w);
        return HorizontalRunLayout {
            glyph_xs,
            line_width_px: run.line_w,
            leading_hang_px,
            trailing_hang_px,
        };
    }

    // Optical kerning: re-space adjacent inked glyphs by true ink-to-ink gaps.
    // A run that cannot be optically kerned (fewer than one finite gap) returns
    // None and falls through to the metric accumulation below.
    if params.kerning_mode == KerningMode::Optical
        && let Some(layout) =
            optical_horizontal_run_layout(pen, run, glyph_kernings.as_slice(), font_system, caches)
    {
        return layout;
    }

    let mut glyph_xs = Vec::with_capacity(run.glyphs.len());
    let mut current_x = run.glyphs.first().map(|glyph| glyph.x).unwrap_or(0.0);
    glyph_xs.push(current_x);
    let default_advance = font_size_px.max(1.0) * 0.5;

    for (idx, pair_kerning) in glyph_kernings
        .iter()
        .copied()
        .enumerate()
        .take(run.glyphs.len())
        .skip(1)
    {
        let prev = &run.glyphs[idx - 1];
        let glyph = &run.glyphs[idx];
        let metric_advance = glyph.x - prev.x;
        // `Fixed` steps by each glyph's OWN (nominal, un-kerned) advance so font
        // GPOS/`kern` pair kerning is dropped; `Auto` keeps the shaped (kerned)
        // delta; `Optical` only reaches this branch as a fallback when the run
        // cannot be optically kerned, where it keeps the shaped delta like `Auto`.
        // Note: `prev.w == metric_advance` in cosmic-text (pair kerning is baked
        // into the advance), so the nominal metrics advance is the actual lever.
        //
        // In a right-to-left run the step is spanned by `glyph` (cosmic-text puts it
        // at `prev.x - glyph.w`), so `Fixed` and an override take THAT glyph's own
        // advance and walk leftwards (`pair_advance::RunOrientation`).
        //
        // A USER-AUTHORED pair overrides all three: the step becomes the spanning
        // glyph's raw `hmtx` advance plus the authored delta, so the font's own
        // pair value is dropped and the user's value takes its place verbatim.
        // That is what makes an authored pair behave exactly like a built-in one.
        // Both inputs are UNSIGNED, so the owner re-applies the run's own direction
        // (`StepDirection::FollowRun`: an RTL run walks the pen leftwards).
        let base_advance = model
            .resolve(
                font_system,
                params.kerning_mode,
                StepDirection::FollowRun,
                PairGlyph::from_run(run.text, prev),
                PairGlyph::from_run(run.text, glyph),
                metric_advance,
            )
            .step_px();
        let tracking_basis = TrackingBasis::HorizontalPen {
            shaped_step_px: metric_advance,
            prev_w_px: prev.w,
            default_advance_px: default_advance,
        };
        // Faux-bold growth of the glyph that SPANS this step (0.0 when off): the
        // previous glyph left-to-right, the current one right-to-left (cosmic-text
        // places it at `prev.x - cur.w`), signed onto the run axis so thickening
        // always widens the pair. Floored so a thinning delta wider than that
        // glyph's own advance cannot step the pen backwards. Manual tracking stays
        // OUTSIDE the floor: a negative letter-spacing is an explicit request for
        // overlap.
        let orientation = RunOrientation::of_step(metric_advance, prev);
        let faux_extra = match orientation {
            RunOrientation::LeftToRight => faux_extras[idx - 1],
            RunOrientation::RightToLeft => -faux_extras[idx],
        };
        let effective_advance = faux_floored_advance(base_advance, faux_extra);
        // Manual tracking is a DISTANCE between the two glyphs, signed onto the run axis
        // like every other part of the step: positive tracking spreads a right-to-left
        // run too (an unsigned `+` would walk its leftward pen back and tighten it).
        current_x += effective_advance + orientation.sign(pair_kerning.tracking_px(tracking_basis));
        glyph_xs.push(current_x);
    }

    // Logical line width includes each glyph's own faux advance growth (the
    // trailing glyph's `2*d + expand` would otherwise be dropped, making
    // right/center alignment overshoot the margin by the faux growth). The same
    // `faux_floored_advance` rule as the pen above, so alignment can never be
    // computed from an effective advance the pen did not take.
    // `faux_extras` are exact zeros without faux, keeping this byte-identical.
    anchor_rtl_run_at_its_left_edge(run, &mut glyph_xs);
    let line_width_px = run
        .glyphs
        .iter()
        .zip(glyph_xs.iter().copied())
        .zip(faux_extras.iter().copied())
        .map(|((glyph, glyph_x), faux_extra)| {
            glyph_x + faux_floored_advance(glyph.w, faux_extra)
        })
        .fold(0.0, f32::max);
    let (leading_hang_px, trailing_hang_px) =
        hanging_metrics_for_layout(run, glyph_xs.as_slice(), line_width_px);
    HorizontalRunLayout {
        glyph_xs,
        line_width_px,
        leading_hang_px,
        trailing_hang_px,
    }
}

/// Keeps a right-to-left run's LEFT end where the shaper put it.
///
/// The pen accumulates from the run's first glyph, which in a right-to-left run is the
/// RIGHTMOST one, and walks leftwards. When the pen changes the run's length (an
/// override, `Fixed`, `Optical`, faux bold, tracking), the change therefore lands on the
/// LEFT end: a tightened line leaves a gap at its left, a widened one runs past `x = 0`,
/// and `line_width_px` (`max(x + w)`, the right end) never sees it — so the line is
/// aligned and wrapped by a width it is not drawn with. Shifting every pen position by
/// `min(pen x) - min(shaped x)` puts the left end back on the shaper's left end, so
/// `max(x + w)` is the line's real drawn extent; alignment and the wrap
/// (`PenRunWidth`) both read it. A left-to-right run starts at its left end already
/// and is untouched; an unchanged right-to-left run shifts by exactly `0.0`.
fn anchor_rtl_run_at_its_left_edge(run: &LayoutRun<'_>, glyph_xs: &mut [f32]) {
    if !run.rtl {
        return;
    }
    let shaped_left = run.glyphs.iter().map(|glyph| glyph.x).fold(f32::INFINITY, f32::min);
    let pen_left = glyph_xs.iter().copied().fold(f32::INFINITY, f32::min);
    let shift = pen_left - shaped_left;
    if shift == 0.0 || !shift.is_finite() {
        return;
    }
    for glyph_x in glyph_xs.iter_mut() {
        *glyph_x -= shift;
    }
}

/// Optical horizontal accumulation for one run.
///
/// MVP scope: optical pairs are considered only WITHIN a single layout run;
/// cosmic-text splits a line into runs at style/font/bidi boundaries, so a pair
/// that straddles a run boundary is spaced by the shaped advance only. This is a
/// known limitation of the horizontal optical path.
///
/// Algorithm (spec Phase 1):
/// 1. Base advance is the SPANNING glyph's own shaped advance (`prev.w`
///    left-to-right, `cur.w` right-to-left), falling back to `|cur.x - prev.x|`
///    when it is not positive/finite, signed onto the run axis
///    (`pair_advance::RunOrientation`), so a right-to-left run walks leftwards.
/// 2. For every adjacent inked pair the MINIMUM DIRECTIONAL horizontal whitespace
///    is measured (`optical_pair_gap`, `OpticalAxis::Horizontal` — an adapter over
///    the owner `pair_gap::directional_pair_gap`): the smallest
///    `cur_left(y) - prev_right(y)` over the pair's overlapping vertical band (the
///    closest facing points), from the glyph outlines placed through the exact
///    draw-pass transform. This projected measure (not a Euclidean min-distance)
///    keeps slanted/overhanging features from inverting the sign. Spaces / empty /
///    outline-less glyphs and pairs with no vertical overlap yield an infinite gap
///    (delta 0).
/// 3. The self-calibrating target is the median of all finite per-pair MIN gaps;
///    fewer than one finite gap returns `None` (caller keeps metric spacing).
/// 4. Each pair is nudged by `optical_delta` (a signed delta normalized on the
///    pair MIN gap so the closest points become uniform, clamped to +/- font_size,
///    then floored on that same MIN gap so the closest points never collide)
///    applied ON TOP of the base advance and any manual tracking
///    (`extra_spacing_px`), with the same `spacing_basis` as the metric branch.
/// 5. A pair listed in the glyph font's `CustomKerningTable` skips steps 1/2/4
///    entirely and steps by the spanning glyph's `nominal_glyph_advance_px` + authored delta: an
///    override is an explicit instruction about that one distance, so normalizing
///    it toward the run's median gap would discard the value the user typed. The
///    remaining pairs of the run are still optical.
///
/// Returns `None` to signal "cannot optically kern this run".
fn optical_horizontal_run_layout(
    pen: RunPen<'_>,
    run: &LayoutRun<'_>,
    glyph_kernings: &[KerningSettings],
    font_system: &mut FontSystem,
    caches: &mut GlyphCaches,
) -> Option<HorizontalRunLayout> {
    let RunPen {
        params,
        inline_style_spans,
        layout_line_offsets,
        custom_kerning,
        font_size_px,
    } = pen;
    let glyph_count = run.glyphs.len();
    if glyph_count < 2 {
        return None;
    }

    // Per-glyph draw scale, mirroring the draw pass (`inline_glyph_scale_for_glyph`)
    // so measured ink uses exactly the scale the glyph is rasterized with.
    let glyph_scales: Vec<GlyphScaleSettings> = run
        .glyphs
        .iter()
        .map(|glyph| {
            inline_glyph_scale_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            )
        })
        .collect();
    // Per-glyph faux style, mirroring the draw pass: optical gaps MUST be
    // measured from the same (possibly offset/sheared) ink that is drawn.
    // The counter flag is deliberately NOT resolved here. Nothing on this path
    // reads it: the contour cache keys off `bold_key_bits()` (which excludes the
    // flag by contract, it being a function of the glyph), the outline variant is
    // selected by `bold` alone, and the placement uses only `shear_x`. The
    // optical branch adds no `outer_delta_px` pen term at all — it normalizes
    // measured ink gaps — so resolving the flag would buy a winding
    // classification per glyph and change nothing.
    let glyph_fauxes: Vec<FauxGlyphStyle> = run
        .glyphs
        .iter()
        .map(|glyph| {
            faux_style_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            )
        })
        .collect();

    // gaps[idx] is the minimum directional projected whitespace of the pair
    // (idx-1, idx) — the closest facing points; index 0 has no pair. The gap is
    // x-translation invariant (rotation 0), so both glyphs are placed relative to
    // a shared baseline: prev at pen 0, cur at pen prev.w (right-to-left: at
    // -cur.w, measuring cur's right against prev's left). `f32::INFINITY` marks a
    // non-kernable pair (first glyph / space / empty / no overlap).
    let mut gaps: Vec<f32> = Vec::with_capacity(glyph_count);
    gaps.push(f32::INFINITY);
    for idx in 1..glyph_count {
        let prev = &run.glyphs[idx - 1];
        let cur = &run.glyphs[idx];
        // Left-to-right `cur` sits right of `prev` (at pen `prev.w`); right-to-left it
        // sits LEFT of it (at pen `-cur.w`, where cosmic-text puts it), and the facing
        // sides are `cur`'s right and `prev`'s left. Measuring the left-to-right
        // placement there would compare two sides that never face each other.
        let orientation = RunOrientation::of_step(cur.x - prev.x, prev);
        let cur_pen_x = match orientation {
            RunOrientation::LeftToRight => prev.w,
            RunOrientation::RightToLeft => -cur.w,
        };
        let prev_placed = place_optical_horizontal_contour(
            prev,
            0.0,
            GlyphStyle {
                scale: glyph_scales[idx - 1],
                faux: glyph_fauxes[idx - 1],
            },
            font_system,
            caches,
        );
        let cur_placed = place_optical_horizontal_contour(
            cur,
            cur_pen_x,
            GlyphStyle {
                scale: glyph_scales[idx],
                faux: glyph_fauxes[idx],
            },
            font_system,
            caches,
        );
        let gap = match (prev_placed, cur_placed, orientation) {
            (Some(p), Some(c), RunOrientation::LeftToRight) => {
                optical_pair_gap(&p, &c, OpticalAxis::Horizontal)
            }
            (Some(p), Some(c), RunOrientation::RightToLeft) => {
                optical_pair_gap(&c, &p, OpticalAxis::Horizontal)
            }
            // Space / empty / outline-less glyph on either side: not kernable.
            _ => f32::INFINITY,
        };
        gaps.push(gap);
    }

    // Self-calibrating target: the median of finite per-pair MIN gaps. None when
    // the run has no finite gap to normalize.
    let target = median_of_gaps(&gaps)?;
    let model = PairAdvanceModel::new(custom_kerning);

    let default_advance = font_size_px.max(1.0) * 0.5;
    let mut glyph_xs = Vec::with_capacity(glyph_count);
    let mut current_x = run.glyphs.first().map(|glyph| glyph.x).unwrap_or(0.0);
    glyph_xs.push(current_x);
    for idx in 1..glyph_count {
        let prev = &run.glyphs[idx - 1];
        let cur = &run.glyphs[idx];
        let metric_advance = cur.x - prev.x;
        // Keep the manual-tracking basis identical to the metric branch.
        let tracking_basis = TrackingBasis::HorizontalPen {
            shaped_step_px: metric_advance,
            prev_w_px: prev.w,
            default_advance_px: default_advance,
        };
        // A user-authored pair wins over the optical normalization too: the point
        // of an override is that the user dictated this one distance, so measuring
        // ink and moving the pair toward the run's median gap would overwrite the
        // very value they entered. Every OTHER pair of the run stays optical.
        // Direction is re-applied exactly as on the metric branch: the override's
        // inputs are unsigned, the run's step may not be.
        let step = match model.resolve(
            font_system,
            KerningMode::Optical,
            StepDirection::FollowRun,
            PairGlyph::from_run(run.text, prev),
            PairGlyph::from_run(run.text, cur),
            metric_advance,
        ) {
            PairStep::Override { step_px, .. } => step_px,
            // `OwnAdvance` is unreachable here (the owner returns it only for `Fixed`,
            // and this branch always asks with `Optical`); it is listed so the match
            // stays exhaustive and would take the optical step if that ever changed.
            // The optical step: the spanning glyph's own advance (`prev` left-to-right,
            // `cur` right-to-left) plus the normalization delta, signed onto the run
            // axis exactly like an override.
            PairStep::Shaped { .. } | PairStep::OwnAdvance { .. } => {
                let orientation = RunOrientation::of_step(metric_advance, prev);
                let spanning = orientation.spanning_glyph(prev, cur);
                orientation.sign(
                    optical_base_advance(spanning.w, metric_advance.abs())
                        + optical_delta(gaps[idx], target, font_size_px),
                )
            }
        };
        // Tracking signed onto the run axis exactly as on the metric branch.
        let tracking = RunOrientation::of_step(metric_advance, prev)
            .sign(glyph_kernings[idx].tracking_px(tracking_basis));
        current_x += step + tracking;
        glyph_xs.push(current_x);
    }

    anchor_rtl_run_at_its_left_edge(run, &mut glyph_xs);
    let line_width_px = run
        .glyphs
        .iter()
        .zip(glyph_xs.iter().copied())
        .map(|(glyph, glyph_x)| glyph_x + glyph.w)
        .fold(0.0, f32::max);
    let (leading_hang_px, trailing_hang_px) =
        hanging_metrics_for_layout(run, glyph_xs.as_slice(), line_width_px);
    Some(HorizontalRunLayout {
        glyph_xs,
        line_width_px,
        leading_hang_px,
        trailing_hang_px,
    })
}

/// Place a glyph's ink contour in world space using the exact transform the
/// horizontal draw pass (`draw_horizontal_placement`) uses, so the measured ink
/// matches the drawn ink.
///
/// `pen_x` is the pen x in layout px (the baseline y is irrelevant to the gap and
/// is fixed at 0). `faux` selects the faux-bold outline variant the contour is
/// derived from (the contour cache is keyed per variant, like `OutlineKey`) and
/// the faux-italic shear the placement applies, so the measured ink matches the
/// drawn ink exactly. Returns `None` for a space/empty glyph (zero-size
/// placement), an outline-less color glyph, or an empty contour — all of which
/// are treated as non-kernable by the caller. Never panics.
fn place_optical_horizontal_contour(
    glyph: &LayoutGlyph,
    pen_x: f32,
    style: GlyphStyle,
    font_system: &mut FontSystem,
    caches: &mut GlyphCaches,
) -> Option<PlacedContour> {
    let GlyphStyle {
        scale: glyph_scale,
        faux,
    } = style;
    let physical = glyph.physical((pen_x, 0.0), 1.0);
    let cache_key = physical.cache_key;
    // Copy the bitmap placement box out before the swash image borrow ends.
    let (glyph_w, glyph_h, placement_left, placement_top, src_left, src_top) = {
        let Some(image) = caches.swash.get_image(font_system, cache_key) else {
            return None;
        };
        let gw = image.placement.width;
        let gh = image.placement.height;
        if gw == 0 || gh == 0 {
            return None;
        }
        (
            gw as f32,
            gh as f32,
            image.placement.left as f32,
            image.placement.top as f32,
            (physical.x + image.placement.left) as f32,
            (physical.y - image.placement.top) as f32,
        )
    };

    // Derive the ink contour once per distinct (font, glyph, em); the outline
    // itself is negatively cached by `OutlineCache`, so an outline-less glyph is
    // cheap to re-probe even without a contour-cache entry.
    let contour_key = (
        hash_font_id(glyph.font_id),
        glyph.glyph_id,
        glyph.font_size.to_bits(),
        faux.bold_key_bits(),
    );
    let contour = match caches.contours.entry(contour_key) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            let outline = resolve_outline_for_glyph(font_system, &mut caches.outlines, glyph, faux.bold)?;
            entry.insert(glyph_contour_from_outline(
                &outline,
                OPTICAL_CONTOUR_SIMPLIFY_TOLERANCE_PX,
            ))
        }
    };
    if contour.is_empty() {
        return None;
    }

    // Same baseline anchor as `draw_horizontal_placement`, so the measured ink is
    // the drawn ink. The baseline here is the probe pen (`pos_y = 0`), which the
    // gap measurement never reads, but the two paths must not diverge.
    let (dst_center_x, dst_center_y) = glyph_scale.scaled_center_about_baseline(
        src_left,
        src_top,
        glyph_w,
        glyph_h,
        src_top + placement_top,
    );
    let transform = glyph_outline_transform(
        GlyphBox {
            placement_left,
            placement_top,
            glyph_w,
            glyph_h,
            scale: glyph_scale,
            subpixel: glyph_subpixel_offset(cache_key),
            shear_x: faux.shear_x,
        },
        [dst_center_x, dst_center_y],
        0.0,
    );
    Some(transform.place_contour(contour))
}
