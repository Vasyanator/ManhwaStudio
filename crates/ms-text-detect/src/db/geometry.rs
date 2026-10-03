/*
File: crates/ms-text-detect/src/db/geometry.rs

Purpose:
The OpenCV / pyclipper geometry the detector ports need to match their Python references to the
pixel: `cv2.minAreaRect` + `cv2.boxPoints` (float rotating calipers over the convex hull; the
crate's ONE float minimum-area-rectangle fit, used by the CTD DB representer and by the Surya
postprocess) and pyclipper's `JT_ROUND` closed-polygon offset on the integer grid (CTD only).

Key functions:
- `min_area_rect_f()` (crate-visible): float minimum-area rectangle of integer points, four
  consecutive corners clockwise on screen. Caller-specific post-steps stay in the callers
  (Surya: near-square axis box and corner roll; CTD: offset and refit).
- `round_offset()`: Clipper `ClipperOffset` with `JT_ROUND` (ArcTolerance 0.25) of a convex
  polygon whose float corners are truncated toward zero first; integer output points.

Notes:
`round_offset` is used only by `DbParams::opencv_geometry` (the CTD preset); `min_area_rect_f`
also by `surya.rs`. The Paddle preset keeps imageproc's
integer `min_area_rect` and the rotated-rectangle expansion, pinned by `db.rs` characterization
tests. Clipper reference: Clipper 6.4 `ClipperOffset::DoOffset` / `OffsetPoint` / `DoRound`
(`Round` = half away from zero, normals `(dy, -dx)`, polygon orientation fixed so they point
outward). The minimum-area rectangle keeps the FIRST hull edge on ties; OpenCV's calipers may
pick another orientation of the same area, which only reorders identical rectangles.
*/

use std::f64::consts::PI;

/// Pyclipper's default arc tolerance (`def_arc_tolerance`).
const ARC_TOLERANCE: f64 = 0.25;

/// Convex hull of integer points (Andrew's monotone chain), counter-clockwise in a y-up frame,
/// collinear points dropped. Fewer than three input points come back deduplicated.
fn convex_hull(points: &[[i64; 2]]) -> Vec<[i64; 2]> {
    let mut pts = points.to_vec();
    pts.sort_unstable();
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let cross = |o: [i64; 2], a: [i64; 2], b: [i64; 2]| (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
    let chain = |ordered: &mut dyn Iterator<Item = [i64; 2]>| {
        let mut out: Vec<[i64; 2]> = Vec::new();
        for p in ordered {
            while out.len() >= 2 && cross(out[out.len() - 2], out[out.len() - 1], p) <= 0 {
                out.pop();
            }
            out.push(p);
        }
        // The last point of each chain is the first point of the other one.
        out.pop();
        out
    };
    let mut hull = chain(&mut pts.iter().copied());
    hull.extend(chain(&mut pts.iter().rev().copied()));
    hull
}

/// Minimum-area rectangle of integer points (`cv2.minAreaRect` + `cv2.boxPoints`) as four
/// consecutive float corners, clockwise on screen (y down).
///
/// Rotating calipers over the hull edges: each edge defines a frame and the smallest
/// frame-aligned bounding rectangle wins (the first hull edge on an equal area). An empty input
/// yields `None`, one distinct point four equal corners, two a degenerate rectangle (zero width).
/// Callers round the corners to `f32` themselves (`OpenCV`'s `boxPoints` are float32).
pub(crate) fn min_area_rect_f(points: &[[i64; 2]]) -> Option<[[f64; 2]; 4]> {
    let hull = convex_hull(points);
    let to_f = |p: [i64; 2]| [i64_f64(p[0]), i64_f64(p[1])];
    match hull.len() {
        0 => return None,
        1 => return Some([to_f(hull[0]); 4]),
        _ => {}
    }
    let mut best: Option<(f64, [[f64; 2]; 4])> = None;
    for edge in 0..hull.len() {
        let origin = to_f(hull[edge]);
        let next = to_f(hull[(edge + 1) % hull.len()]);
        let (dx, dy) = (next[0] - origin[0], next[1] - origin[1]);
        let len = dx.hypot(dy);
        if len == 0.0 {
            continue;
        }
        // `across` is `along` turned +90 degrees, so (along, across) is a rotation of (x, y) and
        // the corner order below stays clockwise on screen whatever the hull orientation.
        let along = [dx / len, dy / len];
        let across = [-along[1], along[0]];
        let (mut along_min, mut along_max, mut across_min, mut across_max) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
        for &p in &hull {
            let rel = [i64_f64(p[0]) - origin[0], i64_f64(p[1]) - origin[1]];
            let (pa, pc) = (rel[0] * along[0] + rel[1] * along[1], rel[0] * across[0] + rel[1] * across[1]);
            along_min = along_min.min(pa);
            along_max = along_max.max(pa);
            across_min = across_min.min(pc);
            across_max = across_max.max(pc);
        }
        let area = (along_max - along_min) * (across_max - across_min);
        if best.as_ref().is_none_or(|(best_area, _)| area < *best_area) {
            let corner = |ca: f64, cc: f64| [origin[0] + along[0] * ca + across[0] * cc, origin[1] + along[1] * ca + across[1] * cc];
            best = Some((area, [corner(along_min, across_min), corner(along_max, across_min), corner(along_max, across_max), corner(along_min, across_max)]));
        }
    }
    best.map(|(_, rect)| rect)
}

/// Clipper `Round`: half away from zero.
fn clipper_round(value: f64) -> i64 {
    let rounded = if value < 0.0 { (value - 0.5).trunc() } else { (value + 0.5).trunc() };
    #[expect(clippy::cast_possible_truncation, reason = "integral pixel coordinate far inside the i64 range")]
    let out = rounded as i64;
    out
}

/// Pyclipper `PyclipperOffset().AddPath(poly, JT_ROUND, ET_CLOSEDPOLYGON); Execute(delta)` for a
/// convex polygon and `delta > 0`: the corners are truncated toward zero (pyclipper's float
/// input), duplicates removed, the orientation fixed, then every vertex gets a round join.
/// Returns the integer output path; fewer than three distinct vertices yield `None`.
pub(super) fn round_offset(poly: &[[f64; 2]], delta: f64) -> Option<Vec<[i64; 2]>> {
    let mut src: Vec<[f64; 2]> = Vec::with_capacity(poly.len());
    for p in poly {
        let q = [p[0].trunc(), p[1].trunc()];
        if src.last() != Some(&q) {
            src.push(q);
        }
    }
    while src.len() > 1 && src.first() == src.last() {
        src.pop();
    }
    if src.len() < 3 || delta <= 0.0 {
        return None;
    }
    // Shoelace sum > 0 in image coordinates makes the `(dy, -dx)` normals point outward.
    let count = src.len();
    let shoelace: f64 = (0..count).map(|i| src[i][0] * src[(i + 1) % count][1] - src[(i + 1) % count][0] * src[i][1]).sum();
    if shoelace < 0.0 {
        src.reverse();
    }
    let normals: Vec<[f64; 2]> = (0..count)
        .map(|i| {
            let (from, to) = (src[i], src[(i + 1) % count]);
            let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
            let inv_len = 1.0 / dx.hypot(dy);
            [dy * inv_len, -dx * inv_len]
        })
        .collect();
    let tolerance = if ARC_TOLERANCE > delta * ARC_TOLERANCE { delta * ARC_TOLERANCE } else { ARC_TOLERANCE };
    let mut steps = PI / (1.0 - tolerance / delta).acos();
    if steps > delta * PI {
        steps = delta * PI;
    }
    let (step_sin, step_cos) = (2.0 * PI / steps).sin_cos();
    let steps_per_rad = steps / (2.0 * PI);
    let offset_point = |vertex: [f64; 2], normal: [f64; 2]| [clipper_round(vertex[0] + normal[0] * delta), clipper_round(vertex[1] + normal[1] * delta)];
    let mut out = Vec::new();
    // Clipper's OffsetPoint(j, k): `prev` is the previous edge's normal index, `cur` this vertex.
    let mut prev = count - 1;
    for cur in 0..count {
        let (n_prev, n_cur) = (normals[prev], normals[cur]);
        let vertex = src[cur];
        let mut sin_a = n_prev[0] * n_cur[1] - n_cur[0] * n_prev[1];
        let cos_a = n_prev[0] * n_cur[0] + n_prev[1] * n_cur[1];
        prev = cur;
        if (sin_a * delta).abs() < 1.0 {
            if cos_a > 0.0 {
                out.push(offset_point(vertex, n_cur));
                continue;
            }
        } else {
            sin_a = sin_a.clamp(-1.0, 1.0);
        }
        if sin_a * delta < 0.0 {
            out.push(offset_point(vertex, n_prev));
            out.push([clipper_round(vertex[0]), clipper_round(vertex[1])]);
            out.push(offset_point(vertex, n_cur));
        } else {
            // DoRound: rotate from the previous edge's normal to this edge's normal.
            let arc_steps = clipper_round(steps_per_rad * sin_a.atan2(cos_a).abs()).max(1);
            let mut normal = n_prev;
            for _ in 0..arc_steps {
                out.push(offset_point(vertex, normal));
                normal = [normal[0] * step_cos - step_sin * normal[1], normal[0] * step_sin + normal[1] * step_cos];
            }
            out.push(offset_point(vertex, n_cur));
        }
    }
    Some(out)
}

/// `i64` coordinate to `f64` (pixel coordinates, exact).
fn i64_f64(value: i64) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "pixel coordinates stay far below 2^53")]
    let out = value as f64;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_aligned_points_give_their_exact_box() {
        let pts = [[49, 221], [91, 221], [91, 230], [49, 230], [60, 225]];
        let rect = min_area_rect_f(&pts).unwrap_or_else(|| panic!("rect"));
        let mut xs: Vec<f64> = rect.iter().map(|p| p[0]).collect();
        let mut ys: Vec<f64> = rect.iter().map(|p| p[1]).collect();
        xs.sort_by(f64::total_cmp);
        ys.sort_by(f64::total_cmp);
        assert_eq!((xs[0], xs[3], ys[0], ys[3]), (49.0, 91.0, 221.0, 230.0));
        assert!(min_area_rect_f(&[]).is_none());
        assert_eq!(min_area_rect_f(&[[3, 4], [3, 4]]), Some([[3.0, 4.0]; 4]));
    }

    #[test]
    fn rotated_points_give_a_clockwise_rectangle_of_minimum_area() {
        // A 45-degree diamond: the axis box (area 8 x 8 = 64) loses to the rotated square of area
        // 32; the corners come back clockwise on screen (positive shoelace in image coordinates).
        let pts = [[4, 0], [8, 4], [4, 8], [0, 4], [4, 4]];
        let rect = min_area_rect_f(&pts).unwrap_or_else(|| panic!("rect"));
        let edge = |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).hypot(a[1] - b[1]);
        assert!((edge(rect[0], rect[1]) * edge(rect[1], rect[2]) - 32.0).abs() < 1e-9, "{rect:?}");
        let shoelace: f64 = (0..4).map(|i| rect[i][0] * rect[(i + 1) % 4][1] - rect[(i + 1) % 4][0] * rect[i][1]).sum();
        assert!(shoelace > 0.0, "{rect:?}");
        let mut corners: Vec<[i64; 2]> = rect.iter().map(|p| [clipper_round(p[0]), clipper_round(p[1])]).collect();
        corners.sort_unstable();
        assert_eq!(corners, vec![[0, 4], [4, 0], [4, 8], [8, 4]]);
    }

    #[test]
    fn round_offset_reproduces_the_pyclipper_fixture_path() {
        // dark_on_light candidate: mini box 49..91 x 221..230, distance = 378 * 1.5 / 102.
        let poly = [[49.0, 221.0], [91.0, 221.0], [91.0, 230.0], [49.0, 230.0]];
        let mut got = round_offset(&poly, 378.0 * 1.5 / 102.0).unwrap_or_else(|| panic!("path"));
        let mut want = vec![[94, 216], [96, 219], [97, 221], [97, 230], [96, 233], [93, 235], [91, 236], [49, 236], [46, 235], [44, 232], [43, 230], [43, 221], [44, 218], [47, 216], [49, 215], [91, 215]];
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want);
    }
}
