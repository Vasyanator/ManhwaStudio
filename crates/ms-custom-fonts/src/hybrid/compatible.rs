/*
File: crates/ms-custom-fonts/src/hybrid/compatible.rs

Purpose:
The COMPATIBLE fast path of the outline blend: direct weighted interpolation of corresponding
points when every participating outline has the same structure.

Key functions:
- `interpolate`: `Some(blended outline)` when every outline can be put in point-to-point
  correspondence with the first one, `None` otherwise (the caller then takes the
  distance-field path).

Notes:
- Contours are first normalized: the implicit closing line is made explicit and zero-length
  lines are dropped, so a TrueType and a CFF contour of the same polygon (e.g. an "H") have
  the same segments.
- Correspondence: same contour count; each reference contour is paired with the unused
  contour of the other outline that has the same segment-kind cycle (line / quad / cubic, so
  quadratic and cubic curves never pair) and the nearest node centroid; the other contour is
  then rotated cyclically (start node) to the kind-preserving rotation with the smallest sum
  of squared node distances. Unrelated designs of one letter often share such a structure
  ("H", "I", "L", many CJK strokes) and interpolate exactly this way.
- Plausibility: corresponding contours must have the same orientation, and every pair of
  corresponding segments longer than `MIN_DIRECTION_LENGTH` must point within 60 degrees of
  each other; a twisted correspondence fails this and falls back to the distance field.
- Exact and cheap (microseconds per glyph); output is a pure function of the inputs.
*/

use kurbo::Point;

use crate::outline::{Contour, Outline, Seg};

/// Segments shorter than this (font units) are not used by the direction check.
const MIN_DIRECTION_LENGTH: f64 = 8.0;
/// Corresponding segment directions must have at least this cosine (60 degrees).
const MIN_DIRECTION_COS: f64 = 0.5;

/// Interpolates `outlines` with `weights` (same length, summing to 1) when all correspond.
pub(crate) fn interpolate(outlines: &[&Outline], weights: &[f64]) -> Option<Outline> {
    let normalized: Vec<Outline> = outlines.iter().map(|outline| outline.iter().map(normalize).collect()).collect();
    let (reference, others) = normalized.split_first()?;
    let mut aligned: Vec<Outline> = Vec::with_capacity(normalized.len());
    aligned.push(reference.clone());
    for other in others {
        aligned.push(correspond(reference, other)?);
    }
    let blend = |pick: &dyn Fn(&Outline) -> Point| -> Point {
        let mut x = 0.0;
        let mut y = 0.0;
        for (outline, weight) in aligned.iter().zip(weights) {
            let point = pick(outline);
            x += weight * point.x;
            y += weight * point.y;
        }
        Point::new(x, y)
    };
    let blended = reference
        .iter()
        .enumerate()
        .map(|(contour_index, contour)| Contour {
            start: blend(&|outline| outline[contour_index].start),
            segs: contour
                .segs
                .iter()
                .enumerate()
                .map(|(seg_index, seg)| {
                    let at = |slot: usize| blend(&|outline| seg_point(&outline[contour_index].segs[seg_index], slot));
                    match seg {
                        Seg::Line(_) => Seg::Line(at(0)),
                        Seg::Quad(..) => Seg::Quad(at(0), at(1)),
                        Seg::Cubic(..) => Seg::Cubic(at(0), at(1), at(2)),
                    }
                })
                .collect(),
        })
        .collect();
    Some(blended)
}

/// Point `slot` of a segment (0-based; the end point is the last slot). Callers only ask for
/// slots the segment kind has, which the correspondence guarantees across outlines.
fn seg_point(seg: &Seg, slot: usize) -> Point {
    match *seg {
        Seg::Line(end) => end,
        Seg::Quad(control, end) => {
            if slot == 0 {
                control
            } else {
                end
            }
        }
        Seg::Cubic(control1, control2, end) => match slot {
            0 => control1,
            1 => control2,
            _ => end,
        },
    }
}

/// Explicit closing line, no zero-length lines.
fn normalize(contour: &Contour) -> Contour {
    let mut segs: Vec<Seg> = Vec::with_capacity(contour.segs.len() + 1);
    let mut current = contour.start;
    for seg in &contour.segs {
        if let Seg::Line(end) = seg
            && *end == current
        {
            continue;
        }
        current = seg.end();
        segs.push(*seg);
    }
    if current != contour.start {
        segs.push(Seg::Line(contour.start));
    }
    Contour { start: contour.start, segs }
}

/// Nodes of a normalized contour: node `i` is where segment `i` starts.
fn nodes(contour: &Contour) -> Vec<Point> {
    std::iter::once(contour.start).chain(contour.segs.iter().map(Seg::end)).take(contour.segs.len()).collect()
}

/// `other` reordered and rotated into correspondence with `reference`, if plausible.
fn correspond(reference: &Outline, other: &Outline) -> Option<Outline> {
    if reference.len() != other.len() {
        return None;
    }
    let other_centroids: Vec<Point> = other.iter().map(centroid).collect();
    let mut used = vec![false; other.len()];
    let mut result = Vec::with_capacity(reference.len());
    for contour in reference {
        let kinds: Vec<u8> = contour.segs.iter().map(Seg::kind).collect();
        let center = centroid(contour);
        let mut best: Option<(f64, usize, Contour)> = None;
        for (index, candidate) in other.iter().enumerate() {
            if used[index] || candidate.segs.len() != kinds.len() {
                continue;
            }
            let Some(rotated) = best_rotation(contour, &kinds, candidate) else {
                continue;
            };
            let distance = center.distance_squared(other_centroids[index]);
            if best.as_ref().is_none_or(|(best_distance, _, _)| distance < *best_distance) {
                best = Some((distance, index, rotated));
            }
        }
        let (_, index, rotated) = best?;
        if !plausible(contour, &rotated) {
            return None;
        }
        used[index] = true;
        result.push(rotated);
    }
    Some(result)
}

/// The kind-preserving cyclic rotation of `candidate` closest to `contour` (sum of squared
/// node distances; ties keep the smaller rotation), or `None` when no rotation preserves the
/// kind cycle.
fn best_rotation(contour: &Contour, kinds: &[u8], candidate: &Contour) -> Option<Contour> {
    let count = kinds.len();
    let candidate_kinds: Vec<u8> = candidate.segs.iter().map(Seg::kind).collect();
    let reference_nodes = nodes(contour);
    let candidate_nodes = nodes(candidate);
    let mut best: Option<(f64, usize)> = None;
    for rotation in 0..count {
        if (0..count).any(|index| kinds[index] != candidate_kinds[(index + rotation) % count]) {
            continue;
        }
        let cost: f64 = (0..count).map(|index| reference_nodes[index].distance_squared(candidate_nodes[(index + rotation) % count])).sum();
        if best.is_none_or(|(best_cost, _)| cost < best_cost) {
            best = Some((cost, rotation));
        }
    }
    let (_, rotation) = best?;
    let segs: Vec<Seg> = (0..count).map(|index| candidate.segs[(index + rotation) % count]).collect();
    Some(Contour { start: candidate_nodes[rotation], segs })
}

/// Same orientation, and corresponding segments point the same way.
fn plausible(a: &Contour, b: &Contour) -> bool {
    let (nodes_a, nodes_b) = (nodes(a), nodes(b));
    if signed_area(&nodes_a) * signed_area(&nodes_b) < 0.0 {
        return false;
    }
    a.segs.iter().zip(&b.segs).enumerate().all(|(index, (seg_a, seg_b))| {
        let direction_a = seg_a.end() - nodes_a[index];
        let direction_b = seg_b.end() - nodes_b[index];
        let (length_a, length_b) = (direction_a.hypot(), direction_b.hypot());
        length_a < MIN_DIRECTION_LENGTH
            || length_b < MIN_DIRECTION_LENGTH
            || direction_a.dot(direction_b) >= MIN_DIRECTION_COS * length_a * length_b
    })
}

/// Mean of the on-curve nodes.
fn centroid(contour: &Contour) -> Point {
    let points = nodes(contour);
    let count = f64::from(u32::try_from(points.len()).unwrap_or(u32::MAX).max(1));
    let (sum_x, sum_y) = points.iter().fold((0.0, 0.0), |(x, y), point| (x + point.x, y + point.y));
    Point::new(sum_x / count, sum_y / count)
}

/// Shoelace area of a node polygon (sign = orientation).
fn signed_area(points: &[Point]) -> f64 {
    let count = points.len();
    (0..count)
        .map(|index| {
            let a = points[index];
            let b = points[(index + 1) % count];
            a.x * b.y - b.x * a.y
        })
        .sum::<f64>()
        * 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clockwise rectangle starting at corner `start_corner`, closed explicitly or not.
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64, start_corner: usize, explicit_close: bool) -> Contour {
        let corners = [Point::new(x0, y0), Point::new(x0, y1), Point::new(x1, y1), Point::new(x1, y0)];
        let ordered: Vec<Point> = (0..4).map(|offset| corners[(start_corner + offset) % 4]).collect();
        let mut segs: Vec<Seg> = ordered[1..].iter().map(|&point| Seg::Line(point)).collect();
        if explicit_close {
            segs.push(Seg::Line(ordered[0]));
        }
        Contour { start: ordered[0], segs }
    }

    #[test]
    fn same_polygon_with_other_start_and_closing_interpolates() {
        let a = vec![rect(0.0, 0.0, 100.0, 700.0, 0, true)];
        let b = vec![rect(0.0, 0.0, 200.0, 700.0, 2, false)];
        let blended = interpolate(&[&a, &b], &[0.5, 0.5]).expect("corresponds after rotation");
        let corners = nodes(&blended[0]);
        assert_eq!(corners, vec![Point::new(0.0, 0.0), Point::new(0.0, 700.0), Point::new(150.0, 700.0), Point::new(150.0, 0.0)]);
    }

    #[test]
    fn contours_pair_by_position_not_by_order() {
        let a = vec![rect(0.0, 0.0, 100.0, 100.0, 0, true), rect(500.0, 0.0, 600.0, 100.0, 0, true)];
        let b = vec![rect(520.0, 0.0, 620.0, 100.0, 0, true), rect(20.0, 0.0, 120.0, 100.0, 0, true)];
        let blended = interpolate(&[&a, &b], &[0.5, 0.5]).expect("pairs by centroid");
        assert_eq!(blended[0].start, Point::new(10.0, 0.0));
        assert_eq!(blended[1].start, Point::new(510.0, 0.0));
    }

    #[test]
    fn opposite_orientation_or_other_kinds_are_rejected() {
        let a = vec![rect(0.0, 0.0, 100.0, 700.0, 0, true)];
        let reversed = vec![crate::outline::reverse_contour(&rect(0.0, 0.0, 100.0, 700.0, 0, true))];
        assert!(interpolate(&[&a, &reversed], &[0.5, 0.5]).is_none());
        let curved = vec![Contour {
            start: Point::new(0.0, 0.0),
            segs: vec![
                Seg::Quad(Point::new(0.0, 350.0), Point::new(0.0, 700.0)),
                Seg::Line(Point::new(100.0, 700.0)),
                Seg::Line(Point::new(100.0, 0.0)),
                Seg::Line(Point::new(0.0, 0.0)),
            ],
        }];
        assert!(interpolate(&[&a, &curved], &[0.5, 0.5]).is_none());
    }
}
