/*
File: crates/ms-custom-fonts/src/hybrid/skeleton/geometry.rs

Purpose:
Geometry kit of the skeleton blend: strokes (polylines carrying a radius per point) with
arc-length sampling, and exact point-to-boundary distances of one source glyph.

Key structures:
- `Stroke`: points + radii + the `Cap` shape of its free ends; `Sampler` evaluates it at an
  arc-length fraction.
- `SegmentIndex`: the glyph's flattened boundary segments in a uniform bucket grid; `distance`
  is the exact Euclidean distance from a point to the nearest segment.

Key functions:
- `sample_count`: bounded sample count for a length.
- `index_f64` / `clamp_index`: the module's only `usize` <-> `f64` conversions.

Notes:
Exact distances (instead of bilinear samples of a node distance field) keep stroke radii
accurate to well below a cell: a bilinear sample between two nodes straddling a stroke's ridge
underestimates the half width by up to half a cell. The bucket index is bounded
(`MAX_INDEX_ENTRIES`); an outline too dense for it is reported by the caller as a fallback.
*/

use kurbo::Point;

use crate::hybrid::sdf::Grid;

/// Bucket edge of the segment index, in grid cells.
const BUCKET_CELLS: f64 = 4.0;
/// Upper bound of (segment, bucket) entries of one index; an outline needing more is too dense
/// for the skeleton path (its caller falls back to the distance-field blend).
const MAX_INDEX_ENTRIES: usize = 1 << 22;
/// Upper bound of the overlap classification's work (polygon boxes tested, candidate segments
/// and polygon edges visited) for one outline. Real glyphs need a few hundred thousand at most;
/// a hostile one is refused instead of costing points x contours.
const MAX_OVERLAP_WORK: usize = 1 << 26;

/// A polyline with a stroke radius (half width, font units) at every point. A single point is
/// a dot (a disc). A closed stroke repeats its first point at the end.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Stroke {
    pub(super) points: Vec<Point>,
    pub(super) radii: Vec<f64>,
    /// Terminal shape at the first / last point when that end is free (not at a junction).
    pub(super) caps: [Option<Cap>; 2],
}

/// Shape of a free stroke end: how far each side's corner fills the square around the round
/// cap, `0` (round) ..= `1` (square, a flat cut perpendicular to the stroke). Sides are seen
/// looking out of the stroke through that end (`left` = counter-clockwise of the outward
/// tangent), so they do not depend on the stroke's direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Cap {
    pub(super) left: f64,
    pub(super) right: f64,
}

impl Stroke {
    /// A stroke without caps.
    pub(super) fn new(points: Vec<Point>, radii: Vec<f64>) -> Self {
        Self { points, radii, caps: [None, None] }
    }
}

impl Stroke {
    /// Total polyline length, font units.
    pub(super) fn length(&self) -> f64 {
        self.points.windows(2).map(|pair| pair[0].distance(pair[1])).sum()
    }

    /// An arc-length sampler over this stroke.
    pub(super) fn sampler(&self) -> Sampler<'_> {
        let mut cumulative = Vec::with_capacity(self.points.len());
        let mut total = 0.0;
        cumulative.push(0.0);
        for pair in self.points.windows(2) {
            total += pair[0].distance(pair[1]);
            cumulative.push(total);
        }
        Sampler { stroke: self, cumulative, total }
    }
}

/// Evaluates a `Stroke` at arc-length fractions.
#[derive(Debug)]
pub(super) struct Sampler<'a> {
    stroke: &'a Stroke,
    cumulative: Vec<f64>,
    total: f64,
}

impl Sampler<'_> {
    /// Point and radius at arc fraction `u` (clamped to `0..=1`). A zero-length stroke yields
    /// its first point; an empty one the origin with radius 0 (never constructed by callers).
    pub(super) fn at(&self, u: f64) -> (Point, f64) {
        let points = &self.stroke.points;
        let radii = &self.stroke.radii;
        let (Some(&first), Some(&first_radius)) = (points.first(), radii.first()) else {
            return (Point::ZERO, 0.0);
        };
        if self.total <= 0.0 || points.len() < 2 {
            return (first, first_radius);
        }
        let target = u.clamp(0.0, 1.0) * self.total;
        // First segment whose end lies at or beyond the target arc length.
        let end = self.cumulative.partition_point(|&length| length < target).clamp(1, points.len() - 1);
        let start = end - 1;
        let span = self.cumulative[end] - self.cumulative[start];
        let t = if span > 0.0 { ((target - self.cumulative[start]) / span).clamp(0.0, 1.0) } else { 0.0 };
        let point = points[start].lerp(points[end], t);
        let radius = radii[start] + (radii[end] - radii[start]) * t;
        (point, radius)
    }

    /// Total length of the sampled stroke.
    pub(super) fn total(&self) -> f64 {
        self.total
    }
}

/// Lossless-enough `usize` -> `f64` for grid indices and counts (always far below 2^32 here).
pub(super) fn index_f64(index: usize) -> f64 {
    f64::from(u32::try_from(index).unwrap_or(u32::MAX))
}

/// `floor(value)` as an index clamped to `0..count` (`count >= 1`); NaN maps to 0.
pub(super) fn clamp_index(value: f64, count: usize) -> usize {
    let last = count.saturating_sub(1);
    let floored = value.floor();
    if floored.is_nan() || floored <= 0.0 {
        return 0;
    }
    if floored >= index_f64(last) {
        return last;
    }
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "an integral value in 0..last (< 2^32), checked just above")]
    let index = floored as usize;
    index
}

/// Samples needed to cover `length` with spacing `spacing`: `ceil(length / spacing)` clamped to
/// `min..=max`. NaN or non-positive input yields `min`.
pub(super) fn sample_count(length: f64, spacing: f64, min: usize, max: usize) -> usize {
    let wanted = (length / spacing).ceil();
    if wanted.is_nan() || wanted <= index_f64(min) {
        return min;
    }
    if wanted >= index_f64(max) {
        return max;
    }
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "an integral value strictly between min and max, checked just above")]
    let count = wanted as usize;
    count
}

/// The boundary segments of one glyph in a uniform bucket grid, for exact nearest-boundary
/// distances.
#[derive(Debug)]
pub(super) struct SegmentIndex {
    segments: Vec<(Point, Point)>,
    x0: f64,
    y0: f64,
    size: f64,
    nx: usize,
    ny: usize,
    /// CSR offsets into `entries`, one per bucket plus the end.
    starts: Vec<usize>,
    entries: Vec<u32>,
}

/// The index would exceed `MAX_INDEX_ENTRIES` (or `u32` segment ids); carries the entry count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct IndexTooLarge(pub(super) usize);

impl SegmentIndex {
    /// Indexes the boundary of the nonzero-winding union of the closed `polygons` (already
    /// aligned) over `grid`'s extent: edge parts inside another contour (overlapping contours,
    /// common in TrueType sources) are not boundary and are left out, so distances measure
    /// the real stroke width.
    ///
    /// # Errors
    /// `IndexTooLarge` when the outline needs more than `MAX_INDEX_ENTRIES` bucket entries.
    pub(super) fn new(polygons: &[Vec<Point>], grid: &Grid) -> Result<Self, IndexTooLarge> {
        let mut segments = Vec::new();
        let mut owners = Vec::new();
        for (contour, polygon) in polygons.iter().enumerate() {
            for (position, &start) in polygon.iter().enumerate() {
                segments.push((start, polygon[(position + 1) % polygon.len()]));
                owners.push(contour);
            }
        }
        if polygons.len() < 2 {
            return Self::from_segments(segments, grid);
        }
        let all = Self::from_segments(segments, grid)?;
        let boundary = union_boundary(polygons, &all, &owners)?;
        Self::from_segments(boundary, grid)
    }

    /// Indexes `segments` over `grid`'s extent.
    ///
    /// # Errors
    /// `IndexTooLarge` when they need more than `MAX_INDEX_ENTRIES` bucket entries.
    fn from_segments(segments: Vec<(Point, Point)>, grid: &Grid) -> Result<Self, IndexTooLarge> {
        let size = BUCKET_CELLS * grid.cell;
        let nx = sample_count(index_f64(grid.nx) * grid.cell, size, 1, usize::MAX) + 1;
        let ny = sample_count(index_f64(grid.ny) * grid.cell, size, 1, usize::MAX) + 1;
        if u32::try_from(segments.len()).is_err() {
            return Err(IndexTooLarge(segments.len()));
        }
        let mut index = Self { segments, x0: grid.x0, y0: grid.y0, size, nx, ny, starts: Vec::new(), entries: Vec::new() };
        let ranges: Vec<[usize; 4]> = index.segments.iter().map(|&(a, b)| index.bucket_box(a, b)).collect();
        let mut counts = vec![0_usize; nx * ny + 1];
        let mut total = 0_usize;
        for &[i0, i1, j0, j1] in &ranges {
            total = total.saturating_add((i1 - i0 + 1).saturating_mul(j1 - j0 + 1));
            if total > MAX_INDEX_ENTRIES {
                return Err(IndexTooLarge(total));
            }
            for j in j0..=j1 {
                for i in i0..=i1 {
                    counts[j * nx + i + 1] += 1;
                }
            }
        }
        for slot in 1..counts.len() {
            counts[slot] += counts[slot - 1];
        }
        let mut cursor = counts.clone();
        let mut entries = vec![0_u32; total];
        for (segment, &[i0, i1, j0, j1]) in ranges.iter().enumerate() {
            // Checked against u32 above.
            let id = u32::try_from(segment).unwrap_or(u32::MAX);
            for j in j0..=j1 {
                for i in i0..=i1 {
                    let bucket = j * nx + i;
                    entries[cursor[bucket]] = id;
                    cursor[bucket] += 1;
                }
            }
        }
        index.starts = counts;
        index.entries = entries;
        Ok(index)
    }

    /// Inclusive bucket ranges `[i0, i1, j0, j1]` of a segment's bounding box.
    fn bucket_box(&self, a: Point, b: Point) -> [usize; 4] {
        [
            clamp_index((a.x.min(b.x) - self.x0) / self.size, self.nx),
            clamp_index((a.x.max(b.x) - self.x0) / self.size, self.nx),
            clamp_index((a.y.min(b.y) - self.y0) / self.size, self.ny),
            clamp_index((a.y.max(b.y) - self.y0) / self.size, self.ny),
        ]
    }

    /// Exact distance from `point` to the nearest boundary segment (`f64::INFINITY` for an
    /// index without segments). Rings of buckets are searched outward until nothing outside
    /// the searched block can be closer.
    pub(super) fn distance(&self, point: Point) -> f64 {
        let ci = clamp_index((point.x - self.x0) / self.size, self.nx);
        let cj = clamp_index((point.y - self.y0) / self.size, self.ny);
        let mut best2 = f64::INFINITY;
        let rings = self.nx.max(self.ny);
        for ring in 0..=rings {
            let (i_low, i_high) = (ci.saturating_sub(ring), (ci + ring).min(self.nx - 1));
            let (j_low, j_high) = (cj.saturating_sub(ring), (cj + ring).min(self.ny - 1));
            for j in j_low..=j_high {
                let edge_row = j + ring == cj || j == cj + ring;
                if edge_row {
                    for i in i_low..=i_high {
                        best2 = best2.min(self.bucket_distance2(point, j * self.nx + i));
                    }
                } else {
                    if ci >= ring {
                        best2 = best2.min(self.bucket_distance2(point, j * self.nx + ci - ring));
                    }
                    if ring > 0 && ci + ring < self.nx {
                        best2 = best2.min(self.bucket_distance2(point, j * self.nx + ci + ring));
                    }
                }
            }
            // Distance from the point to the nearest unsearched bucket (infinite on a side that
            // already reaches the index border).
            let mut bound = f64::INFINITY;
            if ci > ring {
                bound = bound.min(point.x - (self.x0 + index_f64(ci - ring) * self.size));
            }
            if ci + ring + 1 < self.nx {
                bound = bound.min(self.x0 + index_f64(ci + ring + 1) * self.size - point.x);
            }
            if cj > ring {
                bound = bound.min(point.y - (self.y0 + index_f64(cj - ring) * self.size));
            }
            if cj + ring + 1 < self.ny {
                bound = bound.min(self.y0 + index_f64(cj + ring + 1) * self.size - point.y);
            }
            if bound.is_infinite() || best2 <= bound.max(0.0) * bound.max(0.0) {
                break;
            }
        }
        best2.sqrt()
    }

    /// Ids of the segments sharing a bucket with segment `a`-`b`'s box, ascending, no repeats.
    fn candidates(&self, a: Point, b: Point) -> Vec<usize> {
        let [i0, i1, j0, j1] = self.bucket_box(a, b);
        let mut ids = Vec::new();
        for j in j0..=j1 {
            for i in i0..=i1 {
                let bucket = j * self.nx + i;
                ids.extend(self.entries[self.starts[bucket]..self.starts[bucket + 1]].iter().filter_map(|&id| usize::try_from(id).ok()));
            }
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Smallest squared distance from `point` to the segments of one bucket.
    fn bucket_distance2(&self, point: Point, bucket: usize) -> f64 {
        let mut best2 = f64::INFINITY;
        for &id in &self.entries[self.starts[bucket]..self.starts[bucket + 1]] {
            // Ids are positions in `segments` (written by `new`).
            let Some(&(a, b)) = usize::try_from(id).ok().and_then(|id| self.segments.get(id)) else {
                continue;
            };
            best2 = best2.min(segment_distance2(point, a, b));
        }
        best2
    }
}

/// The parts of the polygons' edges on the boundary of their nonzero-winding union. `all`
/// indexes every edge (ids in polygon order), `owners` names each edge's polygon. An edge part
/// of polygon `c` is boundary iff the winding of the OTHER polygons there is 0 or `-s_c` (`s_c`
/// = the winding just inside `c` itself), i.e. iff exactly one side has total winding 0.
/// Edges are split where they cross other polygons' edges; each part is classified at its
/// midpoint. A polygon crossing itself is treated as simple.
///
/// # Errors
/// `IndexTooLarge` (with the work done) when the classification would visit more than
/// `MAX_OVERLAP_WORK` polygon boxes, candidate segments or polygon edges in total.
fn union_boundary(polygons: &[Vec<Point>], all: &SegmentIndex, owners: &[usize]) -> Result<Vec<(Point, Point)>, IndexTooLarge> {
    let mut work = 0_usize;
    let mut spend = |amount: usize| -> Result<(), IndexTooLarge> {
        work = work.saturating_add(amount);
        if work > MAX_OVERLAP_WORK { Err(IndexTooLarge(work)) } else { Ok(()) }
    };
    let boxes: Vec<[f64; 4]> = polygons
        .iter()
        .map(|polygon| {
            polygon.iter().fold([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY], |acc, point| {
                [acc[0].min(point.x), acc[1].min(point.y), acc[2].max(point.x), acc[3].max(point.y)]
            })
        })
        .collect();
    // Winding just inside each polygon (same crossing convention as `winding_number`): a
    // counter-clockwise polygon (positive area) winds -1 there.
    let signs: Vec<i32> = polygons.iter().map(|polygon| if shoelace(polygon) > 0.0 { -1 } else { 1 }).collect();
    // Winding of the other polygons at `point`, and the edges visited to get it.
    let others_winding = |point: Point, own: usize| -> (i32, usize) {
        let mut visited = polygons.len();
        let winding = polygons
            .iter()
            .enumerate()
            .filter(|&(contour, _)| contour != own)
            .filter(|&(contour, _)| {
                let [x0, y0, x1, y1] = boxes[contour];
                point.x >= x0 && point.x <= x1 && point.y >= y0 && point.y <= y1
            })
            .map(|(_, polygon)| {
                visited += polygon.len();
                winding_number(polygon, point)
            })
            .sum();
        (winding, visited)
    };
    let mut boundary = Vec::with_capacity(all.segments.len());
    for (id, &(a, b)) in all.segments.iter().enumerate() {
        let own = owners[id];
        spend(polygons.len())?;
        let touches_other = boxes.iter().enumerate().any(|(contour, &[x0, y0, x1, y1])| {
            contour != own && a.x.max(b.x) >= x0 && a.x.min(b.x) <= x1 && a.y.max(b.y) >= y0 && a.y.min(b.y) <= y1
        });
        if !touches_other {
            boundary.push((a, b));
            continue;
        }
        let mut cuts = vec![0.0, 1.0];
        let candidates = all.candidates(a, b);
        spend(candidates.len())?;
        for other in candidates {
            if owners[other] == own {
                continue;
            }
            let (c, d) = all.segments[other];
            if let Some(t) = crossing_parameter(a, b, c, d) {
                cuts.push(t);
            }
        }
        cuts.sort_by(f64::total_cmp);
        cuts.dedup();
        for pair in cuts.windows(2) {
            let (start, end) = (a.lerp(b, pair[0]), a.lerp(b, pair[1]));
            if start == end {
                continue;
            }
            let (others, visited) = others_winding(start.midpoint(end), own);
            spend(visited)?;
            if others == 0 || others == -signs[own] {
                boundary.push((start, end));
            }
        }
    }
    Ok(boundary)
}

/// Parameter in `(0, 1)` along `start`-`end` where it properly crosses
/// `other_start`-`other_end` (`None` when parallel, degenerate or not crossing).
fn crossing_parameter(start: Point, end: Point, other_start: Point, other_end: Point) -> Option<f64> {
    let direction = end - start;
    let other_direction = other_end - other_start;
    let denominator = direction.cross(other_direction);
    if !exceeds(denominator.abs(), 0.0) {
        return None;
    }
    let offset = other_start - start;
    let along = offset.cross(other_direction) / denominator;
    let along_other = offset.cross(direction) / denominator;
    (along > 0.0 && along < 1.0 && (0.0..=1.0).contains(&along_other)).then_some(along)
}

/// `value > limit`, false when either is NaN (so `!exceeds(x, 0.0)` reads "not a positive
/// number", rejecting NaN too).
pub(super) fn exceeds(value: f64, limit: f64) -> bool {
    value > limit
}

/// Shoelace area of a closed polygon (positive = counter-clockwise, y up).
fn shoelace(polygon: &[Point]) -> f64 {
    let mut sum = 0.0;
    for (position, a) in polygon.iter().enumerate() {
        let b = polygon[(position + 1) % polygon.len()];
        sum += a.x * b.y - b.x * a.y;
    }
    0.5 * sum
}

/// Winding number of `point` against one closed polygon (crossings left of the point, +1 for
/// an upward edge; half-open in y).
pub(super) fn winding_number(polygon: &[Point], point: Point) -> i32 {
    let mut winding = 0;
    for (position, &a) in polygon.iter().enumerate() {
        let b = polygon[(position + 1) % polygon.len()];
        if (a.y <= point.y) != (b.y <= point.y) {
            let x = a.x + (point.y - a.y) * (b.x - a.x) / (b.y - a.y);
            if x < point.x {
                winding += if b.y > a.y { 1 } else { -1 };
            }
        }
    }
    winding
}

/// Squared distance from `point` to the segment `a`-`b`.
pub(super) fn segment_distance2(point: Point, a: Point, b: Point) -> f64 {
    let ab = b - a;
    let length2 = ab.hypot2();
    let t = if length2 > 0.0 { ((point - a).dot(ab) / length2).clamp(0.0, 1.0) } else { 0.0 };
    point.distance_squared(a + ab * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute(point: Point, polygons: &[Vec<Point>]) -> f64 {
        let mut best = f64::INFINITY;
        for polygon in polygons {
            for (position, &a) in polygon.iter().enumerate() {
                best = best.min(segment_distance2(point, a, polygon[(position + 1) % polygon.len()]));
            }
        }
        best.sqrt()
    }

    #[test]
    fn index_distance_equals_brute_force() {
        let polygons = vec![
            vec![Point::new(0.0, 0.0), Point::new(0.0, 700.0), Point::new(90.0, 700.0), Point::new(90.0, 0.0)],
            vec![Point::new(300.0, 100.0), Point::new(500.0, 650.0), Point::new(520.0, 90.0)],
        ];
        let grid = Grid { x0: -20.0, y0: -20.0, cell: 4.0, nx: 150, ny: 190 };
        let index = SegmentIndex::new(&polygons, &grid).expect("small outline");
        assert_eq!(index.segments.len(), 7, "disjoint contours are all boundary");
        for step in 0..400 {
            let x = -20.0 + f64::from(step % 20) * 29.3;
            let y = -20.0 + f64::from(step / 20) * 37.1;
            let point = Point::new(x, y);
            assert!((index.distance(point) - brute(point, &polygons)).abs() < 1e-9, "{point:?}");
        }
    }

    #[test]
    fn overlapping_contours_measure_the_union() {
        // A stem crossing a bar: the bar's edges inside the stem are not boundary.
        let stem = vec![Point::new(0.0, 0.0), Point::new(0.0, 700.0), Point::new(90.0, 700.0), Point::new(90.0, 0.0)];
        let bar = vec![Point::new(-200.0, 300.0), Point::new(-200.0, 380.0), Point::new(300.0, 380.0), Point::new(300.0, 300.0)];
        let grid = Grid { x0: -220.0, y0: -20.0, cell: 4.0, nx: 140, ny: 190 };
        let index = SegmentIndex::new(&[stem.clone(), bar.clone()], &grid).expect("small outline");
        // Centre of the crossing: the nearest real boundary points are the concave corners.
        assert!((index.distance(Point::new(45.0, 340.0)) - 45.0_f64.hypot(40.0)).abs() < 1e-9);
        // A stem point between the bar's (removed) edges and the stem side.
        assert!((index.distance(Point::new(45.0, 200.0)) - 45.0).abs() < 1e-9);
        // A counter inside an outer contour keeps its boundary.
        let outer = vec![Point::new(0.0, 0.0), Point::new(0.0, 500.0), Point::new(500.0, 500.0), Point::new(500.0, 0.0)];
        let hole = vec![Point::new(100.0, 100.0), Point::new(400.0, 100.0), Point::new(400.0, 400.0), Point::new(100.0, 400.0)];
        let grid = Grid { x0: -20.0, y0: -20.0, cell: 4.0, nx: 140, ny: 140 };
        let index = SegmentIndex::new(&[outer, hole], &grid).expect("small outline");
        assert!((index.distance(Point::new(50.0, 250.0)) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn sampler_interpolates_by_arc_length() {
        let stroke = Stroke::new(vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0), Point::new(10.0, 30.0)], vec![1.0, 2.0, 4.0]);
        let sampler = stroke.sampler();
        assert!((sampler.total() - 40.0).abs() < 1e-12);
        let (point, radius) = sampler.at(0.5);
        assert!(point.distance(Point::new(10.0, 10.0)) < 1e-12 && (radius - (2.0 + 2.0 * 10.0 / 30.0)).abs() < 1e-12);
        assert_eq!(sampler.at(-1.0).0, Point::new(0.0, 0.0));
        assert_eq!(sampler.at(2.0).0, Point::new(10.0, 30.0));
    }

    #[test]
    fn counts_and_indices_are_clamped() {
        assert_eq!(sample_count(f64::NAN, 4.0, 2, 10), 2);
        assert_eq!(sample_count(1e300, 4.0, 2, 10), 10);
        assert_eq!(sample_count(17.0, 4.0, 2, 10), 5);
        assert_eq!(clamp_index(-3.0, 5), 0);
        assert_eq!(clamp_index(f64::INFINITY, 5), 4);
        assert_eq!(clamp_index(2.7, 5), 2);
    }
}
