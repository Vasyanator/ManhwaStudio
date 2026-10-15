/*
File: crates/ms-custom-fonts/src/hybrid/sdf.rs

Purpose:
The GENERAL path of the outline blend: signed distance fields of every participating source
glyph on one common grid, their weighted average, and the zero level set of the average as
oriented closed polylines (re-fitted into curves by `trace.rs`).

Key structures:
- `Grid`: node lattice in font units.

Key functions:
- `blend`: outlines + weights -> blended `Outline`.
- `signed_distance_field`: exact-seeded vector-propagated distances, nonzero-winding sign.
- `marching_squares`: oriented zero-level loops (inside on the right => outer contours CW).

Notes:
- Alignment (per glyph): x either "natural" (scaled by blended advance / own advance) or
  ink-box mapped onto the weighted average ink box; y either as designed or ink-box mapped.
  The combination under which the sources' filled areas overlap most (weighted pairwise IoU
  on an `ALIGNMENT_CELL_UNITS` lattice) wins; ties keep the natural one. Neither fixed choice
  works for all glyphs: ink boxes misalign a "1" with a foot against one without (the extra
  feature stretches the other glyph off its stem), the natural frame misaligns a "J" that
  descends against one that sits on the baseline.
- Inherent limit of distance-field morphing: strokes of the two designs that do not overlap
  fade towards 50/50 instead of moving; designs with matching structure blend cleanly.
- Distances: nodes within `SEED_BAND_CELLS` of the flattened outline get the EXACT closest
  point on it; the closest point is then propagated to every other node by two 8SSEDT sweeps.
  Sub-cell accurate everywhere, which keeps the re-traced contours smooth.
- Sign: nonzero winding of the node, by scanline crossings (overlapping contours are a union).
- Grid: `SDF_CELL_UNITS` per cell (coarsened deterministically for huge glyphs so a grid never
  exceeds `MAX_GRID_NODES`). Changing any constant here changes output bytes and requires an
  `ENGINE_VERSION` bump.
- Bounded work on hostile input: a blended glyph wider or taller than `MAX_BLEND_EXTENT_UNITS`
  (or with non-finite coordinates) after alignment is rejected (`ExtentTooLarge`, mapped to
  `CompileError::GlyphTooComplex` by the caller) instead of being gridded; node counts are
  computed in constant time and the coarsening loop is bounded, so tiny units-per-em or
  extreme advances cannot make grid sizing spin.
- Determinism: sources are summed in recipe order; every node is computed independently of
  thread scheduling.
*/

use kurbo::Point;

use crate::hybrid::trace;
use crate::outline::{Outline, flatten_contour};

/// Grid cell size in font units (1000 per em).
pub(crate) const SDF_CELL_UNITS: f64 = 4.0;
/// Upper bound of nodes per grid; larger glyphs get a proportionally coarser cell.
const MAX_GRID_NODES: usize = 1 << 20;
/// Flattening tolerance of source outlines, font units.
const FLATTEN_TOLERANCE: f64 = 0.05;
/// Nodes this many cells around the outline get exact distances.
const SEED_BAND_CELLS: f64 = 1.5;
/// Empty border around the blended ink box, in cells (keeps every level-set loop closed).
const MARGIN_CELLS: f64 = 3.0;
/// Advances or ink extents below this (font units, e.g. combining marks, a hairline) are not
/// used for scaling.
const MIN_SCALABLE_ADVANCE: f64 = 1.0;
/// Lattice spacing of the alignment overlap score, font units.
const ALIGNMENT_CELL_UNITS: f64 = 8.0;
/// Largest width / height (font units) of an aligned glyph this path grids: 8x the whole
/// storable 16-bit coordinate range, so no glyph that could be written is ever refused, while
/// absurd source metrics (tiny units per em, extreme advances scaling the natural alignment)
/// are rejected before any grid work.
const MAX_BLEND_EXTENT_UNITS: f64 = 262_144.0;
/// Cell doublings `grid_for` tries. Within `MAX_BLEND_EXTENT_UNITS` at most ~6 are needed; the
/// bound only guarantees termination.
const MAX_COARSENING_STEPS: u32 = 32;

/// An aligned glyph too large (or not finite) for the distance-field path. `Display` is the
/// reason text of `CompileError::GlyphTooComplex`.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("the blended outline would span {width:.0} x {height:.0} units, beyond the {MAX_BLEND_EXTENT_UNITS} unit limit of the distance-field blend")]
pub(crate) struct ExtentTooLarge {
    /// Width of the aligned outlines' union, font units (may be non-finite).
    pub width: f64,
    /// Height of the aligned outlines' union, font units (may be non-finite).
    pub height: f64,
}

/// Node lattice: node `(i, j)` sits at `(x0 + i * cell, y0 + j * cell)`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Grid {
    pub x0: f64,
    pub y0: f64,
    pub cell: f64,
    pub nx: usize,
    pub ny: usize,
}

impl Grid {
    fn x(&self, i: usize) -> f64 {
        self.x0 + index_f64(i) * self.cell
    }
    fn y(&self, j: usize) -> f64 {
        self.y0 + index_f64(j) * self.cell
    }
}

/// Lossless-enough `usize` -> `f64` for grid indices (always far below 2^53).
fn index_f64(index: usize) -> f64 {
    f64::from(u32::try_from(index).unwrap_or(u32::MAX))
}

/// Axis-aligned box.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

/// Blends non-empty outlines (`weights` sum to 1) through their distance fields. `advances`
/// are the sources' advances (normalized units), used by the natural horizontal alignment.
///
/// # Errors
/// `ExtentTooLarge` when every candidate alignment yields outlines beyond
/// `MAX_BLEND_EXTENT_UNITS` (or with non-finite coordinates).
pub(crate) fn blend(outlines: &[&Outline], weights: &[f64], advances: &[f64]) -> Result<Outline, ExtentTooLarge> {
    let raw: Vec<Vec<Vec<Point>>> = outlines.iter().map(|outline| flatten(outline)).collect();
    let Some(alignment) = choose_alignment(&raw, weights, advances)? else {
        return Ok(Vec::new());
    };
    let polygons: Vec<Vec<Vec<Point>>> = raw.iter().zip(&alignment).map(|(polygons, map)| map.apply(polygons)).collect();
    let all: Vec<Vec<Point>> = polygons.iter().flatten().cloned().collect();
    let Some(union) = checked_bounds(&all)? else {
        return Ok(Vec::new());
    };
    let grid = grid_for(union, SDF_CELL_UNITS).ok_or_else(|| ExtentTooLarge::of(union))?;
    let mut field = vec![0.0_f64; grid.nx * grid.ny];
    for (source_polygons, weight) in polygons.iter().zip(weights) {
        let source_field = signed_distance_field(source_polygons, &grid);
        for (value, distance) in field.iter_mut().zip(&source_field) {
            *value += weight * distance;
        }
    }
    let loops = marching_squares(&field, &grid);
    Ok(trace::fit_loops(loops, grid.cell))
}

impl ExtentTooLarge {
    fn of(bounds: Bounds) -> Self {
        Self { width: bounds.x1 - bounds.x0, height: bounds.y1 - bounds.y0 }
    }
}

/// `bounds`, rejecting a box that is not finite or exceeds `MAX_BLEND_EXTENT_UNITS` on an
/// axis. `Ok(None)` when there are no points.
fn checked_bounds(polygons: &[Vec<Point>]) -> Result<Option<Bounds>, ExtentTooLarge> {
    let Some(found) = bounds(polygons) else {
        return Ok(None);
    };
    let error = ExtentTooLarge::of(found);
    // `!(x <= max)` also rejects NaN; infinite corners make the extent infinite or NaN.
    let within = |extent: f64| extent.is_finite() && extent <= MAX_BLEND_EXTENT_UNITS;
    if within(error.width) && within(error.height) && [found.x0, found.y0, found.x1, found.y1].iter().all(|value| value.is_finite()) {
        Ok(Some(found))
    } else {
        Err(error)
    }
}

/// Per-axis affine map `p -> (sx * x + ox, sy * y + oy)` of one source.
#[derive(Debug, Clone, Copy)]
struct AxisMap {
    sx: f64,
    ox: f64,
    sy: f64,
    oy: f64,
}

impl AxisMap {
    fn apply(&self, polygons: &[Vec<Point>]) -> Vec<Vec<Point>> {
        polygons
            .iter()
            .map(|polygon| polygon.iter().map(|point| Point::new(point.x * self.sx + self.ox, point.y * self.sy + self.oy)).collect())
            .collect()
    }
}

/// How one axis of a source is aligned with the others before blending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AxisAlign {
    /// x: scaled by `blended advance / own advance`; y: as designed.
    Natural,
    /// The ink extent on this axis is mapped onto the weighted average ink extent.
    InkBox,
}

/// Candidate alignments in tie-break order (the first of equal scores wins).
const ALIGNMENTS: [(AxisAlign, AxisAlign); 4] = [
    (AxisAlign::Natural, AxisAlign::Natural),
    (AxisAlign::InkBox, AxisAlign::Natural),
    (AxisAlign::Natural, AxisAlign::InkBox),
    (AxisAlign::InkBox, AxisAlign::InkBox),
];

/// Picks, per glyph, the alignment under which the sources' filled areas overlap most
/// (weighted mean pairwise intersection-over-union on a coarse lattice), and returns each
/// source's map. `Ok(None)` when no source has ink. A candidate whose aligned outlines exceed
/// `MAX_BLEND_EXTENT_UNITS` is skipped; `Err` when every candidate does.
fn choose_alignment(raw: &[Vec<Vec<Point>>], weights: &[f64], advances: &[f64]) -> Result<Option<Vec<AxisMap>>, ExtentTooLarge> {
    let Some(boxes) = raw.iter().map(|polygons| bounds(polygons)).collect::<Option<Vec<Bounds>>>() else {
        return Ok(None);
    };
    let target = Bounds {
        x0: boxes.iter().zip(weights).map(|(glyph_box, weight)| weight * glyph_box.x0).sum(),
        y0: boxes.iter().zip(weights).map(|(glyph_box, weight)| weight * glyph_box.y0).sum(),
        x1: boxes.iter().zip(weights).map(|(glyph_box, weight)| weight * glyph_box.x1).sum(),
        y1: boxes.iter().zip(weights).map(|(glyph_box, weight)| weight * glyph_box.y1).sum(),
    };
    let target_advance: f64 = advances.iter().zip(weights).map(|(advance, weight)| advance * weight).sum();
    let maps_for = |(x_align, y_align): (AxisAlign, AxisAlign)| -> Vec<AxisMap> {
        boxes
            .iter()
            .zip(advances)
            .map(|(glyph_box, &advance)| {
                let (sx, ox) = match x_align {
                    AxisAlign::Natural if advance >= MIN_SCALABLE_ADVANCE && target_advance >= MIN_SCALABLE_ADVANCE => {
                        (target_advance / advance, 0.0)
                    }
                    AxisAlign::Natural => (1.0, 0.0),
                    AxisAlign::InkBox => axis_map(glyph_box.x0, glyph_box.x1, target.x0, target.x1),
                };
                let (sy, oy) = match y_align {
                    AxisAlign::Natural => (1.0, 0.0),
                    AxisAlign::InkBox => axis_map(glyph_box.y0, glyph_box.y1, target.y0, target.y1),
                };
                AxisMap { sx, ox, sy, oy }
            })
            .collect()
    };
    let mut best: Option<(f64, Vec<AxisMap>)> = None;
    let mut first_rejection = None;
    for alignment in ALIGNMENTS {
        let maps = maps_for(alignment);
        let score = match overlap_score(raw, &maps, weights) {
            Ok(score) => score,
            Err(rejection) => {
                first_rejection.get_or_insert(rejection);
                continue;
            }
        };
        if best.as_ref().is_none_or(|(best_score, _)| score > *best_score) {
            best = Some((score, maps));
        }
    }
    match (best, first_rejection) {
        (Some((_, maps)), _) => Ok(Some(maps)),
        (None, Some(rejection)) => Err(rejection),
        // `ALIGNMENTS` is non-empty, so every candidate either scored or was rejected.
        (None, None) => Ok(None),
    }
}

/// Weighted mean pairwise intersection-over-union of the sources' inside sets under `maps`,
/// on a lattice of `ALIGNMENT_CELL_UNITS`.
///
/// # Errors
/// `ExtentTooLarge` when the mapped outlines exceed `MAX_BLEND_EXTENT_UNITS`.
fn overlap_score(raw: &[Vec<Vec<Point>>], maps: &[AxisMap], weights: &[f64]) -> Result<f64, ExtentTooLarge> {
    let mapped: Vec<Vec<Vec<Point>>> = raw.iter().zip(maps).map(|(polygons, map)| map.apply(polygons)).collect();
    let all: Vec<Vec<Point>> = mapped.iter().flatten().cloned().collect();
    let Some(union) = checked_bounds(&all)? else {
        return Ok(0.0);
    };
    let grid = grid_for(union, ALIGNMENT_CELL_UNITS).ok_or_else(|| ExtentTooLarge::of(union))?;
    let masks: Vec<Vec<bool>> = mapped.iter().map(|polygons| winding_inside(polygons, &grid)).collect();
    let mut score = 0.0;
    let mut total = 0.0;
    for (first, (mask_a, weight_a)) in masks.iter().zip(weights).enumerate() {
        for (mask_b, weight_b) in masks.iter().zip(weights).skip(first + 1) {
            let (mut both, mut either) = (0_u32, 0_u32);
            for (&a, &b) in mask_a.iter().zip(mask_b) {
                both += u32::from(a && b);
                either += u32::from(a || b);
            }
            let pair_weight = weight_a * weight_b;
            total += pair_weight;
            if either > 0 {
                score += pair_weight * f64::from(both) / f64::from(either);
            }
        }
    }
    Ok(if total > 0.0 { score / total } else { 0.0 })
}

/// Linear map `[from0, from1] -> [to0, to1]` as `(scale, offset)`; translation of the
/// centre only for a degenerate source extent.
fn axis_map(from0: f64, from1: f64, to0: f64, to1: f64) -> (f64, f64) {
    let extent = from1 - from0;
    if extent < MIN_SCALABLE_ADVANCE {
        return (1.0, (to0 + to1) * 0.5 - (from0 + from1) * 0.5);
    }
    let scale = (to1 - to0) / extent;
    (scale, to0 - from0 * scale)
}

/// The grid covering `target` plus the margin, starting at `cell` units and coarsened (cell
/// doubled) for oversized glyphs. `None` when even `MAX_COARSENING_STEPS` doublings do not fit
/// `MAX_GRID_NODES` (only for targets beyond `MAX_BLEND_EXTENT_UNITS` or non-finite ones,
/// which callers reject before).
fn grid_for(target: Bounds, cell: f64) -> Option<Grid> {
    let mut cell = cell;
    for _ in 0..=MAX_COARSENING_STEPS {
        let margin = MARGIN_CELLS * cell;
        let x0 = ((target.x0 - margin) / cell).floor() * cell;
        let y0 = ((target.y0 - margin) / cell).floor() * cell;
        let nx = cells_to_cover(target.x1 + margin - x0, cell);
        let ny = cells_to_cover(target.y1 + margin - y0, cell);
        if nx.saturating_mul(ny) <= MAX_GRID_NODES {
            return Some(Grid { x0, y0, cell, nx, ny });
        }
        cell *= 2.0;
    }
    None
}

/// Nodes needed to cover `extent` with spacing `cell`: the smallest count >= 2 that exceeds
/// `ceil(extent / cell)`, computed in constant time. Any extent needing more than
/// `MAX_GRID_NODES` nodes (including an infinite one) yields `MAX_GRID_NODES + 1`, which no
/// grid accepts, so the caller coarsens; NaN or a non-positive extent yields 2.
fn cells_to_cover(extent: f64, cell: f64) -> usize {
    let cells = (extent / cell).ceil();
    if cells.is_nan() || cells < 2.0 {
        return 2;
    }
    if cells >= index_f64(MAX_GRID_NODES) {
        return MAX_GRID_NODES + 1;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "an integral value (ceil) in 2..MAX_GRID_NODES (2^20), checked just above"
    )]
    let whole = cells as usize;
    whole + 1
}

/// Flattened closed polygons of an outline.
fn flatten(outline: &Outline) -> Vec<Vec<Point>> {
    let mut polygons = Vec::with_capacity(outline.len());
    for contour in outline {
        let mut polygon = Vec::new();
        flatten_contour(contour, FLATTEN_TOLERANCE, &mut polygon);
        if polygon.len() >= 2 {
            polygons.push(polygon);
        }
    }
    polygons
}

/// Bounding box of polygons, `None` when empty.
fn bounds(polygons: &[Vec<Point>]) -> Option<Bounds> {
    let mut points = polygons.iter().flatten();
    let first = points.next()?;
    Some(points.fold(Bounds { x0: first.x, y0: first.y, x1: first.x, y1: first.y }, |acc, point| Bounds {
        x0: acc.x0.min(point.x),
        y0: acc.y0.min(point.y),
        x1: acc.x1.max(point.x),
        y1: acc.y1.max(point.y),
    }))
}

/// Signed distance (negative inside) from every grid node to the polygons' boundary.
pub(crate) fn signed_distance_field(polygons: &[Vec<Point>], grid: &Grid) -> Vec<f64> {
    let node_count = grid.nx * grid.ny;
    let mut nearest = vec![Point::ZERO; node_count];
    let mut dist2 = vec![f64::INFINITY; node_count];
    seed_band(polygons, grid, &mut nearest, &mut dist2);
    propagate(grid, &mut nearest, &mut dist2);
    let inside = winding_inside(polygons, grid);
    dist2
        .iter()
        .zip(&inside)
        .map(|(&squared, &is_inside)| {
            let distance = if squared.is_finite() { squared.sqrt() } else { f64::MAX };
            if is_inside { -distance } else { distance }
        })
        .collect()
}

/// Exact closest points for nodes near each polygon edge.
fn seed_band(polygons: &[Vec<Point>], grid: &Grid, nearest: &mut [Point], dist2: &mut [f64]) {
    let band = SEED_BAND_CELLS * grid.cell;
    for polygon in polygons {
        for index in 0..polygon.len() {
            let a = polygon[index];
            let b = polygon[(index + 1) % polygon.len()];
            let (i0, i1) = node_range(a.x.min(b.x) - band, a.x.max(b.x) + band, grid.x0, grid.cell, grid.nx);
            let (j0, j1) = node_range(a.y.min(b.y) - band, a.y.max(b.y) + band, grid.y0, grid.cell, grid.ny);
            let ab = b - a;
            let length2 = ab.hypot2();
            for j in j0..j1 {
                let y = grid.y(j);
                for i in i0..i1 {
                    let node = Point::new(grid.x(i), y);
                    let t = if length2 > 0.0 { ((node - a).dot(ab) / length2).clamp(0.0, 1.0) } else { 0.0 };
                    let closest = a + ab * t;
                    // Skip nodes outside the band of this edge (the box is wider for diagonals).
                    let squared = node.distance_squared(closest);
                    if squared > band * band {
                        continue;
                    }
                    let slot = j * grid.nx + i;
                    if squared < dist2[slot] {
                        dist2[slot] = squared;
                        nearest[slot] = closest;
                    }
                }
            }
        }
    }
}

/// Node index range `[start, end)` whose coordinates fall in `[low, high]`.
fn node_range(low: f64, high: f64, origin: f64, cell: f64, count: usize) -> (usize, usize) {
    let to_index = |value: f64| -> usize {
        let cells = ((value - origin) / cell).floor();
        if cells <= 0.0 {
            return 0;
        }
        let mut index = 0_usize;
        // Grid dimensions are small (<= MAX_GRID_NODES), so a bounded binary search is exact.
        let (mut low_index, mut high_index) = (0_usize, count);
        while low_index < high_index {
            let mid = low_index + (high_index - low_index) / 2;
            if index_f64(mid) <= cells {
                index = mid;
                low_index = mid + 1;
            } else {
                high_index = mid;
            }
        }
        index
    };
    let start = to_index(low);
    let end = (to_index(high) + 2).min(count);
    (start.min(end), end)
}

/// Takes the closest boundary point of node `from` for node `slot` when it is closer.
fn relax(slot: usize, from: usize, node: Point, nearest: &mut [Point], dist2: &mut [f64]) {
    if !dist2[from].is_finite() {
        return;
    }
    let candidate = nearest[from];
    let squared = node.distance_squared(candidate);
    if squared < dist2[slot] {
        dist2[slot] = squared;
        nearest[slot] = candidate;
    }
}

/// Two 8SSEDT sweeps propagating closest boundary points to every node.
fn propagate(grid: &Grid, nearest: &mut [Point], dist2: &mut [f64]) {
    let (nx, ny) = (grid.nx, grid.ny);
    // Forward: rows bottom-up, neighbours already visited in this pass.
    for j in 0..ny {
        let y = grid.y(j);
        for i in 0..nx {
            let slot = j * nx + i;
            let node = Point::new(grid.x(i), y);
            if i > 0 {
                relax(slot, slot - 1, node, nearest, dist2);
            }
            if j > 0 {
                let below = slot - nx;
                relax(slot, below, node, nearest, dist2);
                if i > 0 {
                    relax(slot, below - 1, node, nearest, dist2);
                }
                if i + 1 < nx {
                    relax(slot, below + 1, node, nearest, dist2);
                }
            }
        }
        for i in (0..nx.saturating_sub(1)).rev() {
            let slot = j * nx + i;
            relax(slot, slot + 1, Point::new(grid.x(i), y), nearest, dist2);
        }
    }
    // Backward: rows top-down.
    for j in (0..ny).rev() {
        let y = grid.y(j);
        for i in (0..nx).rev() {
            let slot = j * nx + i;
            let node = Point::new(grid.x(i), y);
            if i + 1 < nx {
                relax(slot, slot + 1, node, nearest, dist2);
            }
            if j + 1 < ny {
                let above = slot + nx;
                relax(slot, above, node, nearest, dist2);
                if i + 1 < nx {
                    relax(slot, above + 1, node, nearest, dist2);
                }
                if i > 0 {
                    relax(slot, above - 1, node, nearest, dist2);
                }
            }
        }
        for i in 1..nx {
            let slot = j * nx + i;
            relax(slot, slot - 1, Point::new(grid.x(i), y), nearest, dist2);
        }
    }
}

/// Nonzero-winding inside flags of every node, by per-row scanline crossings.
fn winding_inside(polygons: &[Vec<Point>], grid: &Grid) -> Vec<bool> {
    let mut inside = vec![false; grid.nx * grid.ny];
    let mut crossings: Vec<(f64, i32)> = Vec::new();
    for j in 0..grid.ny {
        let y = grid.y(j);
        crossings.clear();
        for polygon in polygons {
            for index in 0..polygon.len() {
                let a = polygon[index];
                let b = polygon[(index + 1) % polygon.len()];
                // Half-open rule: a vertex exactly on the scanline counts once.
                if (a.y <= y) != (b.y <= y) {
                    let x = a.x + (y - a.y) * (b.x - a.x) / (b.y - a.y);
                    crossings.push((x, if b.y > a.y { 1 } else { -1 }));
                }
            }
        }
        crossings.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
        let mut winding = 0;
        let mut next = 0;
        for i in 0..grid.nx {
            let x = grid.x(i);
            while next < crossings.len() && crossings[next].0 < x {
                winding += crossings[next].1;
                next += 1;
            }
            inside[j * grid.nx + i] = winding != 0;
        }
    }
    inside
}

/// Oriented zero-level loops of `field` (inside = negative), inside on the right of travel.
pub(crate) fn marching_squares(field: &[f64], grid: &Grid) -> Vec<Vec<Point>> {
    let (nx, ny) = (grid.nx, grid.ny);
    let edge_slots = 2 * nx * ny;
    // `next_edge[e]` = the crossing edge a segment starting at crossing edge `e` ends on.
    let mut next_edge = vec![usize::MAX; edge_slots];
    let mut starts: Vec<usize> = Vec::new();
    let inside = |i: usize, j: usize| field[j * nx + i] < 0.0;
    for j in 0..ny.saturating_sub(1) {
        for i in 0..nx.saturating_sub(1) {
            // Corners counter-clockwise: (i,j) (i+1,j) (i+1,j+1) (i,j+1); edge k joins
            // corner k to corner k+1.
            let corners = [inside(i, j), inside(i + 1, j), inside(i + 1, j + 1), inside(i, j + 1)];
            let edges = [horizontal_edge(i, j, nx), vertical_edge(i + 1, j, nx), horizontal_edge(i, j + 1, nx), vertical_edge(i, j, nx)];
            let crossing_count = (0..4).filter(|&k| corners[k] != corners[(k + 1) % 4]).count();
            if crossing_count == 0 {
                continue;
            }
            // Walking the cell boundary CCW, edge k "enters" the inside when corner k is out
            // and corner k+1 in. A segment runs from an entering edge to an exiting edge.
            let enter = |k: usize| !corners[k] && corners[(k + 1) % 4];
            let center_inside = (field[j * nx + i] + field[j * nx + i + 1] + field[(j + 1) * nx + i + 1] + field[(j + 1) * nx + i]) < 0.0;
            for k in 0..4 {
                if !enter(k) {
                    continue;
                }
                let exit = if crossing_count == 2 {
                    (1..4).map(|offset| (k + offset) % 4).find(|&other| corners[other] && !corners[(other + 1) % 4])
                } else if center_inside {
                    Some((k + 3) % 4)
                } else {
                    Some((k + 1) % 4)
                };
                if let Some(exit) = exit {
                    next_edge[edges[k]] = edges[exit];
                    starts.push(edges[k]);
                }
            }
        }
    }
    let mut visited = vec![false; edge_slots];
    let mut loops = Vec::new();
    starts.sort_unstable();
    for &start in &starts {
        if visited[start] {
            continue;
        }
        let mut polyline = Vec::new();
        let mut edge = start;
        while !visited[edge] && next_edge[edge] != usize::MAX {
            visited[edge] = true;
            polyline.push(crossing_point(edge, field, grid));
            edge = next_edge[edge];
        }
        if edge == start && polyline.len() >= 3 {
            loops.push(polyline);
        }
    }
    loops
}

/// Id of the horizontal edge from node `(i, j)` to `(i+1, j)`.
fn horizontal_edge(i: usize, j: usize, nx: usize) -> usize {
    2 * (j * nx + i)
}

/// Id of the vertical edge from node `(i, j)` to `(i, j+1)`.
fn vertical_edge(i: usize, j: usize, nx: usize) -> usize {
    2 * (j * nx + i) + 1
}

/// The zero crossing on an edge, interpolated linearly from its two nodes (computed from the
/// edge id alone, so both cells sharing the edge agree bit for bit).
fn crossing_point(edge: usize, field: &[f64], grid: &Grid) -> Point {
    let node = edge / 2;
    let (i, j) = (node % grid.nx, node / grid.nx);
    let horizontal = edge.is_multiple_of(2);
    let other = if horizontal { node + 1 } else { node + grid.nx };
    let (value_a, value_b) = (field[node], field[other]);
    let difference = value_a - value_b;
    // Only edges with a sign change get here, so the difference is non-zero in practice.
    let t = if difference.abs() > 0.0 { (value_a / difference).clamp(0.0, 1.0) } else { 0.5 };
    if horizontal {
        Point::new(grid.x(i) + t * grid.cell, grid.y(j))
    } else {
        Point::new(grid.x(i), grid.y(j) + t * grid.cell)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::{Contour, Seg};

    fn square(x0: f64, y0: f64, x1: f64, y1: f64) -> Outline {
        vec![Contour {
            start: Point::new(x0, y0),
            segs: vec![Seg::Line(Point::new(x0, y1)), Seg::Line(Point::new(x1, y1)), Seg::Line(Point::new(x1, y0))],
        }]
    }

    #[test]
    fn distance_field_is_exact_for_a_square() {
        let polygons = flatten(&square(0.0, 0.0, 100.0, 100.0));
        let grid = Grid { x0: -20.0, y0: -20.0, cell: 2.0, nx: 71, ny: 71 };
        let field = signed_distance_field(&polygons, &grid);
        // Node (i, j) sits at (-20 + 2i, -20 + 2j).
        let at = |i: usize, j: usize| field[j * grid.nx + i];
        assert!((at(35, 35) + 50.0).abs() < 1e-9, "center (50, 50) is 50 units inside");
        assert!((at(0, 35) - 20.0).abs() < 1e-9, "(-20, 50) is 20 units left of the square");
        assert!((at(70, 70) - 20.0 * 2.0_f64.sqrt()).abs() < 1e-9, "corner distance of (120, 120)");
    }

    #[test]
    fn blending_a_shape_with_itself_reproduces_it() {
        let outline = square(100.0, 0.0, 300.0, 700.0);
        let blended = blend(&[&outline, &outline], &[0.5, 0.5], &[400.0, 400.0]).expect("in range");
        assert_eq!(blended.len(), 1);
        let polygons = flatten(&blended);
        let blended_box = bounds(&polygons).expect("ink");
        assert!((blended_box.x0 - 100.0).abs() < 1.0 && (blended_box.x1 - 300.0).abs() < 1.0);
        assert!((blended_box.y0 - 0.0).abs() < 1.0 && (blended_box.y1 - 700.0).abs() < 1.0);
    }

    /// The linear count `cells_to_cover` replaced, kept as the reference it must reproduce.
    fn reference_cells_to_cover(extent: f64, cell: f64) -> usize {
        let cells = (extent / cell).ceil();
        let mut count = 2_usize;
        while index_f64(count) <= cells {
            count += 1;
        }
        count
    }

    #[test]
    fn node_count_matches_the_linear_reference_and_saturates() {
        for cell in [SDF_CELL_UNITS, ALIGNMENT_CELL_UNITS, 16.0] {
            for step in -10..40_000 {
                let extent = f64::from(step) * 0.37;
                assert_eq!(cells_to_cover(extent, cell), reference_cells_to_cover(extent, cell), "extent {extent}, cell {cell}");
            }
        }
        let limit = index_f64(MAX_GRID_NODES);
        assert_eq!(cells_to_cover((limit - 1.0) * 4.0, 4.0), MAX_GRID_NODES);
        assert_eq!(cells_to_cover(limit * 4.0, 4.0), MAX_GRID_NODES + 1);
        assert_eq!(cells_to_cover(f64::INFINITY, 4.0), MAX_GRID_NODES + 1);
        assert_eq!(cells_to_cover(f64::NAN, 4.0), 2);
        assert_eq!(cells_to_cover(f64::NEG_INFINITY, 4.0), 2);
    }

    #[test]
    fn grid_sizing_is_bounded_for_huge_and_non_finite_targets() {
        let at_limit = Bounds { x0: 0.0, y0: 0.0, x1: MAX_BLEND_EXTENT_UNITS, y1: MAX_BLEND_EXTENT_UNITS };
        let grid = grid_for(at_limit, SDF_CELL_UNITS).expect("the largest accepted glyph gets a grid");
        assert!(grid.nx * grid.ny <= MAX_GRID_NODES);
        let infinite = Bounds { x0: 0.0, y0: 0.0, x1: f64::INFINITY, y1: 10.0 };
        assert!(grid_for(infinite, SDF_CELL_UNITS).is_none(), "terminates instead of spinning");
    }

    #[test]
    fn extreme_extents_are_rejected_instead_of_gridded() {
        // A glyph of 10^12 units (what a tiny units-per-em plus extreme coordinates produce).
        let huge = square(0.0, 0.0, 1.0e12, 1.0e12);
        let small = square(0.0, 0.0, 100.0, 100.0);
        assert!(blend(&[&huge, &small], &[0.5, 0.5], &[500.0, 500.0]).is_err());
        // Normal outlines, but an extreme advance ratio scales the natural alignment by 10^9;
        // the ink-box alignments stay in range, so the glyph still blends.
        let a = square(0.0, 0.0, 400.0, 700.0);
        let b = square(50.0, 0.0, 350.0, 700.0);
        let blended = blend(&[&a, &b], &[0.5, 0.5], &[1.0, 2.0e9]).expect("an in-range alignment remains");
        assert!(!blended.is_empty());
        // Non-finite coordinates never reach the grid.
        let broken = square(0.0, 0.0, f64::INFINITY, 100.0);
        assert!(blend(&[&broken, &small], &[0.5, 0.5], &[500.0, 500.0]).is_err());
    }
}
