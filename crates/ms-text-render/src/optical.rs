/*
File: src/tabs/typing/render_next/optical.rs

Purpose:
Axis-agnostic pure numeric core shared by the optical-kerning paths. The
horizontal path (`pipeline.rs`) and the vertical path (`layout/vertical.rs`)
both re-space adjacent inked glyphs by measuring the MINIMUM DIRECTIONAL
projected ink whitespace of each pair — the closest facing points between the two
glyphs — and normalizing it toward the run/column median so the tightest points
are uniform. The per-pair gap is a scanline projection along the advance axis
(NOT a Euclidean minimum distance): for the horizontal axis it is the smallest
horizontal whitespace over the overlapping vertical band; for the vertical axis
it is the smallest vertical whitespace over the overlapping horizontal band.
Contour placement (the exact draw-pass transform) stays in the axis-specific
callers; the scanline metric, the self-calibrating math, and the shared
cache/tolerance/floor constants live here, so there is exactly one source of
truth for the optical spacing formula.

Key types:
- OpticalContourCache
- OpticalAxis

Key functions:
- median_of_gaps()
- optical_base_advance()
- optical_delta()
- optical_pair_gap()

Notes:
Shared by the horizontal and vertical optical kerning accumulation. Both callers
gate the use of these helpers strictly on `KerningMode::Optical`; every other
mode never touches this module.
*/

use super::glyph_contour::{GlyphContour, PlacedContour};
use std::collections::HashMap;

/// Upper bound on the number of scanline samples taken across the overlap band
/// of a pair. Very tall/wide glyph pairs widen the step (coarser than 1px)
/// instead of scanning every pixel row/column, bounding the cost per pair.
const OPTICAL_MAX_SCAN_SAMPLES: usize = 512;

/// Bezier-flattening simplify tolerance (px) for glyph ink contours used by the
/// optical accumulation on both axes; kept equal to the on-path value in
/// `formula/render.rs` so measured ink matches the drawn ink.
pub(crate) const OPTICAL_CONTOUR_SIMPLIFY_TOLERANCE_PX: f32 = 1.5;

/// Hard lower bound on the resulting ink-to-ink gap (px) for optical kerning; the
/// applied delta never lets an adjacent pair collide tighter than this. Kept
/// equal to the on-path floor in `formula/render.rs`. Shared by both axes.
pub(crate) const OPTICAL_MIN_INK_GAP_FLOOR_PX: f32 = 0.5;

/// Per-render cache of glyph ink contours keyed by
/// `(hash_font_id(font_id), glyph_id, font_size.to_bits(), faux_bold_bits)`,
/// so the bounds and draw passes plus repeated glyphs derive each contour at
/// most once. The last component is `vector::faux_key_bits` (`0` = plain), so a
/// faux-bold (offset-outline) variant never aliases the plain contour — the
/// same variant keying `OutlineKey` uses. Shared by the horizontal
/// (`pipeline.rs`) and vertical (`layout/vertical.rs`) paths.
pub(crate) type OpticalContourCache = HashMap<(u64, u16, u32, u32), GlyphContour>;

/// Median of the finite entries of `gaps`.
///
/// Non-finite entries (infinite gaps from spaces/empty/outline-less pairs) are
/// excluded before the median. Returns `None` when no finite gap exists, i.e. the
/// run/column cannot be optically normalized. For an even count the two central
/// values are averaged. Shared by the horizontal and vertical optical paths.
#[must_use]
pub(crate) fn median_of_gaps(gaps: &[f32]) -> Option<f32> {
    let mut finite: Vec<f32> = gaps.iter().copied().filter(|g| g.is_finite()).collect();
    if finite.is_empty() {
        return None;
    }
    finite.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = finite.len();
    let mid = n / 2;
    Some(if n % 2 == 1 {
        finite[mid]
    } else {
        (finite[mid - 1] + finite[mid]) * 0.5
    })
}

/// Base advance for one optical step: the glyph's own advance `own_advance`, or
/// the metric advance `metric_advance` when `own_advance` is not a positive finite
/// value (defensive against degenerate/zero-advance glyphs).
///
/// Axis-agnostic: `own_advance` is the horizontal shaped advance (`prev.w`) on the
/// horizontal path and the vertical per-glyph step (ink height + base gap) on the
/// vertical path; `metric_advance` is the corresponding metric fallback.
#[must_use]
pub(crate) fn optical_base_advance(own_advance: f32, metric_advance: f32) -> f32 {
    if own_advance.is_finite() && own_advance > 0.0 {
        own_advance
    } else {
        metric_advance
    }
}

/// Advance axis of an optical pair; selects which projected whitespace
/// `optical_pair_gap` measures.
///
/// - `Horizontal`: the gap is the horizontal whitespace (`cur_left - prev_right`)
///   measured over the pair's overlapping VERTICAL band (prev is the left glyph,
///   cur the right glyph).
/// - `Vertical`: the gap is the vertical whitespace (`cur_top - prev_bottom`)
///   measured over the pair's overlapping HORIZONTAL band (prev is the upper
///   glyph, cur the lower glyph).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpticalAxis {
    Horizontal,
    Vertical,
}

/// Signed spacing delta that nudges an adjacent pair's minimum ink gap toward
/// `target`.
///
/// A non-finite `gap` (space/empty/outline-less/no-overlap pair) yields `0.0` (no
/// kern). Otherwise `delta = target - gap` pulls loose pairs closed and pushes
/// tight pairs open, normalizing on the MINIMUM projected whitespace (the closest
/// facing points). The magnitude is clamped to `+/- font_size` (sanity bound) and
/// then floored on the same minimum gap so the resulting facing-edge gap never
/// drops below `OPTICAL_MIN_INK_GAP_FLOOR_PX` (hard anti-collision safety applied
/// last so it always holds: a shift by `delta` moves the facing edges by
/// ~`delta`, so `gap + delta` is the resulting closest gap). `gap`, `target`, and
/// `font_size` are in px. Shared by both optical axes.
#[must_use]
pub(crate) fn optical_delta(gap: f32, target: f32, font_size: f32) -> f32 {
    if !gap.is_finite() {
        return 0.0;
    }
    let magnitude = font_size.abs();
    // Sanity bound on how far a single pair may move (normalize on the min gap).
    let bounded = (target - gap).clamp(-magnitude, magnitude);
    // Hard floor last: keep gap + delta >= floor (never collide tighter). The
    // floor keys off the same min gap, so the closest points can't collide below
    // OPTICAL_MIN_INK_GAP_FLOOR_PX.
    bounded.max(OPTICAL_MIN_INK_GAP_FLOOR_PX - gap)
}

/// Minimum directional projected ink whitespace between two placed glyph
/// contours along `axis` (see [`OpticalAxis`]) — the closest facing points of the
/// pair (px).
///
/// `prev` and `cur` must already be placed in the SAME world frame the draw pass
/// uses (horizontal: prev at pen 0, cur at pen `prev.w`; vertical: prev ink-top
/// at local 0, cur ink-top at prev's base advance). The measure is a scanline
/// projection, NOT a Euclidean minimum distance: it reports the whitespace along
/// the advance axis so slanted/overhanging features do not invert the sign of the
/// correction.
///
/// Returns `f32::INFINITY` when the glyphs do not overlap on the band axis
/// (perpendicular to the advance axis), when either contour is empty, or when no
/// scanline has ink on BOTH glyphs. Otherwise returns the SMALLEST per-scanline
/// gap over the contributing scanlines (the tightest facing points). Never panics.
#[must_use]
pub(crate) fn optical_pair_gap(prev: &PlacedContour, cur: &PlacedContour, axis: OpticalAxis) -> f32 {
    if prev.components.is_empty() || cur.components.is_empty() {
        return f32::INFINITY;
    }

    // `band_axis` is the coordinate we sample along (perpendicular to the
    // advance); `gap_axis` is the coordinate the whitespace is measured on. For
    // the horizontal advance we scan rows (y) and measure x; for the vertical
    // advance we scan columns (x) and measure y.
    let (band_axis, gap_axis) = match axis {
        OpticalAxis::Horizontal => (1usize, 0usize),
        OpticalAxis::Vertical => (0usize, 1usize),
    };

    // Overlap band on the sampling axis; no overlap -> not kernable.
    let lo = prev.aabb_min[band_axis].max(cur.aabb_min[band_axis]);
    let hi = prev.aabb_max[band_axis].min(cur.aabb_max[band_axis]);
    let span = hi - lo;
    if !span.is_finite() || span <= 0.0 {
        return f32::INFINITY;
    }

    // ~1px step, clamped to OPTICAL_MAX_SCAN_SAMPLES samples (step widens for very
    // tall/wide pairs). Sampling at row centers (`+ 0.5` of the step) keeps the
    // scanline off the band endpoints and off exact integer vertices, which the
    // even-odd crossing test handles poorly.
    let sample_count = (span.ceil() as usize)
        .clamp(1, OPTICAL_MAX_SCAN_SAMPLES);
    let step = span / sample_count as f32;

    let mut min_gap = f32::INFINITY;
    let mut contributing = 0usize;
    for i in 0..sample_count {
        let s = lo + (i as f32 + 0.5) * step;
        // prev faces cur with its far edge (MAX gap coord); cur faces prev with
        // its near edge (MIN gap coord). A row contributes only when BOTH glyphs
        // have ink crossing that scanline.
        let Some((_, prev_far)) = scanline_crossings(&prev.components, band_axis, gap_axis, s) else {
            continue;
        };
        let Some((cur_near, _)) = scanline_crossings(&cur.components, band_axis, gap_axis, s) else {
            continue;
        };
        // The closest facing points are the smallest per-scanline gap.
        min_gap = min_gap.min(cur_near - prev_far);
        contributing += 1;
    }

    if contributing == 0 {
        return f32::INFINITY;
    }
    min_gap
}

/// Min/max `gap_axis` coordinate where the closed-polygon edges of `components`
/// cross the scanline `band_axis == s`.
///
/// Each component is a closed ring (closing edge `last -> first` implicit). An
/// edge `p0 -> p1` crosses the scanline when `(p0[band] <= s) != (p1[band] <= s)`;
/// the crossing `gap_axis` coordinate is the linear interpolation at `s`. Returns
/// `None` when no edge crosses (the glyph has no ink at that scanline). The
/// divisor `p1[band] - p0[band]` is non-zero exactly because the endpoints fall
/// on opposite sides of `s`.
fn scanline_crossings(
    components: &[Vec<[f32; 2]>],
    band_axis: usize,
    gap_axis: usize,
    s: f32,
) -> Option<(f32, f32)> {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    let mut found = false;
    for component in components {
        let n = component.len();
        if n < 2 {
            continue;
        }
        for i in 0..n {
            let p0 = component[i];
            let p1 = component[(i + 1) % n];
            let b0 = p0[band_axis];
            let b1 = p1[band_axis];
            if (b0 <= s) != (b1 <= s) {
                let t = (s - b0) / (b1 - b0);
                let g = p0[gap_axis] + t * (p1[gap_axis] - p0[gap_axis]);
                lo = lo.min(g);
                hi = hi.max(g);
                found = true;
            }
        }
    }
    if found { Some((lo, hi)) } else { None }
}

#[cfg(test)]
mod tests {
    use super::{
        OPTICAL_MIN_INK_GAP_FLOOR_PX, OpticalAxis, median_of_gaps, optical_base_advance,
        optical_delta, optical_pair_gap,
    };
    use crate::glyph_contour::GlyphContour;

    /// Place an axis-aligned rectangle in world space (identity transform).
    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> super::PlacedContour {
        GlyphContour {
            components: vec![vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]],
        }
        .placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0)
    }

    #[test]
    fn optical_median_normalizes_tight_and_loose_pairs() {
        // A tight pair (small gap) and a loose pair (large gap). The target is
        // their median; the tight pair is pushed open (positive delta) and the
        // loose pair is pulled closed (negative delta), both converging toward it.
        let gaps = [f32::INFINITY, 2.0, 10.0];
        let target = median_of_gaps(&gaps).expect("two finite gaps -> median");
        assert!(
            (target - 6.0).abs() < 1e-4,
            "median of 2 and 10 is 6, got {target}"
        );

        let font_size = 100.0;
        let delta_tight = optical_delta(2.0, target, font_size);
        let delta_loose = optical_delta(10.0, target, font_size);
        assert!(delta_tight > 0.0, "tight pair opens: {delta_tight}");
        assert!(delta_loose < 0.0, "loose pair closes: {delta_loose}");
        // Resulting gaps both land on the target (no clamp/floor active here).
        assert!((2.0 + delta_tight - target).abs() < 1e-4);
        assert!((10.0 + delta_loose - target).abs() < 1e-4);
    }

    #[test]
    fn optical_median_excludes_infinite_gaps() {
        // Spaces/empty/outline-less pairs contribute an infinite gap and must be
        // excluded from the median; a run with no finite gap yields None.
        assert!(median_of_gaps(&[f32::INFINITY, f32::INFINITY]).is_none());
        let m = median_of_gaps(&[f32::INFINITY, 4.0, f32::INFINITY]).expect("one finite gap");
        assert!((m - 4.0).abs() < 1e-4, "median ignores infinities, got {m}");
    }

    #[test]
    fn optical_delta_zero_for_infinite_gap() {
        // A non-finite (space/empty/no-overlap) pair must never kern.
        assert_eq!(optical_delta(f32::INFINITY, 6.0, 50.0), 0.0);
    }

    #[test]
    fn optical_delta_magnitude_clamped_to_font_size() {
        let font_size = 12.0;
        // A huge target vs a tight gap wants a large positive delta -> clamped up.
        let d_pos = optical_delta(1.0, 1000.0, font_size);
        assert!((d_pos - font_size).abs() < 1e-4, "positive clamp: {d_pos}");
        // A tiny target vs a very loose gap wants a large negative delta ->
        // clamped down to -font_size (floor is far more negative, so inactive).
        let d_neg = optical_delta(500.0, 1.0, font_size);
        assert!((d_neg + font_size).abs() < 1e-4, "negative clamp: {d_neg}");
    }

    #[test]
    fn optical_delta_floor_keys_off_min_gap() {
        // The collision floor keys off the single min gap (the closest facing
        // points). A pair pulled hard closed must not collide below the floor at
        // its tightest point.
        let gap = 0.2;
        let d = optical_delta(gap, -50.0, 100.0);
        assert!(
            (gap + d - OPTICAL_MIN_INK_GAP_FLOOR_PX).abs() < 1e-4,
            "floored on min gap: gap+delta should equal {OPTICAL_MIN_INK_GAP_FLOOR_PX}, got {}",
            gap + d
        );
        // For any finite gap/target the resulting closest gap is never below the
        // floor.
        for &gap in &[0.0f32, 0.2, 0.5, 3.0, 40.0] {
            for &target in &[-100.0f32, -1.0, 0.0, 5.0, 200.0] {
                let delta = optical_delta(gap, target, 80.0);
                assert!(
                    gap + delta >= OPTICAL_MIN_INK_GAP_FLOOR_PX - 1e-4,
                    "gap {gap} target {target} -> resulting min gap {} below floor",
                    gap + delta
                );
            }
        }
    }

    #[test]
    fn pair_gap_horizontal_uniform_rectangles() {
        // prev right edge at x=2, cur left edge at x=7, sharing rows y in [0,4].
        // Uniform horizontal gap of 5 over the whole overlap band.
        let prev = rect(0.0, 0.0, 2.0, 4.0);
        let cur = rect(7.0, 0.0, 9.0, 4.0);
        let gap = optical_pair_gap(&prev, &cur, OpticalAxis::Horizontal);
        assert!((gap - 5.0).abs() < 1e-3, "min gap {gap}");
    }

    #[test]
    fn pair_gap_horizontal_no_vertical_overlap_is_infinite() {
        // cur sits entirely below prev: no shared vertical band -> not kernable.
        let prev = rect(0.0, 0.0, 2.0, 4.0);
        let cur = rect(7.0, 10.0, 9.0, 14.0);
        let gap = optical_pair_gap(&prev, &cur, OpticalAxis::Horizontal);
        assert!(gap.is_infinite(), "gap {gap}");
    }

    #[test]
    fn pair_gap_horizontal_is_directional_not_euclidean() {
        // prev is a tall thin bar; cur is a small block near prev's TOP, offset
        // up and to the right. The Euclidean nearest approach is the diagonal
        // from prev's top-right corner to cur's bottom-left corner; the
        // DIRECTIONAL horizontal gap over the shared band is purely horizontal
        // and strictly smaller than that diagonal.
        let prev = rect(0.0, 0.0, 2.0, 20.0);
        let cur = rect(6.0, 0.0, 8.0, 4.0);
        let gap = optical_pair_gap(&prev, &cur, OpticalAxis::Horizontal);
        // Shared band is y in [0,4]; horizontal gap = cur_left(6) - prev_right(2) = 4.
        assert!(
            (gap - 4.0).abs() < 1e-3,
            "directional horizontal gap should be 4, got {gap}"
        );
        // A diagonal (Euclidean) measure between the facing corners would exceed 4.
        assert!(gap < 5.0, "must be the horizontal projection, not diagonal");
    }

    #[test]
    fn pair_gap_vertical_uniform_rectangles() {
        // prev bottom edge at y=2, cur top edge at y=7, sharing columns x in [0,4].
        // Uniform vertical gap of 5 over the whole overlap band.
        let prev = rect(0.0, 0.0, 4.0, 2.0);
        let cur = rect(0.0, 7.0, 4.0, 9.0);
        let gap = optical_pair_gap(&prev, &cur, OpticalAxis::Vertical);
        assert!((gap - 5.0).abs() < 1e-3, "min gap {gap}");
    }

    #[test]
    fn pair_gap_vertical_no_horizontal_overlap_is_infinite() {
        // cur sits entirely to the right of prev: no shared column -> not kernable.
        let prev = rect(0.0, 0.0, 4.0, 2.0);
        let cur = rect(10.0, 7.0, 14.0, 9.0);
        let gap = optical_pair_gap(&prev, &cur, OpticalAxis::Vertical);
        assert!(gap.is_infinite(), "gap {gap}");
    }

    #[test]
    fn pair_gap_empty_contour_is_infinite() {
        let empty = super::PlacedContour::default();
        let real = rect(0.0, 0.0, 2.0, 4.0);
        assert!(optical_pair_gap(&empty, &real, OpticalAxis::Horizontal).is_infinite());
        assert!(optical_pair_gap(&real, &empty, OpticalAxis::Horizontal).is_infinite());
    }

    #[test]
    fn optical_base_advance_selects_own_advance_or_metric_fallback() {
        // Positive finite own advance is used verbatim.
        assert!((optical_base_advance(18.0, 22.0) - 18.0).abs() < 1e-4);
        // Non-positive or non-finite own advance falls back to the metric advance.
        assert!((optical_base_advance(0.0, 22.0) - 22.0).abs() < 1e-4);
        assert!((optical_base_advance(-3.0, 22.0) - 22.0).abs() < 1e-4);
        assert!((optical_base_advance(f32::NAN, 22.0) - 22.0).abs() < 1e-4);
        assert!((optical_base_advance(f32::INFINITY, 22.0) - 22.0).abs() < 1e-4);
    }
}
