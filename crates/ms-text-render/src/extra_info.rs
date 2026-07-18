/*
File: crates/ms-text-render/src/extra_info.rs

Purpose:
Pure geometry core for the renderer's optional "extra render info" payload
(mean/median glyph centers). It collects per-glyph placement-box samples in
CONTENT space, then reduces them to the requested centers and maps them into
final-image pixels. It knows nothing about fonts, layout, or rasterization.

Main responsibilities:
- accumulate per-glyph corner/center samples only for the requested metrics;
- compute the convex-hull area centroid (mean center) with a point-average
  fallback for degenerate hulls;
- compute the per-axis median (median center);
- translate accumulated content-space samples (e.g. through a mesh warp) and map
  the finished centers into canvas pixels via a content->canvas offset.

Key structures:
- ExtraInfoAccumulator

Key functions:
- ExtraInfoAccumulator::new / is_active / add_glyph / map_points / finish
- convex_hull (monotone chain)
- polygon_area_centroid
- per_axis_median

Notes:
Coordinates are the same content-space units the horizontal draw pass uses; the
`x_offset`/`y_offset` passed to `finish` are exactly the pass's content->canvas
translation, so `canvas = content + offset`. A pure translation commutes with
both the hull-centroid and the median, so the mapping is exact.
*/

use crate::types::{RenderExtraInfoRequest, RenderedTextExtraInfo};

/// Collects per-glyph placement samples and reduces them to the requested
/// mean/median centers.
///
/// Constructed from a [`RenderExtraInfoRequest`]. When the request is inactive
/// (nothing requested) every method is a true no-op: `add_glyph` stores nothing,
/// `map_points` does nothing, and `finish` returns
/// [`RenderedTextExtraInfo::default`]. Samples are held in CONTENT space; the
/// content->canvas mapping happens once in [`Self::finish`].
#[derive(Debug)]
pub(crate) struct ExtraInfoAccumulator {
    request: RenderExtraInfoRequest,
    /// Placement-box corner samples feeding the convex-hull mean center. Empty
    /// unless `request.mean_center` is set.
    hull_points: Vec<[f32; 2]>,
    /// Per-corner warp flag parallel to `hull_points`: [`Self::map_points`]
    /// transforms a corner only when its flag is `true`. Color-glyph BITMAP-fallback
    /// glyphs push `false` because their raster pixels are drawn UNWARPED.
    hull_warpable: Vec<bool>,
    /// Placement-box center samples feeding the per-axis median center. Empty
    /// unless `request.median_center` is set.
    center_points: Vec<[f32; 2]>,
    /// Per-center warp flag parallel to `center_points` (see `hull_warpable`).
    center_warpable: Vec<bool>,
}

impl ExtraInfoAccumulator {
    /// Build an accumulator for `request`. Constructing an inactive accumulator
    /// allocates nothing and makes every later call a no-op.
    #[must_use]
    pub(crate) fn new(request: RenderExtraInfoRequest) -> Self {
        Self {
            request,
            hull_points: Vec::new(),
            hull_warpable: Vec::new(),
            center_points: Vec::new(),
            center_warpable: Vec::new(),
        }
    }

    /// `true` when at least one metric is requested. Callers gate all per-glyph
    /// sampling on this so the fast path stays allocation- and branch-free.
    #[must_use]
    pub(crate) fn is_active(&self) -> bool {
        self.request.is_active()
    }

    /// Feed one included glyph's placement box: its four `corners` (any winding
    /// order) and its `center`, all in content space. `warpable` records whether the
    /// glyph's pixels are drawn through the mesh warp: outline glyphs pass `true`,
    /// while color-glyph BITMAP-fallback glyphs pass `false` because their raster
    /// pixels are drawn UNWARPED — [`Self::map_points`] then leaves their samples in
    /// place so the samples stay aligned with the pixels. Only the samples the active
    /// metrics need are stored. A no-op when the accumulator is inactive.
    pub(crate) fn add_glyph(&mut self, corners: [[f32; 2]; 4], center: [f32; 2], warpable: bool) {
        if self.request.mean_center {
            self.hull_points.extend_from_slice(&corners);
            // One flag per corner keeps `hull_warpable` parallel to `hull_points`.
            self.hull_warpable.extend_from_slice(&[warpable; 4]);
        }
        if self.request.median_center {
            self.center_points.push(center);
            self.center_warpable.push(warpable);
        }
    }

    /// Apply a point transform (e.g. a mesh warp) to every stored WARPABLE
    /// content-space sample. Samples added with `warpable == false` (color-glyph
    /// bitmap fallbacks, whose pixels are drawn unwarped) are left untouched so they
    /// stay aligned with their pixels. Used to move the raw samples into the same
    /// post-warp content frame the rasterizer draws in before `finish` maps to canvas
    /// pixels. A no-op when the accumulator is inactive.
    pub(crate) fn map_points(&mut self, f: impl Fn([f32; 2]) -> [f32; 2]) {
        if !self.is_active() {
            return;
        }
        for (point, &warpable) in self.hull_points.iter_mut().zip(self.hull_warpable.iter()) {
            if warpable {
                *point = f(*point);
            }
        }
        for (point, &warpable) in self
            .center_points
            .iter_mut()
            .zip(self.center_warpable.iter())
        {
            if warpable {
                *point = f(*point);
            }
        }
    }

    /// Reduce the collected samples to the requested centers and map them from
    /// content space to final-image pixels by adding `(x_offset, y_offset)` —
    /// the same content->canvas translation the draw pass uses. A requested
    /// metric with no contributing glyph is `None`.
    #[must_use]
    pub(crate) fn finish(&self, x_offset: f32, y_offset: f32) -> RenderedTextExtraInfo {
        let mean_center = if self.request.mean_center {
            polygon_area_centroid(&self.hull_points)
                .map(|[cx, cy]| [cx + x_offset, cy + y_offset])
        } else {
            None
        };
        let median_center = if self.request.median_center {
            per_axis_median(&self.center_points).map(|[cx, cy]| [cx + x_offset, cy + y_offset])
        } else {
            None
        };
        RenderedTextExtraInfo {
            mean_center,
            median_center,
        }
    }
}

/// Content-space placement-box `(corners, center)` of a glyph whose SCALED box of
/// size `width` x `height` is centered at `(center_x, center_y)` and rotated by
/// `rotation_rad` about that center.
///
/// Uses the shared screen (y-down) `[cos -sin; sin cos]` rotation convention, the
/// same one every draw path applies for its block / group / global rotation, so the
/// sampled box matches the exact box the rasterizer draws. Corners are emitted
/// top-left -> top-right -> bottom-right -> bottom-left, which is visually CLOCKWISE
/// in y-down screen space (it is the counter-clockwise winding of a y-up math frame).
/// The exact winding does not matter downstream: the signed-area centroid in
/// `polygon_area_centroid` handles either orientation. Shared by the rotated draw
/// paths (vertical block rotation and the formula/on-path group+global rotation).
#[must_use]
pub(crate) fn rotated_box_samples(
    center_x: f32,
    center_y: f32,
    width: f32,
    height: f32,
    rotation_rad: f32,
) -> ([[f32; 2]; 4], [f32; 2]) {
    let (sin_a, cos_a) = rotation_rad.sin_cos();
    let half_w = width * 0.5;
    let half_h = height * 0.5;
    let local = [
        [-half_w, -half_h],
        [half_w, -half_h],
        [half_w, half_h],
        [-half_w, half_h],
    ];
    let corners = local.map(|[lx, ly]| {
        [
            center_x + lx * cos_a - ly * sin_a,
            center_y + lx * sin_a + ly * cos_a,
        ]
    });
    (corners, [center_x, center_y])
}

/// Area centroid of the convex hull of `points`, or `None` when `points` is empty.
///
/// Non-degenerate hulls (positive area) return the polygon area centroid. A
/// degenerate hull (a single point or all points collinear, so the enclosed area
/// is ~0) falls back to the arithmetic mean of the input points, which is the
/// only stable center in that case.
#[must_use]
fn polygon_area_centroid(points: &[[f32; 2]]) -> Option<[f32; 2]> {
    if points.is_empty() {
        return None;
    }
    let hull = convex_hull(points);
    // A hull with fewer than 3 vertices has zero area (single point or a segment).
    if hull.len() >= 3 {
        // Signed area and area-weighted centroid via the shoelace formula, in f64
        // to keep the 1/(6A) division stable for thin polygons.
        let mut area2 = 0.0f64; // twice the signed area
        let mut cx = 0.0f64;
        let mut cy = 0.0f64;
        for i in 0..hull.len() {
            let [x0, y0] = hull[i];
            let [x1, y1] = hull[(i + 1) % hull.len()];
            let (x0, y0, x1, y1) = (f64::from(x0), f64::from(y0), f64::from(x1), f64::from(y1));
            let cross = x0 * y1 - x1 * y0;
            area2 += cross;
            cx += (x0 + x1) * cross;
            cy += (y0 + y1) * cross;
        }
        if area2.abs() > f64::EPSILON {
            let inv = 1.0 / (3.0 * area2); // 1/(6A) with A = area2/2
            // Intentional f64->f32 at the contract boundary; magnitudes are
            // image-plane pixels, well within f32 range.
            return Some([(cx * inv) as f32, (cy * inv) as f32]);
        }
    }
    // Degenerate hull: average of all sample points.
    point_average(points)
}

/// Arithmetic mean of `points`, or `None` when empty.
#[must_use]
fn point_average(points: &[[f32; 2]]) -> Option<[f32; 2]> {
    if points.is_empty() {
        return None;
    }
    let mut sum_x = 0.0f64;
    let mut sum_y = 0.0f64;
    for [x, y] in points {
        sum_x += f64::from(*x);
        sum_y += f64::from(*y);
    }
    // `points.len()` is a glyph-sample count, far below f64's 2^53 integer-exact range.
    let count = points.len() as f64;
    // Intentional f64->f32 at the contract boundary; magnitudes are image-plane
    // pixels, well within f32 range.
    Some([(sum_x / count) as f32, (sum_y / count) as f32])
}

/// Per-axis median of `points`, or `None` when empty.
///
/// The x and y medians are computed independently. For an even count each axis
/// returns the average of the two middle values; for an odd count the middle
/// value. NaN coordinates sort to one end but never panic (total order via
/// `f32::total_cmp`).
#[must_use]
fn per_axis_median(points: &[[f32; 2]]) -> Option<[f32; 2]> {
    if points.is_empty() {
        return None;
    }
    let mut xs: Vec<f32> = points.iter().map(|p| p[0]).collect();
    let mut ys: Vec<f32> = points.iter().map(|p| p[1]).collect();
    Some([median_of(&mut xs), median_of(&mut ys)])
}

/// Median of `values` (mutated in place by sorting). `values` must be non-empty.
/// Even count -> average of the two middle values; odd count -> middle value.
#[must_use]
fn median_of(values: &mut [f32]) -> f32 {
    values.sort_by(f32::total_cmp);
    let len = values.len();
    let mid = len / 2;
    if len % 2 == 1 {
        values[mid]
    } else {
        // Average the two central values; halves avoid an intermediate overflow.
        values[mid - 1] * 0.5 + values[mid] * 0.5
    }
}

/// Convex hull of `points` via the monotone-chain (Andrew) algorithm.
///
/// Returns the hull vertices in the monotone-chain winding: counter-clockwise in a
/// y-up math frame, which is visually CLOCKWISE in screen y-down. The exact
/// orientation is irrelevant to callers because `polygon_area_centroid`'s signed-area
/// shoelace handles either winding. Duplicate and collinear points are removed;
/// fewer than three unique points return the unique points themselves (a single
/// point or a segment), signaling a degenerate hull to the caller.
#[must_use]
fn convex_hull(points: &[[f32; 2]]) -> Vec<[f32; 2]> {
    let mut pts: Vec<[f32; 2]> = points.to_vec();
    // Lexicographic sort by (x, y) using a total order so NaN never panics.
    pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }

    // 2D cross product of OA x OB; > 0 = counter-clockwise turn in a y-up math frame
    // (a visually clockwise turn in screen y-down).
    fn cross(o: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f64 {
        let ox = f64::from(o[0]);
        let oy = f64::from(o[1]);
        (f64::from(a[0]) - ox) * (f64::from(b[1]) - oy)
            - (f64::from(a[1]) - oy) * (f64::from(b[0]) - ox)
    }

    let mut hull: Vec<[f32; 2]> = Vec::with_capacity(pts.len() + 1);
    // Lower hull.
    for &p in &pts {
        while hull.len() >= 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    // Upper hull; `lower` marks how many lower-hull points to keep untouched.
    let lower = hull.len() + 1;
    for &p in pts.iter().rev() {
        while hull.len() >= lower && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    // The last point duplicates the first.
    hull.pop();
    hull
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RenderExtraInfoRequest;

    const EPS: f32 = 1e-3;

    fn approx(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() <= EPS && (a[1] - b[1]).abs() <= EPS
    }

    #[test]
    fn hull_centroid_of_axis_aligned_square_is_its_center() {
        // A dense point set inside a square still hulls to the square; the area
        // centroid is the square center regardless of interior points.
        let pts = [
            [0.0, 0.0],
            [4.0, 0.0],
            [4.0, 4.0],
            [0.0, 4.0],
            [2.0, 2.0],
            [1.0, 3.0],
        ];
        let c = polygon_area_centroid(&pts).expect("non-empty");
        assert!(approx(c, [2.0, 2.0]), "got {c:?}");
    }

    #[test]
    fn hull_centroid_ignores_concave_interior_points() {
        // Concave/interior points must not pull the AREA centroid: the hull of
        // this L-shaped point cloud is the bounding triangle's convex hull.
        // Outer points form a right triangle (0,0)-(6,0)-(0,6); interior points
        // inside it do not change the hull, so the centroid is the triangle's.
        let pts = [
            [0.0, 0.0],
            [6.0, 0.0],
            [0.0, 6.0],
            [1.0, 1.0],
            [2.0, 1.0],
            [1.0, 2.0],
        ];
        let c = polygon_area_centroid(&pts).expect("non-empty");
        // Triangle centroid = average of the three vertices = (2, 2).
        assert!(approx(c, [2.0, 2.0]), "got {c:?}");
    }

    #[test]
    fn hull_centroid_single_point_is_that_point() {
        let pts = [[3.5, -2.0]];
        let c = polygon_area_centroid(&pts).expect("non-empty");
        assert!(approx(c, [3.5, -2.0]), "got {c:?}");
    }

    #[test]
    fn hull_centroid_collinear_points_average() {
        // Collinear points have zero hull area -> point-average fallback.
        let pts = [[0.0, 0.0], [2.0, 0.0], [4.0, 0.0]];
        let c = polygon_area_centroid(&pts).expect("non-empty");
        assert!(approx(c, [2.0, 0.0]), "got {c:?}");
    }

    #[test]
    fn hull_centroid_empty_is_none() {
        assert!(polygon_area_centroid(&[]).is_none());
    }

    #[test]
    fn median_odd_count_is_middle_value() {
        let pts = [[1.0, 10.0], [5.0, 30.0], [3.0, 20.0]];
        let m = per_axis_median(&pts).expect("non-empty");
        assert!(approx(m, [3.0, 20.0]), "got {m:?}");
    }

    #[test]
    fn median_even_count_averages_two_middle() {
        // x sorted: 1,3,5,9 -> (3+5)/2 = 4 ; y sorted: 0,2,4,6 -> (2+4)/2 = 3
        let pts = [[9.0, 6.0], [1.0, 0.0], [5.0, 4.0], [3.0, 2.0]];
        let m = per_axis_median(&pts).expect("non-empty");
        assert!(approx(m, [4.0, 3.0]), "got {m:?}");
    }

    #[test]
    fn median_single_point() {
        let pts = [[-7.0, 8.0]];
        let m = per_axis_median(&pts).expect("non-empty");
        assert!(approx(m, [-7.0, 8.0]), "got {m:?}");
    }

    #[test]
    fn median_empty_is_none() {
        assert!(per_axis_median(&[]).is_none());
    }

    #[test]
    fn inactive_accumulator_is_noop() {
        let mut acc = ExtraInfoAccumulator::new(RenderExtraInfoRequest::default());
        assert!(!acc.is_active());
        acc.add_glyph([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]], [0.5, 0.5], true);
        acc.map_points(|p| [p[0] + 100.0, p[1] + 100.0]);
        assert_eq!(acc.finish(5.0, 5.0), RenderedTextExtraInfo::default());
    }

    #[test]
    fn accumulator_offset_maps_content_to_canvas() {
        let req = RenderExtraInfoRequest {
            mean_center: true,
            median_center: true,
        };
        let mut acc = ExtraInfoAccumulator::new(req);
        // Two unit squares centered at (0.5,0.5) and (2.5,0.5).
        acc.add_glyph([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]], [0.5, 0.5], true);
        acc.add_glyph([[2.0, 0.0], [3.0, 0.0], [3.0, 1.0], [2.0, 1.0]], [2.5, 0.5], true);
        let out = acc.finish(10.0, 20.0);
        // Hull is the rectangle (0,0)-(3,1); centroid (1.5, 0.5) + offset.
        let mean = out.mean_center.expect("mean requested");
        assert!(approx(mean, [11.5, 20.5]), "got {mean:?}");
        // Median centers: xs 0.5,2.5 -> 1.5 ; ys 0.5,0.5 -> 0.5 ; + offset.
        let median = out.median_center.expect("median requested");
        assert!(approx(median, [11.5, 20.5]), "got {median:?}");
    }

    #[test]
    fn map_points_translates_before_finish() {
        let req = RenderExtraInfoRequest {
            mean_center: true,
            median_center: true,
        };
        let mut acc = ExtraInfoAccumulator::new(req);
        acc.add_glyph([[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]], [1.0, 1.0], true);
        acc.map_points(|p| [p[0] + 3.0, p[1] - 1.0]);
        let out = acc.finish(0.0, 0.0);
        let mean = out.mean_center.expect("mean requested");
        assert!(approx(mean, [4.0, 0.0]), "got {mean:?}");
        let median = out.median_center.expect("median requested");
        assert!(approx(median, [4.0, 0.0]), "got {median:?}");
    }

    #[test]
    fn map_points_leaves_non_warpable_samples_untouched() {
        // A warpable glyph at center (1,1) and a non-warpable (bitmap-fallback) glyph
        // at center (5,1). `map_points` must move only the warpable one; the
        // non-warpable box must stay where the unwarped pixels were drawn.
        let req = RenderExtraInfoRequest {
            mean_center: true,
            median_center: true,
        };
        let mut acc = ExtraInfoAccumulator::new(req);
        acc.add_glyph([[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]], [1.0, 1.0], true);
        acc.add_glyph([[4.0, 0.0], [6.0, 0.0], [6.0, 2.0], [4.0, 2.0]], [5.0, 1.0], false);
        // Translate every warpable sample by +100 in x; non-warpable stays.
        acc.map_points(|p| [p[0] + 100.0, p[1]]);
        let out = acc.finish(0.0, 0.0);
        // Median of xs {101, 5} -> 53 ; ys {1,1} -> 1.
        let median = out.median_center.expect("median requested");
        assert!(approx(median, [53.0, 1.0]), "got {median:?}");
        // Hull corners: warpable box shifted to x in [100,102], non-warpable box left
        // at x in [4,6]; the hull is the rectangle x:[4,102], y:[0,2], centroid (53,1).
        let mean = out.mean_center.expect("mean requested");
        assert!(approx(mean, [53.0, 1.0]), "got {mean:?}");
    }

    #[test]
    fn rotated_box_samples_unrotated_is_axis_aligned() {
        // Zero rotation: the corners are the plain axis-aligned box around the center.
        let (corners, center) = rotated_box_samples(10.0, 20.0, 4.0, 2.0, 0.0);
        assert!(approx(center, [10.0, 20.0]), "center {center:?}");
        assert!(approx(corners[0], [8.0, 19.0]), "tl {:?}", corners[0]);
        assert!(approx(corners[2], [12.0, 21.0]), "br {:?}", corners[2]);
    }

    #[test]
    fn rotated_box_samples_quarter_turn_swaps_extents() {
        // A 90° turn maps the box half-width onto the y axis and half-height onto x
        // (screen y-down `[cos -sin; sin cos]`); the center is unchanged.
        let (corners, center) =
            rotated_box_samples(0.0, 0.0, 4.0, 2.0, std::f32::consts::FRAC_PI_2);
        assert!(approx(center, [0.0, 0.0]), "center {center:?}");
        // Hull of the four corners must be the swapped 2x4 box.
        let c = polygon_area_centroid(&corners).expect("non-empty");
        assert!(approx(c, [0.0, 0.0]), "rotated centroid {c:?}");
        let max_x = corners.iter().map(|p| p[0]).fold(f32::MIN, f32::max);
        let max_y = corners.iter().map(|p| p[1]).fold(f32::MIN, f32::max);
        assert!((max_x - 1.0).abs() <= EPS, "half-height onto x: {max_x}");
        assert!((max_y - 2.0).abs() <= EPS, "half-width onto y: {max_y}");
    }

    #[test]
    fn only_requested_metric_is_computed() {
        let req = RenderExtraInfoRequest {
            mean_center: true,
            median_center: false,
        };
        let mut acc = ExtraInfoAccumulator::new(req);
        acc.add_glyph([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]], [0.5, 0.5], true);
        let out = acc.finish(0.0, 0.0);
        assert!(out.mean_center.is_some());
        assert!(out.median_center.is_none());
    }
}
