/*
File: crates/ms-custom-fonts/src/hybrid/matched.rs

Purpose:
The MATCHED path of the outline blend, between the exact compatible path and the distance
field: for glyphs whose designs share their topology (same number of contours, pairable one to
one) but not their point structure, it finds a point correspondence along each contour pair
and interpolates it, so strokes MOVE to their weighted position instead of fading as they do
in a distance-field blend.

Key functions:
- `interpolate`: `Some(blended outline)` or `None` (no plausible correspondence).

Notes:
- Frame: every source is scaled horizontally by `blended advance / own advance` (the natural
  frame of the distance-field path); contours are flattened at `FLATTEN_TOLERANCE`.
- Pairing: same contour count in every source; the reference (highest weight, first on ties)
  pairs each contour, largest first, with the unused contour of the same orientation whose
  centroid is nearest.
- Correspondence per pair: both contours are sampled every `MATCH_SPACING` units along their
  arc length; features are positions normalized by the contour's own centroid and
  sqrt(|area|) (so a small and a large counter still match) plus the unit tangent. The cyclic
  start is chosen on `COARSE_SAMPLES` uniform samples (least squares), then dynamic time
  warping inside a Sakoe-Chiba band aligns the samples; every reference arc position gets the
  mean matched arc position of the other contour.
- Blend: the reference contour is resampled densely (its own vertices kept, gaps of at most
  `BLEND_SPACING`), each point is blended with its matched points, and the loop is fitted by
  `trace::fit_loops`.
- Plausibility (else `None`): every blended contour keeps the reference orientation and at
  least `MIN_AREA_RATIO` of the smallest paired area, and no blended edges cross (within or
  between contours). This rejects twisted or mis-paired correspondences.
- Pure function of its inputs; basic arithmetic and `sqrt`/`hypot` only.
*/

use kurbo::{Point, Vec2};

use crate::hybrid::trace;
use crate::outline::{Outline, flatten_contour};

/// Flattening tolerance of the source contours, font units.
const FLATTEN_TOLERANCE: f64 = 0.2;
/// Arc spacing of the correspondence samples, font units.
const MATCH_SPACING: f64 = 6.0;
/// Largest gap between consecutive blended points, font units.
const BLEND_SPACING: f64 = 3.0;
/// Uniform samples of the cyclic start search.
const COARSE_SAMPLES: usize = 64;
/// Half-width of the dynamic-time-warping band as a fraction of the other contour's samples.
const BAND_FRACTION: f64 = 0.2;
/// Weight of the tangent term against the normalized squared distance.
const TANGENT_COST: f64 = 0.5;
/// A blended contour must keep at least this share of the smallest paired source area.
const MIN_AREA_RATIO: f64 = 0.2;
/// Contours with an absolute area below this (square units) are flattening debris.
const MIN_CONTOUR_AREA: f64 = 1.0;
/// Cell size of the edge-crossing test's spatial hash, font units.
const CROSSING_CELL: f64 = 16.0;
/// Corner-window scale handed to the curve fitter (the blended points' spacing).
const FIT_CELL: f64 = BLEND_SPACING;

/// A closed polyline with cumulative arc lengths.
#[derive(Debug, Clone)]
struct Ring {
    points: Vec<Point>,
    /// `cum[i]` = arc length from point 0 to point `i`; `cum.len() == points.len() + 1`, the
    /// last entry is the full perimeter (back to point 0).
    cum: Vec<f64>,
    area: f64,
    centroid: Point,
}

impl Ring {
    fn new(points: Vec<Point>) -> Option<Self> {
        if points.len() < 3 {
            return None;
        }
        let count = points.len();
        let mut cum = Vec::with_capacity(count + 1);
        cum.push(0.0);
        for index in 0..count {
            let next = points[(index + 1) % count];
            cum.push(cum[index] + points[index].distance(next));
        }
        let area = signed_area(&points);
        if area.abs() < MIN_CONTOUR_AREA || cum[count] <= 0.0 {
            return None;
        }
        let centroid = points.iter().fold(Vec2::ZERO, |sum, point| sum + point.to_vec2()) / f64::from(u32::try_from(count).ok()?);
        Some(Self { points, cum, area, centroid: centroid.to_point() })
    }

    fn length(&self) -> f64 {
        self.cum[self.cum.len() - 1]
    }

    /// Point at arc position `s` (wrapped into `[0, length)`).
    fn at(&self, s: f64) -> Point {
        let length = self.length();
        let s = s.rem_euclid(length);
        // Last segment start whose cumulative length is <= s.
        let index = self.cum.partition_point(|&value| value <= s).saturating_sub(1).min(self.points.len() - 1);
        let a = self.points[index];
        let b = self.points[(index + 1) % self.points.len()];
        let span = self.cum[index + 1] - self.cum[index];
        if span > 0.0 { a.lerp(b, (s - self.cum[index]) / span) } else { a }
    }

    /// Unit tangent at arc position `s`, measured over `±reach`.
    fn tangent(&self, s: f64, reach: f64) -> Vec2 {
        let vector = self.at(s + reach) - self.at(s - reach);
        let length = vector.hypot();
        if length > 0.0 { vector / length } else { Vec2::ZERO }
    }

    /// Position normalized by the ring's centroid and size.
    fn normalized(&self, point: Point) -> Vec2 {
        (point - self.centroid) / self.area.abs().sqrt()
    }
}

/// Blends same-topology outlines by contour correspondence; see the file header.
pub(crate) fn interpolate(outlines: &[&Outline], weights: &[f64], advances: &[f64]) -> Option<Outline> {
    let target_advance: f64 = advances.iter().zip(weights).map(|(advance, weight)| advance * weight).sum();
    let sources: Vec<Vec<Ring>> = outlines
        .iter()
        .zip(advances)
        .map(|(outline, &advance)| {
            let scale = if advance >= 1.0 && target_advance >= 1.0 { target_advance / advance } else { 1.0 };
            let mut polygon = Vec::new();
            outline
                .iter()
                .filter_map(|contour| {
                    flatten_contour(contour, FLATTEN_TOLERANCE, &mut polygon);
                    Ring::new(polygon.iter().map(|point| Point::new(point.x * scale, point.y)).collect())
                })
                .collect()
        })
        .collect();
    let count = sources.first()?.len();
    if count == 0 || sources.iter().any(|rings| rings.len() != count) {
        return None;
    }
    // Reference: highest weight, first on ties.
    let reference = (0..weights.len()).fold(0, |best, index| if weights[index] > weights[best] { index } else { best });
    let pairings: Vec<Vec<usize>> = sources.iter().map(|rings| pair_rings(&sources[reference], rings)).collect::<Option<_>>()?;

    let mut blended_rings: Vec<Vec<Point>> = Vec::with_capacity(count);
    for (contour, reference_ring) in sources[reference].iter().enumerate() {
        let params = dense_params(reference_ring);
        let mut blended = vec![Point::ZERO; params.len()];
        let mut smallest_area = reference_ring.area.abs();
        for (source, rings) in sources.iter().enumerate() {
            let weight = weights[source];
            let ring = &rings[pairings[source][contour]];
            smallest_area = smallest_area.min(ring.area.abs());
            let mapped: Vec<Point> = if source == reference {
                params.iter().map(|&s| reference_ring.at(s)).collect()
            } else {
                let mapping = correspond(reference_ring, ring)?;
                params.iter().map(|&s| ring.at(mapping.map(s))).collect()
            };
            for (point, source_point) in blended.iter_mut().zip(&mapped) {
                *point = (point.to_vec2() + source_point.to_vec2() * weight).to_point();
            }
        }
        let area = signed_area(&blended);
        if area * reference_ring.area <= 0.0 || area.abs() < MIN_AREA_RATIO * smallest_area {
            return None;
        }
        blended_rings.push(blended);
    }
    if edges_cross(&blended_rings) {
        return None;
    }
    Some(trace::fit_loops(blended_rings, FIT_CELL))
}

/// For each reference ring, the index of its partner in `rings` (same orientation, nearest
/// centroid; largest reference rings pick first). `None` when some ring has no partner.
fn pair_rings(reference: &[Ring], rings: &[Ring]) -> Option<Vec<usize>> {
    let mut order: Vec<usize> = (0..reference.len()).collect();
    order.sort_by(|&a, &b| reference[b].area.abs().total_cmp(&reference[a].area.abs()).then(a.cmp(&b)));
    let mut partner = vec![usize::MAX; reference.len()];
    let mut used = vec![false; rings.len()];
    for index in order {
        let ring = &reference[index];
        let best = rings
            .iter()
            .enumerate()
            .filter(|(candidate, other)| !used[*candidate] && other.area * ring.area > 0.0)
            .min_by(|(a_index, a), (b_index, b)| {
                ring.centroid
                    .distance_squared(a.centroid)
                    .total_cmp(&ring.centroid.distance_squared(b.centroid))
                    .then(a_index.cmp(b_index))
            })
            .map(|(candidate, _)| candidate)?;
        used[best] = true;
        partner[index] = best;
    }
    Some(partner)
}

/// Arc positions of the dense reference resampling: every vertex, plus evenly spaced points
/// on edges longer than `BLEND_SPACING`.
fn dense_params(ring: &Ring) -> Vec<f64> {
    let mut params = Vec::with_capacity(ring.points.len() * 2);
    for index in 0..ring.points.len() {
        let (start, end) = (ring.cum[index], ring.cum[index + 1]);
        params.push(start);
        let pieces = ((end - start) / BLEND_SPACING).ceil();
        let mut piece = 1.0;
        while piece < pieces {
            params.push(start + (end - start) * piece / pieces);
            piece += 1.0;
        }
    }
    params
}

/// Monotone map from reference arc positions to positions on the other ring.
struct ArcMap {
    /// Reference arc step between coarse samples.
    step: f64,
    /// Matched (unwrapped) arc position on the other ring per coarse reference sample,
    /// `samples + 1` entries (the last closes the loop).
    matched: Vec<f64>,
}

impl ArcMap {
    /// Other-ring arc position for reference arc position `s` (linear between samples).
    fn map(&self, s: f64) -> f64 {
        let position = s / self.step;
        let index = floor_index(position, self.matched.len() - 2);
        let t = (position - as_f64(index)).clamp(0.0, 1.0);
        self.matched[index] + (self.matched[index + 1] - self.matched[index]) * t
    }
}

/// Number of samples for a ring of `length` at `spacing` (8..=4096).
fn sample_count(length: f64, spacing: f64) -> usize {
    floor_index((length / spacing).ceil(), 4096).max(8)
}

/// `floor(value)` as an index clamped to `0..=max` (non-finite -> 0).
fn floor_index(value: f64, max: usize) -> usize {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let limit = as_f64(max);
    let floored = value.floor().min(limit);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..=max (a small usize) just above"
    )]
    let index = floored as usize;
    index
}

/// `usize` -> `f64` for small counts.
fn as_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

/// Dynamic-time-warping correspondence from `reference` onto `other`.
fn correspond(reference: &Ring, other: &Ring) -> Option<ArcMap> {
    let (length_a, length_b) = (reference.length(), other.length());
    // Cyclic start on the other ring: least squares over uniform fractions.
    let coarse = |ring: &Ring, start: f64, index: usize| {
        ring.normalized(ring.at(start + ring.length() * as_f64(index) / as_f64(COARSE_SAMPLES)))
    };
    let reference_coarse: Vec<Vec2> = (0..COARSE_SAMPLES).map(|index| coarse(reference, 0.0, index)).collect();
    let other_coarse: Vec<Vec2> = (0..COARSE_SAMPLES).map(|index| coarse(other, 0.0, index)).collect();
    let shift = (0..COARSE_SAMPLES)
        .map(|shift| {
            let cost: f64 = (0..COARSE_SAMPLES)
                .map(|index| (reference_coarse[index] - other_coarse[(index + shift) % COARSE_SAMPLES]).hypot2())
                .sum();
            (cost, shift)
        })
        .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
        .map(|(_, shift)| shift)?;
    let start_b = length_b * as_f64(shift) / as_f64(COARSE_SAMPLES);

    let samples_a = sample_count(length_a, MATCH_SPACING);
    let samples_b = sample_count(length_b, MATCH_SPACING);
    let (step_a, step_b) = (length_a / as_f64(samples_a), length_b / as_f64(samples_b));
    let features = |ring: &Ring, start: f64, step: f64, samples: usize| -> Vec<(Vec2, Vec2)> {
        (0..=samples)
            .map(|index| {
                let s = start + step * as_f64(index);
                (ring.normalized(ring.at(s)), ring.tangent(s, MATCH_SPACING))
            })
            .collect()
    };
    let feature_a = features(reference, 0.0, step_a, samples_a);
    let feature_b = features(other, start_b, step_b, samples_b);
    let cost = |i: usize, j: usize| {
        let (position_a, tangent_a) = feature_a[i];
        let (position_b, tangent_b) = feature_b[j];
        (position_a - position_b).hypot2() + TANGENT_COST * (1.0 - tangent_a.dot(tangent_b))
    };

    // Banded DTW over (samples_a + 1) x (samples_b + 1), from (0, 0) to the closing corner.
    let rows = samples_a + 1;
    let columns = samples_b + 1;
    let band = (BAND_FRACTION * as_f64(columns)).ceil().max(4.0);
    let mut table = vec![f64::INFINITY; rows * columns];
    for i in 0..rows {
        let center = as_f64(i) * as_f64(samples_b) / as_f64(samples_a);
        for j in 0..columns {
            if (as_f64(j) - center).abs() > band {
                continue;
            }
            let previous = if i == 0 && j == 0 {
                0.0
            } else {
                let up = if i > 0 { table[(i - 1) * columns + j] } else { f64::INFINITY };
                let left = if j > 0 { table[i * columns + j - 1] } else { f64::INFINITY };
                let diagonal = if i > 0 && j > 0 { table[(i - 1) * columns + j - 1] } else { f64::INFINITY };
                up.min(left).min(diagonal)
            };
            if previous.is_finite() {
                table[i * columns + j] = previous + cost(i, j);
            }
        }
    }
    if !table[rows * columns - 1].is_finite() {
        return None;
    }
    // Backtrack; collect the matched columns of every row.
    let mut sums = vec![0.0_f64; rows];
    let mut counts = vec![0_u32; rows];
    let (mut i, mut j) = (rows - 1, columns - 1);
    loop {
        sums[i] += as_f64(j);
        counts[i] += 1;
        if i == 0 && j == 0 {
            break;
        }
        let up = if i > 0 { table[(i - 1) * columns + j] } else { f64::INFINITY };
        let left = if j > 0 { table[i * columns + j - 1] } else { f64::INFINITY };
        let diagonal = if i > 0 && j > 0 { table[(i - 1) * columns + j - 1] } else { f64::INFINITY };
        if diagonal <= up && diagonal <= left {
            i -= 1;
            j -= 1;
        } else if up <= left {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    let matched = sums
        .iter()
        .zip(&counts)
        .map(|(&sum, &count)| start_b + step_b * sum / f64::from(count.max(1)))
        .collect();
    Some(ArcMap { step: step_a, matched })
}

/// Shoelace area (negative = clockwise).
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

/// Whether any two non-adjacent edges of the rings cross (spatial hash of `CROSSING_CELL`).
fn edges_cross(rings: &[Vec<Point>]) -> bool {
    // (ring, index) of every edge, bucketed by the hash cells its bounding box touches.
    let mut buckets: std::collections::HashMap<(i64, i64), Vec<(usize, usize)>> = std::collections::HashMap::new();
    let cell_of = |value: f64| -> i64 {
        let cell = (value / CROSSING_CELL).floor();
        if !cell.is_finite() {
            return 0;
        }
        #[expect(clippy::cast_possible_truncation, reason = "clamped far inside the i64 range just below")]
        let cell = cell.clamp(-1.0e9, 1.0e9) as i64;
        cell
    };
    for (ring_index, ring) in rings.iter().enumerate() {
        for index in 0..ring.len() {
            let a = ring[index];
            let b = ring[(index + 1) % ring.len()];
            for cx in cell_of(a.x.min(b.x))..=cell_of(a.x.max(b.x)) {
                for cy in cell_of(a.y.min(b.y))..=cell_of(a.y.max(b.y)) {
                    buckets.entry((cx, cy)).or_default().push((ring_index, index));
                }
            }
        }
    }
    let edge = |(ring, index): (usize, usize)| (rings[ring][index], rings[ring][(index + 1) % rings[ring].len()]);
    let adjacent = |(ring_a, a): (usize, usize), (ring_b, b): (usize, usize)| {
        let length = rings[ring_a].len();
        ring_a == ring_b && (a == b || (a + 1) % length == b || (b + 1) % length == a)
    };
    buckets.values().any(|edges| {
        edges.iter().enumerate().any(|(position, &first)| {
            edges[position + 1..].iter().any(|&second| {
                if adjacent(first, second) {
                    return false;
                }
                let (p1, p2) = edge(first);
                let (q1, q2) = edge(second);
                segments_cross(p1, p2, q1, q2)
            })
        })
    })
}

/// Proper crossing of segments `p1-p2` and `q1-q2` (touching endpoints do not count).
fn segments_cross(p1: Point, p2: Point, q1: Point, q2: Point) -> bool {
    let orient = |a: Point, b: Point, c: Point| (b - a).cross(c - a);
    let d1 = orient(q1, q2, p1);
    let d2 = orient(q1, q2, p2);
    let d3 = orient(p1, p2, q1);
    let d4 = orient(p1, p2, q2);
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0)) && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::{Contour, Seg};

    fn polygon(points: &[(f64, f64)]) -> Contour {
        let mut iter = points.iter().map(|&(x, y)| Point::new(x, y));
        let start = iter.next().expect("non-empty");
        Contour { start, segs: iter.map(Seg::Line).collect() }
    }

    #[test]
    fn same_topology_with_moved_bar_interpolates_the_bar() {
        // Two "E"-like bars: a horizontal bar at y 300..380 vs 400..480 inside an outline of
        // a different point structure; the blend puts it at 350..430.
        let low = vec![polygon(&[(0.0, 0.0), (0.0, 700.0), (500.0, 700.0), (500.0, 380.0), (100.0, 380.0), (100.0, 300.0), (500.0, 300.0), (500.0, 0.0)])];
        let high = vec![polygon(&[(0.0, 0.0), (0.0, 700.0), (250.0, 700.0), (500.0, 700.0), (500.0, 480.0), (100.0, 480.0), (100.0, 400.0), (500.0, 400.0), (500.0, 0.0)])];
        let blended = interpolate(&[&low, &high], &[0.5, 0.5], &[500.0, 500.0]).expect("same topology matches");
        assert_eq!(blended.len(), 1);
        let mut polygon_points = Vec::new();
        flatten_contour(&blended[0], 0.2, &mut polygon_points);
        let near = |x: f64, y: f64| polygon_points.iter().any(|point| point.distance(Point::new(x, y)) < 4.0);
        assert!(near(100.0, 350.0) && near(100.0, 430.0), "bar corners moved to the average height");
    }

    #[test]
    fn different_contour_counts_do_not_match() {
        let one = vec![polygon(&[(0.0, 0.0), (0.0, 100.0), (100.0, 100.0), (100.0, 0.0)])];
        let two = vec![
            polygon(&[(0.0, 0.0), (0.0, 100.0), (40.0, 100.0), (40.0, 0.0)]),
            polygon(&[(60.0, 0.0), (60.0, 100.0), (100.0, 100.0), (100.0, 0.0)]),
        ];
        assert!(interpolate(&[&one, &two], &[0.5, 0.5], &[100.0, 100.0]).is_none());
    }

    #[test]
    fn crossing_detection() {
        let bow = vec![vec![Point::new(0.0, 0.0), Point::new(100.0, 100.0), Point::new(100.0, 0.0), Point::new(0.0, 100.0)]];
        assert!(edges_cross(&bow));
        let square = vec![vec![Point::new(0.0, 0.0), Point::new(0.0, 100.0), Point::new(100.0, 100.0), Point::new(100.0, 0.0)]];
        assert!(!edges_cross(&square));
    }
}
