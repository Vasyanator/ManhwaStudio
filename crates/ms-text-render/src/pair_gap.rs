/*
File: crates/ms-text-render/src/pair_gap.rs

Purpose:
Single owner of the "how far apart are two adjacent placed glyphs" measurement:
the MINIMUM DIRECTIONAL projected ink whitespace of a pair along an arbitrary
oriented axis. Every spacing path in this crate that re-spaces a pair by its true
ink whitespace measures it here and nowhere else.

Main responsibilities:
- define the oriented measuring frame (`GapAxis`: a unit "forward" advance
  direction plus its perpendicular "cross" band direction);
- scan the pair's overlap band on the cross axis and report the smallest signed
  facing gap along the forward axis.

Key structures:
- GapAxis: forward/cross unit vectors; `GapAxis::HORIZONTAL` / `GapAxis::VERTICAL`
  are the world-axis-aligned special cases used by optical kerning.

Key functions:
- directional_pair_gap(): the metric itself.

Notes:
- The measure is a scanline PROJECTION, not a Euclidean minimum distance
  (`glyph_contour::min_placed_distance`): a Euclidean minimum inverts the sign of
  the correction on slanted/overhanging pairs. See the MEASUREMENT CONTRACT in
  `MODULE_README.md`.
- The axis is carried as vectors rather than as an array index, so a pair whose
  glyphs are ROTATED (text on a path) can be measured in its own local frame.
  `PlacedContour` vertices are already world-space with the rotation baked in
  (`glyph_contour.rs`), so the metric needs no axis-aligned geometry.
- Pure `[f32; 2]` math, no font/layout/egui types: directly unit-testable.
*/

use crate::glyph_contour::PlacedContour;

/// Upper bound on the number of scanline samples taken across the overlap band
/// of a pair. Very tall/wide glyph pairs widen the step (coarser than 1px)
/// instead of scanning every pixel row/column, bounding the cost per pair.
const PAIR_GAP_MAX_SCAN_SAMPLES: usize = 512;

/// Oriented measuring frame of a pair gap: which direction the pair advances in
/// and which direction its overlap band is sampled along.
///
/// `forward` points from `prev` toward `cur` (the advance direction); `cross` is
/// its perpendicular, the direction the overlap band is measured and sampled on.
/// Both are expected to be unit length and mutually perpendicular — the caller
/// owns that invariant, exactly as `GlyphContour::placed` owns its `cos`/`sin`.
/// Nothing panics if they are not; the result is simply not a distance.
///
/// The two world-axis-aligned cases are [`GapAxis::HORIZONTAL`] and
/// [`GapAxis::VERTICAL`]; an on-path pair supplies its own rotated frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GapAxis {
    /// Advance direction: the gap is measured as a projection onto this vector.
    forward: [f32; 2],
    /// Band direction: the overlap band is projected onto and sampled along this
    /// vector.
    cross: [f32; 2],
}

impl GapAxis {
    /// Frame from an explicit `forward`/`cross` pair (both unit length and
    /// mutually perpendicular; see the type contract).
    #[must_use]
    pub(crate) const fn new(forward: [f32; 2], cross: [f32; 2]) -> Self {
        Self { forward, cross }
    }

    /// Left-to-right advance: the gap is the horizontal whitespace
    /// (`cur_left - prev_right`) over the pair's overlapping VERTICAL band
    /// (`prev` is the left glyph, `cur` the right one).
    pub(crate) const HORIZONTAL: Self = Self::new([1.0, 0.0], [0.0, 1.0]);

    /// Top-to-bottom advance: the gap is the vertical whitespace
    /// (`cur_top - prev_bottom`) over the pair's overlapping HORIZONTAL band
    /// (`prev` is the upper glyph, `cur` the lower one).
    pub(crate) const VERTICAL: Self = Self::new([0.0, 1.0], [1.0, 0.0]);
}

/// Minimum directional projected ink whitespace between two placed glyph
/// contours along `axis` — the gap at the pair's closest facing points (px).
///
/// `prev` and `cur` must already be placed in the SAME world frame the draw pass
/// uses, with `cur` on the `axis.forward` side of `prev`. The measure is a
/// scanline projection, NOT a Euclidean minimum distance: it reports the
/// whitespace along the advance axis, so slanted/overhanging features cannot
/// invert the sign of a spacing correction derived from it.
///
/// The pair's overlap band is the intersection of the two contours' extents
/// projected onto `axis.cross`. It is sampled at ~1px steps (at most
/// [`PAIR_GAP_MAX_SCAN_SAMPLES`] samples, widening the step for very tall/wide
/// pairs), at sample centers so a scanline never lands on a band endpoint or an
/// exact vertex. Each sample contributes `cur_near - prev_far`, the signed
/// distance between `prev`'s far edge and `cur`'s near edge along `axis.forward`.
///
/// # Returned value
/// - the SMALLEST per-scanline signed gap over the contributing scanlines (the
///   tightest facing points) in the general case;
/// - a NEGATIVE value when the two inks overlap on some scanline (`cur`'s near
///   edge lies behind `prev`'s far edge there) — the metric is signed and does
///   NOT clamp at zero, unlike `glyph_contour::min_placed_distance`;
/// - `f32::INFINITY` in every non-measurable case: either contour has no
///   components, either contour has components but no vertices, the projected
///   band extents do not overlap (a zero-width touching band counts as no
///   overlap), or no sampled scanline crosses ink on BOTH contours.
///
/// Sampling is a discretization: features narrower than the step of a band wider
/// than [`PAIR_GAP_MAX_SCAN_SAMPLES`] px can be missed. Non-finite vertex
/// coordinates yield an unspecified (but never panicking) result.
#[must_use]
pub(crate) fn directional_pair_gap(prev: &PlacedContour, cur: &PlacedContour, axis: GapAxis) -> f32 {
    if prev.components.is_empty() || cur.components.is_empty() {
        return f32::INFINITY;
    }

    // Overlap band on the cross axis; no overlap -> not measurable. Computed by
    // projecting the vertices rather than by reusing the cached world AABB, so a
    // rotated frame is handled by the same code path (for the world-aligned
    // frames the projection reproduces the AABB extents exactly).
    let (prev_lo, prev_hi) = cross_extent(&prev.components, axis.cross);
    let (cur_lo, cur_hi) = cross_extent(&cur.components, axis.cross);
    let lo = prev_lo.max(cur_lo);
    let hi = prev_hi.min(cur_hi);
    let span = hi - lo;
    if !span.is_finite() || span <= 0.0 {
        return f32::INFINITY;
    }

    // ~1px step, clamped to PAIR_GAP_MAX_SCAN_SAMPLES samples (step widens for
    // very tall/wide pairs). Sampling at band-slice centers (`+ 0.5` of the step)
    // keeps the scanline off the band endpoints and off exact integer vertices,
    // which the even-odd crossing test handles poorly.
    // `as usize` is safe here: `span` is finite and > 0, and a saturating huge
    // value is clamped to the sample cap on the next call.
    let sample_count = (span.ceil() as usize).clamp(1, PAIR_GAP_MAX_SCAN_SAMPLES);
    let step = span / sample_count as f32;

    let mut min_gap = f32::INFINITY;
    let mut contributing = 0usize;
    for i in 0..sample_count {
        let s = lo + (i as f32 + 0.5) * step;
        // prev faces cur with its far edge (MAX forward coord); cur faces prev
        // with its near edge (MIN forward coord). A scanline contributes only
        // when BOTH glyphs have ink crossing it.
        let Some((_, prev_far)) = scanline_crossings(&prev.components, axis, s) else {
            continue;
        };
        let Some((cur_near, _)) = scanline_crossings(&cur.components, axis, s) else {
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

/// Min/max projection of every vertex of `components` onto `cross`.
///
/// Returns `(f32::INFINITY, f32::NEG_INFINITY)` when there is no vertex at all,
/// which the caller turns into a non-finite band span (not measurable).
fn cross_extent(components: &[Vec<[f32; 2]>], cross: [f32; 2]) -> (f32, f32) {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for component in components {
        for vertex in component {
            let p = project(*vertex, cross);
            lo = lo.min(p);
            hi = hi.max(p);
        }
    }
    (lo, hi)
}

/// Min/max `axis.forward` coordinate where the closed-polygon edges of
/// `components` cross the scanline `project(v, axis.cross) == s`.
///
/// Each component is a closed ring (closing edge `last -> first` implicit). An
/// edge `p0 -> p1` crosses the scanline when `(b0 <= s) != (b1 <= s)` for the
/// endpoints' cross projections; the crossing forward coordinate is the linear
/// interpolation at `s`. Returns `None` when no edge crosses (the glyph has no
/// ink at that scanline). The divisor `b1 - b0` is non-zero exactly because the
/// endpoints fall on opposite sides of `s`.
fn scanline_crossings(components: &[Vec<[f32; 2]>], axis: GapAxis, s: f32) -> Option<(f32, f32)> {
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
            let b0 = project(p0, axis.cross);
            let b1 = project(p1, axis.cross);
            if (b0 <= s) != (b1 <= s) {
                let t = (s - b0) / (b1 - b0);
                let g0 = project(p0, axis.forward);
                let g1 = project(p1, axis.forward);
                let g = g0 + t * (g1 - g0);
                lo = lo.min(g);
                hi = hi.max(g);
                found = true;
            }
        }
    }
    if found { Some((lo, hi)) } else { None }
}

/// Dot product of a vertex with an axis vector (the vertex's coordinate in that
/// direction).
#[inline]
fn project(v: [f32; 2], axis: [f32; 2]) -> f32 {
    v[0] * axis[0] + v[1] * axis[1]
}

#[cfg(test)]
mod tests {
    use super::{GapAxis, directional_pair_gap};
    use crate::glyph_contour::{GlyphContour, PlacedContour};

    /// Place an axis-aligned rectangle in world space (identity transform).
    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> PlacedContour {
        GlyphContour {
            components: vec![vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]],
        }
        .placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0)
    }

    /// Place an axis-aligned rectangle rotated by `(cos, sin)` about the origin.
    fn rotated_rect(x0: f32, y0: f32, x1: f32, y1: f32, cos: f32, sin: f32) -> PlacedContour {
        GlyphContour {
            components: vec![vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]],
        }
        .placed(cos, sin, 1.0, 1.0, 0.0, 0.0)
    }

    #[test]
    fn overlapping_inks_yield_a_negative_gap() {
        // cur's left edge (x=1) lies BEHIND prev's right edge (x=2) over the
        // shared band: the metric is signed and reports -1, it does not clamp to
        // zero the way `min_placed_distance` does.
        let prev = rect(0.0, 0.0, 2.0, 4.0);
        let cur = rect(1.0, 0.0, 5.0, 4.0);
        let gap = directional_pair_gap(&prev, &cur, GapAxis::HORIZONTAL);
        assert!((gap + 1.0).abs() < 1e-3, "overlap must be negative, got {gap}");
    }

    #[test]
    fn touching_band_without_area_is_infinite() {
        // The bands share exactly one coordinate (y = 4): a zero-width band is
        // NOT an overlap, so the pair is not measurable.
        let prev = rect(0.0, 0.0, 2.0, 4.0);
        let cur = rect(7.0, 4.0, 9.0, 8.0);
        let gap = directional_pair_gap(&prev, &cur, GapAxis::HORIZONTAL);
        assert!(gap.is_infinite(), "gap {gap}");
    }

    #[test]
    fn components_without_vertices_are_infinite() {
        // A contour with a component but no vertices has no band at all.
        let hollow = GlyphContour {
            components: vec![Vec::new()],
        }
        .placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        let real = rect(0.0, 0.0, 2.0, 4.0);
        assert!(directional_pair_gap(&hollow, &real, GapAxis::HORIZONTAL).is_infinite());
        assert!(directional_pair_gap(&real, &hollow, GapAxis::HORIZONTAL).is_infinite());
    }

    #[test]
    fn rotated_frame_measures_the_same_gap_as_the_unrotated_pair() {
        // A horizontal pair with a uniform gap of 5, then the SAME pair rotated
        // by 30 degrees and measured in its own rotated frame: the projected gap
        // is invariant, which is what makes the metric usable on a curve.
        let flat_gap = directional_pair_gap(
            &rect(0.0, 0.0, 2.0, 4.0),
            &rect(7.0, 0.0, 9.0, 4.0),
            GapAxis::HORIZONTAL,
        );
        assert!((flat_gap - 5.0).abs() < 1e-3, "flat gap {flat_gap}");

        let (sin, cos) = (30.0f32).to_radians().sin_cos();
        let prev = rotated_rect(0.0, 0.0, 2.0, 4.0, cos, sin);
        let cur = rotated_rect(7.0, 0.0, 9.0, 4.0, cos, sin);
        // The rotated advance direction is the image of +x; its perpendicular is
        // the image of +y.
        let axis = GapAxis::new([cos, sin], [-sin, cos]);
        let rotated_gap = directional_pair_gap(&prev, &cur, axis);
        assert!(
            (rotated_gap - 5.0).abs() < 1e-3,
            "rotated gap should match the flat gap, got {rotated_gap}"
        );
    }

    #[test]
    fn world_axis_frame_misreads_a_rotated_pair() {
        // The counterpart of the test above and the reason the frame is carried
        // as vectors: measuring the rotated pair on the WORLD x axis reports a
        // different number than the pair's own frame does.
        let (sin, cos) = (30.0f32).to_radians().sin_cos();
        let prev = rotated_rect(0.0, 0.0, 2.0, 4.0, cos, sin);
        let cur = rotated_rect(7.0, 0.0, 9.0, 4.0, cos, sin);
        let world_gap = directional_pair_gap(&prev, &cur, GapAxis::HORIZONTAL);
        assert!(
            (world_gap - 5.0).abs() > 0.5,
            "world-axis measure of a rotated pair should differ from 5, got {world_gap}"
        );
    }
}
