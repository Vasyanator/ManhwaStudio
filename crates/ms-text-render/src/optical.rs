/*
File: crates/ms-text-render/src/optical.rs

Purpose:
Axis-agnostic pure numeric core shared by the optical-kerning paths. The
horizontal path (`pipeline.rs`) and the vertical path (`layout/vertical.rs`)
both re-space adjacent inked glyphs by measuring the MINIMUM DIRECTIONAL
projected ink whitespace of each pair — the closest facing points between the two
glyphs — and normalizing it toward the run/column median so the tightest points
are uniform. The self-calibrating math and the shared cache/tolerance/floor
constants live here, so there is exactly one source of truth for the optical
spacing formula.

The gap MEASUREMENT itself is not implemented here: it is owned by `pair_gap.rs`
(`directional_pair_gap` over a `GapAxis`), which measures on an arbitrary
oriented axis so the on-path path can share it. `OpticalAxis` and
`optical_pair_gap` are the optical paths' thin adapter onto that owner, and the
world-axis-aligned frames are `GapAxis::HORIZONTAL` / `GapAxis::VERTICAL`.
Contour placement (the exact draw-pass transform) stays in the axis-specific
callers.

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
use super::pair_gap::{GapAxis, directional_pair_gap};
use std::collections::HashMap;

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
///
/// Both are world-axis-aligned special cases of the general oriented frame
/// [`GapAxis`]; this enum exists so the optical call sites keep naming their
/// advance axis instead of building vectors.
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
/// Thin adapter: it only maps [`OpticalAxis`] onto the world-axis-aligned
/// [`GapAxis`] frames and delegates to [`directional_pair_gap`], the single owner
/// of the measurement. The full contract — signed result, negative on overlapping
/// ink, `f32::INFINITY` for every non-measurable case (empty contour, no
/// vertices, no band overlap, no scanline with ink on both sides) — is documented
/// there and holds verbatim here.
///
/// `prev` and `cur` must already be placed in the SAME world frame the draw pass
/// uses (horizontal: prev at pen 0, cur at pen `prev.w`; vertical: prev ink-top
/// at local 0, cur ink-top at prev's base advance). Never panics.
#[must_use]
pub(crate) fn optical_pair_gap(prev: &PlacedContour, cur: &PlacedContour, axis: OpticalAxis) -> f32 {
    let axis = match axis {
        OpticalAxis::Horizontal => GapAxis::HORIZONTAL,
        OpticalAxis::Vertical => GapAxis::VERTICAL,
    };
    directional_pair_gap(prev, cur, axis)
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
