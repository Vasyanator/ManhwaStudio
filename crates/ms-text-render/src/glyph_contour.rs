/*
File: src/tabs/typing/render_next/glyph_contour.rs

Purpose:
Represent a glyph's ink boundary as one or more closed outer polygons so the
text-on-path engine can measure the true minimum distance between the shapes of
two adjacent glyphs, instead of using center-to-center spacing.

Main responsibilities:
- hold glyph-local closed outer contour(s) (produced upstream by
  `vector::glyph_contour_from_outline` from the true font outline);
- apply an affine placement transform and cache an AABB;
- compute the minimum Euclidean distance between two placed contours.

Key structures:
- GlyphContour: glyph-local closed outer contour(s).
- PlacedContour: GlyphContour after an affine transform, with a cached AABB.

Key functions:
- GlyphContour::placed
- GlyphContour::is_empty
- min_placed_distance

Notes:
- Contour vertices live in the glyph-local frame chosen by the producer; for the
  outline-derived contours used on-path this is the outline's pen-relative,
  y-down pixel space.
- The module is intentionally dependency-free (plain `[f32; 2]` math, no glam
  or egui types) so its geometry is directly unit-testable.
- `placed` and `min_placed_distance` never panic: empty contours yield a
  zero-AABB placement and an infinite distance respectively.
*/

/// Closed outer contour(s) of a glyph's ink in glyph-local pixel space.
///
/// One closed polygon per disjoint ink subpath; interior holes are discarded
/// (they never face a neighboring glyph). Vertices are ordered and the closing
/// edge (last -> first) is implicit: the first vertex is NOT duplicated at the
/// end. Each component polygon has at least three vertices.
#[derive(Debug, Clone, Default)]
pub struct GlyphContour {
    /// One closed polygon (list of vertices) per ink component.
    pub components: Vec<Vec<[f32; 2]>>,
}

/// A [`GlyphContour`] after an affine placement transform, with a cached AABB.
///
/// `aabb_min`/`aabb_max` bound every vertex of every component in world space.
/// When there are no components both are `[0.0, 0.0]`.
#[derive(Debug, Clone, Default)]
pub struct PlacedContour {
    /// Transformed component polygons in world space.
    pub components: Vec<Vec<[f32; 2]>>,
    /// Minimum corner of the world-space AABB over all vertices.
    pub aabb_min: [f32; 2],
    /// Maximum corner of the world-space AABB over all vertices.
    pub aabb_max: [f32; 2],
}

impl GlyphContour {
    /// Apply a local->world transform to every vertex and compute the AABB.
    ///
    /// Transform order is scale, then rotate, then translate:
    /// `world = Rot(cos, sin) * (Scale(scale_x, scale_y) * local) + (tx, ty)`,
    /// where `Rot(cos, sin) * [x, y] = [x*cos - y*sin, x*sin + y*cos]`.
    ///
    /// `cos`/`sin` are supplied directly (they are not required to be a unit
    /// vector; the caller owns that invariant). Returns a [`PlacedContour`]
    /// whose AABB bounds every transformed vertex, or a default (empty,
    /// zero-AABB) result when there are no components.
    #[must_use]
    pub fn placed(
        &self,
        cos: f32,
        sin: f32,
        scale_x: f32,
        scale_y: f32,
        tx: f32,
        ty: f32,
    ) -> PlacedContour {
        let mut placed = PlacedContour::default();
        let mut min = [f32::INFINITY, f32::INFINITY];
        let mut max = [f32::NEG_INFINITY, f32::NEG_INFINITY];

        for component in &self.components {
            let mut world = Vec::with_capacity(component.len());
            for vertex in component {
                let sx = vertex[0] * scale_x;
                let sy = vertex[1] * scale_y;
                let wx = sx * cos - sy * sin + tx;
                let wy = sx * sin + sy * cos + ty;
                min[0] = min[0].min(wx);
                min[1] = min[1].min(wy);
                max[0] = max[0].max(wx);
                max[1] = max[1].max(wy);
                world.push([wx, wy]);
            }
            placed.components.push(world);
        }

        if placed.components.is_empty() {
            return PlacedContour::default();
        }
        placed.aabb_min = min;
        placed.aabb_max = max;
        placed
    }

    /// Returns `true` when the contour has no components.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }
}

/// Minimum Euclidean distance between the ink boundaries of two placed glyphs.
///
/// Returns `0.0` if any two edges cross or touch (the shapes overlap). Returns
/// `f32::INFINITY` when either side has no components. Per-component AABB
/// rejection skips component pairs whose bounding boxes are already farther
/// apart than the current best distance.
#[must_use]
pub fn min_placed_distance(a: &PlacedContour, b: &PlacedContour) -> f32 {
    if a.components.is_empty() || b.components.is_empty() {
        return f32::INFINITY;
    }

    let boxes_a: Vec<([f32; 2], [f32; 2])> = a.components.iter().map(component_aabb).collect();
    let boxes_b: Vec<([f32; 2], [f32; 2])> = b.components.iter().map(component_aabb).collect();

    let mut best = f32::INFINITY;
    for (comp_a, box_a) in a.components.iter().zip(boxes_a.iter()) {
        for (comp_b, box_b) in b.components.iter().zip(boxes_b.iter()) {
            // Reject pairs whose AABB gap already exceeds the best distance.
            let gap = aabb_gap(*box_a, *box_b);
            if gap >= best {
                continue;
            }
            let d = polygons_min_distance(comp_a, comp_b);
            if d <= 0.0 {
                return 0.0;
            }
            best = best.min(d);
        }
    }
    best
}

/// Inclusive AABB of a polygon's vertices as `(min, max)`.
fn component_aabb(component: &Vec<[f32; 2]>) -> ([f32; 2], [f32; 2]) {
    let mut min = [f32::INFINITY, f32::INFINITY];
    let mut max = [f32::NEG_INFINITY, f32::NEG_INFINITY];
    for v in component {
        min[0] = min[0].min(v[0]);
        min[1] = min[1].min(v[1]);
        max[0] = max[0].max(v[0]);
        max[1] = max[1].max(v[1]);
    }
    (min, max)
}

/// Minimum gap between two AABBs (0.0 when they overlap or touch).
fn aabb_gap(a: ([f32; 2], [f32; 2]), b: ([f32; 2], [f32; 2])) -> f32 {
    let (a_min, a_max) = a;
    let (b_min, b_max) = b;
    let dx = (a_min[0] - b_max[0]).max(b_min[0] - a_max[0]).max(0.0);
    let dy = (a_min[1] - b_max[1]).max(b_min[1] - a_max[1]).max(0.0);
    (dx * dx + dy * dy).sqrt()
}

/// Minimum distance between the edges of two closed polygons.
///
/// Both polygons include their implicit closing edge (last -> first). Returns
/// `0.0` as soon as any edge pair intersects.
fn polygons_min_distance(poly_a: &[[f32; 2]], poly_b: &[[f32; 2]]) -> f32 {
    let mut best = f32::INFINITY;
    let na = poly_a.len();
    let nb = poly_b.len();
    if na < 2 || nb < 2 {
        // Degenerate polygon: fall back to point/point or point/edge distance.
        return degenerate_min_distance(poly_a, poly_b);
    }
    for i in 0..na {
        let a0 = poly_a[i];
        let a1 = poly_a[(i + 1) % na];
        for j in 0..nb {
            let b0 = poly_b[j];
            let b1 = poly_b[(j + 1) % nb];
            let d = segment_segment_distance(a0, a1, b0, b1);
            if d <= 0.0 {
                return 0.0;
            }
            best = best.min(d);
        }
    }
    best
}

/// Distance fallback when one polygon has fewer than two vertices.
fn degenerate_min_distance(poly_a: &[[f32; 2]], poly_b: &[[f32; 2]]) -> f32 {
    let mut best = f32::INFINITY;
    for a in poly_a {
        for b in poly_b {
            best = best.min(dist(*a, *b));
        }
    }
    best
}

/// Minimum Euclidean distance between two line segments.
///
/// Returns `0.0` when the segments intersect (crossing or touching). Otherwise
/// the minimum is one of the four endpoint-to-segment distances. Degenerate
/// (zero-length) segments are handled by [`point_segment_distance`].
fn segment_segment_distance(p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], p4: [f32; 2]) -> f32 {
    if segments_intersect(p1, p2, p3, p4) {
        return 0.0;
    }
    // No intersection: the closest approach is at an endpoint of one segment.
    point_segment_distance(p1, p3, p4)
        .min(point_segment_distance(p2, p3, p4))
        .min(point_segment_distance(p3, p1, p2))
        .min(point_segment_distance(p4, p1, p2))
}

/// Whether segments `p1p2` and `p3p4` intersect (including collinear overlap).
///
/// Uses signed-area orientation tests with the standard collinear special
/// cases so any touching or crossing configuration is reported.
fn segments_intersect(p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], p4: [f32; 2]) -> bool {
    let d1 = orientation(p3, p4, p1);
    let d2 = orientation(p3, p4, p2);
    let d3 = orientation(p1, p2, p3);
    let d4 = orientation(p1, p2, p4);

    // General case: each segment straddles the other's supporting line.
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
    {
        return true;
    }

    // Collinear/touching cases: an endpoint lies on the other segment.
    if d1 == 0.0 && on_segment(p3, p4, p1) {
        return true;
    }
    if d2 == 0.0 && on_segment(p3, p4, p2) {
        return true;
    }
    if d3 == 0.0 && on_segment(p1, p2, p3) {
        return true;
    }
    if d4 == 0.0 && on_segment(p1, p2, p4) {
        return true;
    }
    false
}

/// Signed area (z of cross product) of `(b - a) x (c - a)`.
///
/// Positive is counter-clockwise, negative clockwise, zero collinear.
fn orientation(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

/// Whether `p`, known to be collinear with `a`-`b`, lies within the segment.
fn on_segment(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> bool {
    p[0] >= a[0].min(b[0])
        && p[0] <= a[0].max(b[0])
        && p[1] >= a[1].min(b[1])
        && p[1] <= a[1].max(b[1])
}

/// Distance from point `p` to segment `a`-`b`.
///
/// Projects `p` onto the segment, clamps the parameter to `[0, 1]`, and returns
/// the distance to that clamped point. A zero-length segment returns the
/// distance to `a`.
fn point_segment_distance(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let abx = b[0] - a[0];
    let aby = b[1] - a[1];
    let len_sq = abx * abx + aby * aby;
    if len_sq <= f32::EPSILON {
        return dist(p, a);
    }
    let t = (((p[0] - a[0]) * abx) + ((p[1] - a[1]) * aby)) / len_sq;
    let t = t.clamp(0.0, 1.0);
    let proj = [a[0] + t * abx, a[1] + t * aby];
    dist(p, proj)
}

/// Euclidean distance between two points.
fn dist(a: [f32; 2], b: [f32; 2]) -> f32 {
    dist_sq(a, b).sqrt()
}

/// Squared Euclidean distance between two points.
fn dist_sq(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use super::{GlyphContour, min_placed_distance};

    /// Build an axis-aligned rectangle contour from inclusive corner coords.
    fn rect_contour(x0: f32, y0: f32, x1: f32, y1: f32) -> GlyphContour {
        GlyphContour {
            components: vec![vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]],
        }
    }

    #[test]
    fn placed_rect_aabb_covers_vertices() {
        // A rectangle from (2,1) to (7,5) placed at identity bounds exactly it.
        let contour = rect_contour(2.0, 1.0, 7.0, 5.0);
        let placed = contour.placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        assert!((placed.aabb_min[0] - 2.0).abs() < 1e-4);
        assert!((placed.aabb_min[1] - 1.0).abs() < 1e-4);
        assert!((placed.aabb_max[0] - 7.0).abs() < 1e-4);
        assert!((placed.aabb_max[1] - 5.0).abs() < 1e-4);
    }

    #[test]
    fn empty_contour_yields_infinite_distance() {
        let empty = GlyphContour::default();
        assert!(empty.is_empty());
        let placed_empty = empty.placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        // A real contour on the other side still returns INFINITY vs empty.
        let other = rect_contour(1.0, 1.0, 3.0, 3.0).placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        assert_eq!(min_placed_distance(&placed_empty, &other), f32::INFINITY);
        assert_eq!(min_placed_distance(&other, &placed_empty), f32::INFINITY);
    }

    #[test]
    fn known_gap_between_separated_rects() {
        // Left rect right edge at x=2, right rect left edge at x=7 -> gap 5.
        let left = rect_contour(1.0, 1.0, 2.0, 2.0).placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        let right = rect_contour(7.0, 1.0, 8.0, 2.0).placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        let d = min_placed_distance(&left, &right);
        assert!((d - 5.0).abs() < 1e-4, "distance was {d}");
    }

    #[test]
    fn overlapping_rects_have_zero_distance() {
        let a = rect_contour(1.0, 1.0, 5.0, 5.0).placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        let b = rect_contour(3.0, 3.0, 7.0, 7.0).placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        assert_eq!(min_placed_distance(&a, &b), 0.0);
    }

    #[test]
    fn two_disjoint_components_measure_nearest_pair() {
        // `a` has two blobs; the nearer one sits 3px left of `b`.
        let a = GlyphContour {
            components: vec![
                vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
                vec![[5.0, 0.0], [6.0, 0.0], [6.0, 1.0], [5.0, 1.0]],
            ],
        }
        .placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        let b = rect_contour(9.0, 0.0, 10.0, 1.0).placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        let d = min_placed_distance(&a, &b);
        assert!((d - 3.0).abs() < 1e-4, "distance was {d}");
    }

    #[test]
    fn placed_rotation_maps_corner() {
        // Single component with one identifiable corner vertex.
        let contour = GlyphContour {
            components: vec![vec![[2.0, 0.0], [3.0, 0.0], [3.0, 1.0]]],
        };
        // 90-degree rotation: cos=0, sin=1 -> [x,y] -> [-y, x].
        let placed = contour.placed(0.0, 1.0, 1.0, 1.0, 0.0, 0.0);
        let v = placed.components[0][0];
        // [2,0] -> [2*0 - 0*1, 2*1 + 0*0] = [0, 2].
        assert!((v[0] - 0.0).abs() < 1e-4, "x was {}", v[0]);
        assert!((v[1] - 2.0).abs() < 1e-4, "y was {}", v[1]);
    }
}
