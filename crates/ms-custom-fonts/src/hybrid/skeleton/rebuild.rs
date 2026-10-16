/*
File: crates/ms-custom-fonts/src/hybrid/skeleton/rebuild.rs

Purpose:
Turns the blended strokes back into an outline: the distance field of the union of their
tapered capsules on a grid, its zero level set (`sdf::marching_squares`) and the curve fit
(`trace::fit_loops`).

Key functions:
- `rebuild`: strokes + cell -> outline, or a `SkeletonFallback`.

Notes:
- A stroke segment from `a` (radius `ra`) to `b` (radius `rb`) is the union of the discs along
  it; its signed distance is `min over s (|p - c(s)| - r(s))`, solved in closed form (the
  minimizer of a convex function of the arc parameter), so tapering corner spurs reconstruct
  their corners instead of being shrunk by a projection approximation.
- Free ends with a `Cap` add, per side, a quadrilateral filling the corner between the round
  cap and the square cut, so flat terminals stay flat (the medial axis of a flat end has no
  corner branches after thinning).
- Only nodes within the segment's box grown by its larger radius plus `FIELD_MARGIN_CELLS` are
  evaluated; the others keep `FAR_FIELD`. Every node next to the zero level set lies in some
  box, so the level set is exact. Work is bounded by `MAX_TOTAL_SEGMENTS` x the box of the
  largest radius (`MAX_RADIUS_UNITS`).
*/

use kurbo::{Point, Vec2};

use super::geometry::{Cap, Stroke, clamp_index, exceeds, segment_distance2};
use super::{MAX_TOTAL_SEGMENTS, SkeletonFallback};
use crate::hybrid::sdf::{Bounds, ExtentTooLarge, checked_bounds, grid_for, marching_squares};
use crate::hybrid::trace;
use crate::outline::Outline;

/// Field value of nodes no capsule box reaches (far outside).
const FAR_FIELD: f64 = 1.0e9;
/// Extra cells around each capsule box whose nodes are evaluated.
const FIELD_MARGIN_CELLS: f64 = 2.0;

/// The outline of the union of `strokes` (aligned coordinates) at grid cell `cell`.
///
/// # Errors
/// `TooManyStrokes` beyond `MAX_TOTAL_SEGMENTS` segments; `Extent` when the strokes span more
/// than the distance-field limits (or are not finite); `NoSkeleton` when nothing is left.
pub(super) fn rebuild(strokes: &[Stroke], cell: f64) -> Result<Outline, SkeletonFallback> {
    let segments: usize = strokes.iter().map(|stroke| stroke.points.len().saturating_sub(1).max(1)).sum();
    if segments > MAX_TOTAL_SEGMENTS {
        return Err(SkeletonFallback::TooManyStrokes { count: segments });
    }
    let mut corners = Vec::new();
    for stroke in strokes {
        for (point, radius) in stroke.points.iter().zip(&stroke.radii) {
            // A cap's square corner lies up to radius x sqrt(2) from its end point.
            let reach = radius * std::f64::consts::SQRT_2;
            corners.push(Point::new(point.x - reach, point.y - reach));
            corners.push(Point::new(point.x + reach, point.y + reach));
        }
    }
    let Some(bounds) = checked_bounds(&[corners]).map_err(SkeletonFallback::Extent)? else {
        return Err(SkeletonFallback::NoSkeleton);
    };
    let grid = grid_for(bounds, cell).ok_or(SkeletonFallback::Extent(ExtentTooLarge::of(bounds)))?;
    let mut field = vec![FAR_FIELD; grid.nx * grid.ny];
    for stroke in strokes {
        let pieces: Vec<(Point, f64, Point, f64)> = if stroke.points.len() == 1 {
            vec![(stroke.points[0], stroke.radii[0], stroke.points[0], stroke.radii[0])]
        } else {
            (1..stroke.points.len()).map(|k| (stroke.points[k - 1], stroke.radii[k - 1], stroke.points[k], stroke.radii[k])).collect()
        };
        for (a, radius_a, b, radius_b) in pieces {
            let reach = radius_a.max(radius_b) + FIELD_MARGIN_CELLS * grid.cell;
            let area = Bounds { x0: a.x.min(b.x) - reach, y0: a.y.min(b.y) - reach, x1: a.x.max(b.x) + reach, y1: a.y.max(b.y) + reach };
            let i0 = clamp_index((area.x0 - grid.x0) / grid.cell, grid.nx);
            let i1 = clamp_index((area.x1 - grid.x0) / grid.cell, grid.nx);
            let j0 = clamp_index((area.y0 - grid.y0) / grid.cell, grid.ny);
            let j1 = clamp_index((area.y1 - grid.y0) / grid.cell, grid.ny);
            for j in j0..=j1 {
                let y = grid.y(j);
                for i in i0..=i1 {
                    let node = Point::new(grid.x(i), y);
                    let slot = j * grid.nx + i;
                    field[slot] = field[slot].min(capsule_distance(node, a, radius_a, b, radius_b));
                }
            }
        }
    }
    for stroke in strokes {
        for (end, cap) in stroke.caps.iter().enumerate() {
            let Some(cap) = cap else {
                continue;
            };
            for quad in cap_quads(stroke, end, *cap, grid.cell) {
                let reach = FIELD_MARGIN_CELLS * grid.cell;
                let x0 = quad.iter().map(|point| point.x).fold(f64::INFINITY, f64::min) - reach;
                let x1 = quad.iter().map(|point| point.x).fold(f64::NEG_INFINITY, f64::max) + reach;
                let y0 = quad.iter().map(|point| point.y).fold(f64::INFINITY, f64::min) - reach;
                let y1 = quad.iter().map(|point| point.y).fold(f64::NEG_INFINITY, f64::max) + reach;
                for j in clamp_index((y0 - grid.y0) / grid.cell, grid.ny)..=clamp_index((y1 - grid.y0) / grid.cell, grid.ny) {
                    let y = grid.y(j);
                    for i in clamp_index((x0 - grid.x0) / grid.cell, grid.nx)..=clamp_index((x1 - grid.x0) / grid.cell, grid.nx) {
                        let slot = j * grid.nx + i;
                        field[slot] = field[slot].min(convex_distance(Point::new(grid.x(i), y), &quad));
                    }
                }
            }
        }
    }
    let loops = marching_squares(&field, &grid);
    let outline = trace::fit_loops(loops, grid.cell);
    if outline.is_empty() {
        return Err(SkeletonFallback::NoSkeleton);
    }
    Ok(outline)
}

/// The corner fills of a free end: per side with squareness `s > 0`, the quadrilateral centre,
/// side point, corner (pulled from the round cap's 45-degree point to the square corner by
/// `s`), tip point. Its union with the end disc is the round cap at `s = 0` and the square cut
/// at `s = 1`.
fn cap_quads(stroke: &Stroke, end: usize, cap: Cap, cell: f64) -> Vec<[Point; 4]> {
    let (Some(&tip), Some(&radius)) = (if end == 0 { stroke.points.first() } else { stroke.points.last() }, if end == 0 { stroke.radii.first() } else { stroke.radii.last() }) else {
        return Vec::new();
    };
    let ordered: Vec<Point> = if end == 0 { stroke.points.clone() } else { stroke.points.iter().rev().copied().collect() };
    let Some(inner) = ordered.iter().skip(1).find(|point| point.distance(tip) >= cell).or_else(|| ordered.get(1)) else {
        return Vec::new();
    };
    let direction = tip - *inner;
    let length = direction.hypot();
    if !exceeds(length, 0.0) || !exceeds(radius, 0.0) {
        return Vec::new();
    }
    let outward = direction / length;
    let left = Vec2::new(-outward.y, outward.x);
    let mut quads = Vec::with_capacity(2);
    for (normal, squareness) in [(left, cap.left), (-left, cap.right)] {
        if !exceeds(squareness, 0.0) {
            continue;
        }
        let reach = std::f64::consts::FRAC_1_SQRT_2 + squareness.min(1.0) * (1.0 - std::f64::consts::FRAC_1_SQRT_2);
        quads.push([tip, tip + normal * radius, tip + (outward + normal) * (radius * reach), tip + outward * radius]);
    }
    quads
}

/// Signed distance (negative inside) from `point` to a convex quadrilateral (either winding).
fn convex_distance(point: Point, quad: &[Point; 4]) -> f64 {
    let mut nearest2 = f64::INFINITY;
    let (mut positive, mut negative) = (false, false);
    for (position, &a) in quad.iter().enumerate() {
        let b = quad[(position + 1) % 4];
        nearest2 = nearest2.min(segment_distance2(point, a, b));
        let cross = (b - a).cross(point - a);
        positive |= cross > 0.0;
        negative |= cross < 0.0;
    }
    let distance = nearest2.sqrt();
    if positive && negative { distance } else { -distance }
}

/// Signed distance from `point` to the union of the discs centred on segment `a`-`b` with
/// radius varying linearly from `radius_a` to `radius_b`.
pub(super) fn capsule_distance(point: Point, a: Point, radius_a: f64, b: Point, radius_b: f64) -> f64 {
    let axis = b - a;
    let length = axis.hypot();
    if !exceeds(length, 0.0) {
        return point.distance(a) - radius_a.max(radius_b);
    }
    let direction = axis / length;
    let relative = point - a;
    let along = relative.dot(direction);
    let across = (relative - direction * along).hypot();
    let slope = (radius_b - radius_a) / length;
    let at = |s: f64| ((along - s).powi(2) + across * across).sqrt() - (radius_a + slope * s);
    if slope.abs() >= 1.0 {
        // One end disc contains the other: the union is that disc.
        return at(0.0).min(at(length));
    }
    // d/ds (|p - c(s)| - r(s)) = 0  <=>  (along - s) / sqrt((along - s)^2 + across^2) = -slope.
    let best = (along + slope * across / (1.0 - slope * slope).sqrt()).clamp(0.0, length);
    at(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capsule_distance_matches_dense_disc_sampling() {
        let (a, b) = (Point::new(0.0, 0.0), Point::new(100.0, 20.0));
        for (radius_a, radius_b) in [(10.0, 10.0), (30.0, 2.0), (1.0, 40.0)] {
            for step in 0..200 {
                let point = Point::new(-60.0 + f64::from(step % 20) * 11.0, -60.0 + f64::from(step / 20) * 13.0);
                let dense = (0..=2000)
                    .map(|k| {
                        let t = f64::from(k) / 2000.0;
                        point.distance(a.lerp(b, t)) - (radius_a + (radius_b - radius_a) * t)
                    })
                    .fold(f64::INFINITY, f64::min);
                let exact = capsule_distance(point, a, radius_a, b, radius_b);
                assert!((exact - dense).abs() < 0.05, "{point:?} r=({radius_a},{radius_b}) exact {exact} dense {dense}");
            }
        }
    }
}
