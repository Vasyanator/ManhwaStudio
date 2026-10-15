/*
File: crates/ms-custom-fonts/src/hybrid/trace.rs

Purpose:
Turns dense closed polylines — the zero-level loops of the blended distance field (`sdf.rs`)
and the interpolated contours of the matched path (`matched.rs`) — into smooth outline
contours.

Key functions:
- `fit_loops`: drops noise loops, splits each loop at corners and fits every corner-to-corner
  run with few cubic Béziers (or a line when the run is straight).
- `fit_run`: least-squares cubic fitting with fixed end tangents (Schneider, "An Algorithm
  for Automatically Fitting Digitized Curves", Graphics Gems 1990): chord-length
  parameters, a few Newton reparameterizations, split at the worst point otherwise.

Notes:
- Corners: the turn between the chords to the points `CORNER_WINDOW_CELLS` of arc length
  behind and ahead of a vertex; a cosine below `CORNER_MIN_TURN_COS` makes the vertex a
  corner candidate, and only the sharpest candidate of a cluster is kept.
- Tangents are measured over `TANGENT_WINDOW_CELLS` of arc length, not between direct
  neighbours, so the sub-cell noise of the level set does not tilt them.
- Handle lengths are bounded (`MAX_HANDLE_RATIO` of the chord, never <= 0): a degenerate
  least-squares solution falls back to the chord/3 heuristic, so no fitted control point can
  leave the neighbourhood of its run (`kurbo::simplify_bezpath` did, on near-cusps).
- Iterative (explicit stack), so long loops cannot overflow a worker's stack.
- The output keeps the polylines' orientation (outer contours clockwise, TrueType).
- Constants are part of the engine's output semantics (`ENGINE_VERSION`). No randomness, no
  order-dependent reductions: the same build gives the same bytes.
*/

use kurbo::{Point, Vec2};

use crate::outline::{Contour, Outline, Seg};

/// Max distance (font units) between the fitted curve and the level-set points.
const FIT_TOLERANCE: f64 = 0.75;
/// Arc length (in cells) of the chords that measure a vertex's turn.
const CORNER_WINDOW_CELLS: f64 = 2.0;
/// A vertex whose chord directions have a cosine below this is a corner (about 35 degrees).
const CORNER_MIN_TURN_COS: f64 = 0.82;
/// Arc length (in cells) over which a tangent direction is measured.
const TANGENT_WINDOW_CELLS: f64 = 2.0;
/// Loops with an area below this many square cells are level-set noise and dropped.
const MIN_LOOP_AREA_CELLS: f64 = 1.0;
/// Newton reparameterization rounds before a run is split.
const REPARAMETERIZE_ROUNDS: usize = 4;
/// Longest allowed Bézier handle as a multiple of its segment's chord.
const MAX_HANDLE_RATIO: f64 = 1.5;

/// Fits every loop (oriented polylines in font units) into curve contours. `cell` is the
/// polylines' sampling scale: it sizes the corner and tangent windows and the noise-loop area.
pub(crate) fn fit_loops(loops: Vec<Vec<Point>>, cell: f64) -> Outline {
    let mut outline = Vec::with_capacity(loops.len());
    for polyline in loops {
        let points = dedupe(polyline, cell * 1e-3);
        if points.len() < 3 || polygon_area(&points).abs() < MIN_LOOP_AREA_CELLS * cell * cell {
            continue;
        }
        outline.push(fit_loop(&points, cell));
    }
    outline
}

/// Removes consecutive points closer than `epsilon` (cyclic).
fn dedupe(points: Vec<Point>, epsilon: f64) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for point in points {
        if out.last().is_none_or(|last: &Point| last.distance(point) > epsilon) {
            out.push(point);
        }
    }
    while out.len() > 1 && out[0].distance(out[out.len() - 1]) <= epsilon {
        out.pop();
    }
    out
}

/// Shoelace area (negative = clockwise).
fn polygon_area(points: &[Point]) -> f64 {
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

/// Fits one closed polyline (at least 3 points).
fn fit_loop(points: &[Point], cell: f64) -> Contour {
    let count = points.len();
    let corners = find_corners(points, cell);
    let mut segs = Vec::new();
    if corners.is_empty() {
        // Smooth closed loop: one run from point 0 around back to point 0, with the cyclic
        // tangent at point 0 at both ends.
        let run: Vec<Point> = (0..=count).map(|index| points[index % count]).collect();
        let window = TANGENT_WINDOW_CELLS * cell;
        let tangent = unit(walk(points, 0, window, true) - walk(points, 0, window, false));
        fit_run(&run, tangent, -tangent, cell, &mut segs);
        return Contour { start: points[0], segs: merge_lines(points[0], segs) };
    }
    for (run_index, &start) in corners.iter().enumerate() {
        let end = corners.get(run_index + 1).copied().unwrap_or(corners[0] + count);
        let run: Vec<Point> = (start..=end).map(|index| points[index % count]).collect();
        let (t0, t1) = run_end_tangents(&run, cell);
        fit_run(&run, t0, t1, cell, &mut segs);
    }
    let start = points[corners[0]];
    Contour { start, segs: merge_lines(start, segs) }
}

/// Merges runs of consecutive lines into one line while every dropped joint stays within
/// `FIT_TOLERANCE` of the merged chord (the fitter emits a straight side split at its
/// reparameterization points as several collinear lines).
fn merge_lines(start: Point, segs: Vec<Seg>) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::with_capacity(segs.len());
    // Start of the line run being extended and the joints it has absorbed.
    let mut run_start = start;
    let mut joints: Vec<Point> = Vec::new();
    let mut previous_end = start;
    for seg in segs {
        if let (Seg::Line(end), Some(Seg::Line(last_end))) = (seg, out.last().copied()) {
            let mut candidate = joints.clone();
            candidate.push(last_end);
            if candidate.iter().all(|&joint| distance_to_segment(joint, run_start, end) <= FIT_TOLERANCE) {
                joints = candidate;
                if let Some(last) = out.last_mut() {
                    *last = Seg::Line(end);
                }
                previous_end = end;
                continue;
            }
        }
        if matches!(seg, Seg::Line(_)) {
            run_start = previous_end;
            joints.clear();
        }
        previous_end = seg.end();
        out.push(seg);
    }
    out
}

/// Unit tangents at the two ends of an open run: forward at the start, backward at the end,
/// each measured over the tangent window (at most a third of the run).
fn run_end_tangents(run: &[Point], cell: f64) -> (Vec2, Vec2) {
    let length: f64 = run.windows(2).map(|pair| pair[0].distance(pair[1])).sum();
    let window = (TANGENT_WINDOW_CELLS * cell).min(length / 3.0);
    let last = run.len() - 1;
    let ahead = point_at_arc(run, 0, window, true);
    let behind = point_at_arc(run, last, window, false);
    (unit(ahead - run[0]), unit(behind - run[last]))
}

/// The first point of an OPEN polyline at least `distance` of arc length from `points[index]`
/// in the given direction (the last point reached at the polyline's end).
fn point_at_arc(points: &[Point], index: usize, distance: f64, forward: bool) -> Point {
    let mut travelled = 0.0;
    let mut current = index;
    loop {
        let next = if forward {
            if current + 1 >= points.len() {
                break;
            }
            current + 1
        } else {
            if current == 0 {
                break;
            }
            current - 1
        };
        travelled += points[current].distance(points[next]);
        current = next;
        if travelled >= distance {
            break;
        }
    }
    points[current]
}

/// Unit vector (zero stays zero).
fn unit(vector: Vec2) -> Vec2 {
    let length = vector.hypot();
    if length > 0.0 { vector / length } else { Vec2::ZERO }
}

/// Indices of corner vertices of a closed polyline, ascending.
fn find_corners(points: &[Point], cell: f64) -> Vec<usize> {
    let count = points.len();
    let window = CORNER_WINDOW_CELLS * cell;
    // Turn cosine per vertex (1 = straight).
    let turn: Vec<f64> = (0..count)
        .map(|index| {
            let behind = walk(points, index, window, false);
            let ahead = walk(points, index, window, true);
            unit(points[index] - behind).dot(unit(ahead - points[index]))
        })
        .collect();
    let mut corners = Vec::new();
    for index in 0..count {
        if turn[index] >= CORNER_MIN_TURN_COS {
            continue;
        }
        // Keep only the sharpest vertex among neighbours within the window (ties: lowest index).
        let mut is_peak = true;
        'directions: for forward in [false, true] {
            let mut distance = 0.0;
            let mut current = index;
            while distance < window {
                let next = if forward { (current + 1) % count } else { (current + count - 1) % count };
                distance += points[current].distance(points[next]);
                current = next;
                if current == index {
                    break;
                }
                let sharper = match turn[current].total_cmp(&turn[index]) {
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Equal => current < index,
                    std::cmp::Ordering::Greater => false,
                };
                if sharper {
                    is_peak = false;
                    break 'directions;
                }
            }
        }
        if is_peak {
            corners.push(index);
        }
    }
    corners
}

/// The first point at least `distance` of arc length away from `points[index]` on a CLOSED
/// polyline (forward or backward); the farthest point reached when the loop is shorter.
fn walk(points: &[Point], index: usize, distance: f64, forward: bool) -> Point {
    let count = points.len();
    let mut travelled = 0.0;
    let mut current = index;
    for _ in 0..count / 2 {
        let next = if forward { (current + 1) % count } else { (current + count - 1) % count };
        travelled += points[current].distance(points[next]);
        current = next;
        if travelled >= distance {
            break;
        }
    }
    points[current]
}

/// A pending piece of a run: index range (inclusive) and its end tangents.
struct Piece {
    first: usize,
    last: usize,
    t0: Vec2,
    t1: Vec2,
}

/// Fits the open run `points` (first and last are kept exactly) with end unit tangents `t0`
/// (forward) and `t1` (backward), appending segments in order.
fn fit_run(points: &[Point], t0: Vec2, t1: Vec2, cell: f64, out: &mut Vec<Seg>) {
    if points.len() < 2 {
        return;
    }
    let first_point = points[0];
    let last_point = points[points.len() - 1];
    if first_point != last_point && points.iter().all(|&point| distance_to_segment(point, first_point, last_point) <= FIT_TOLERANCE) {
        out.push(Seg::Line(last_point));
        return;
    }
    // Explicit stack: the right piece is pushed first so pieces come out in order.
    let mut stack = vec![Piece { first: 0, last: points.len() - 1, t0, t1 }];
    while let Some(piece) = stack.pop() {
        let slice = &points[piece.first..=piece.last];
        let (cubic, split) = fit_piece(slice, piece.t0, piece.t1);
        if let Some(local_split) = split {
            let split = piece.first + local_split;
            let window = TANGENT_WINDOW_CELLS * cell;
            let center = unit(point_at_arc(slice, local_split, window, true) - point_at_arc(slice, local_split, window, false));
            stack.push(Piece { first: split, last: piece.last, t0: center, t1: piece.t1 });
            stack.push(Piece { first: piece.first, last: split, t0: piece.t0, t1: -center });
        } else {
            out.push(cubic_or_line(cubic));
        }
    }
}

/// A cubic as `[p0, c1, c2, p3]`.
type Cubic = [Point; 4];

/// Fits one cubic to `points`; returns it with `None`, or a split index (inside the slice)
/// when the error stays above tolerance.
fn fit_piece(points: &[Point], t0: Vec2, t1: Vec2) -> (Cubic, Option<usize>) {
    let count = points.len();
    let (p0, p3) = (points[0], points[count - 1]);
    if count == 2 {
        let handle = p0.distance(p3) / 3.0;
        return ([p0, p0 + t0 * handle, p3 + t1 * handle, p3], None);
    }
    let mut params = chord_params(points);
    let mut cubic = generate(points, &params, t0, t1);
    let (mut error, mut worst) = max_error(points, &params, &cubic);
    if error <= FIT_TOLERANCE * FIT_TOLERANCE {
        return (cubic, None);
    }
    if error <= 16.0 * FIT_TOLERANCE * FIT_TOLERANCE {
        for _ in 0..REPARAMETERIZE_ROUNDS {
            reparameterize(points, &mut params, &cubic);
            cubic = generate(points, &params, t0, t1);
            (error, worst) = max_error(points, &params, &cubic);
            if error <= FIT_TOLERANCE * FIT_TOLERANCE {
                return (cubic, None);
            }
        }
    }
    (cubic, Some(worst.clamp(1, count - 2)))
}

/// Chord-length parameters in `[0, 1]`.
fn chord_params(points: &[Point]) -> Vec<f64> {
    let mut params = Vec::with_capacity(points.len());
    let mut total = 0.0;
    params.push(0.0);
    for pair in points.windows(2) {
        total += pair[0].distance(pair[1]);
        params.push(total);
    }
    if total > 0.0 {
        for param in &mut params {
            *param /= total;
        }
    }
    params
}

/// Bernstein basis of a cubic at `t`.
fn bernstein(t: f64) -> [f64; 4] {
    let mt = 1.0 - t;
    [mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t]
}

/// Least-squares handle lengths along the fixed tangents; bounded, never degenerate.
fn generate(points: &[Point], params: &[f64], t0: Vec2, t1: Vec2) -> Cubic {
    let (p0, p3) = (points[0], points[points.len() - 1]);
    let mut c = [[0.0_f64; 2]; 2];
    let mut x = [0.0_f64; 2];
    for (point, &t) in points.iter().zip(params) {
        let basis = bernstein(t);
        let a1 = t0 * basis[1];
        let a2 = t1 * basis[2];
        c[0][0] += a1.dot(a1);
        c[0][1] += a1.dot(a2);
        c[1][1] += a2.dot(a2);
        let base = p0.to_vec2() * (basis[0] + basis[1]) + p3.to_vec2() * (basis[2] + basis[3]);
        let residual = point.to_vec2() - base;
        x[0] += a1.dot(residual);
        x[1] += a2.dot(residual);
    }
    // For a closed smooth loop the chord is 0: bound the handles by the run's extent instead.
    let span = points.iter().map(|point| point.distance(p0)).fold(p0.distance(p3), f64::max);
    let det = c[0][0] * c[1][1] - c[0][1] * c[0][1];
    let (mut alpha0, mut alpha1) =
        if det.abs() > 1e-12 { ((x[0] * c[1][1] - x[1] * c[0][1]) / det, (c[0][0] * x[1] - c[0][1] * x[0]) / det) } else { (0.0, 0.0) };
    let low = 1e-6 * span;
    let high = MAX_HANDLE_RATIO * span;
    if !(alpha0 > low && alpha0 < high && alpha1 > low && alpha1 < high) {
        alpha0 = span / 3.0;
        alpha1 = span / 3.0;
    }
    [p0, p0 + t0 * alpha0, p3 + t1 * alpha1, p3]
}

/// Cubic point at `t`.
fn evaluate(cubic: &Cubic, t: f64) -> Point {
    let basis = bernstein(t);
    let vector = cubic[0].to_vec2() * basis[0] + cubic[1].to_vec2() * basis[1] + cubic[2].to_vec2() * basis[2] + cubic[3].to_vec2() * basis[3];
    vector.to_point()
}

/// Largest squared distance between points and the curve at their parameters, and its index.
fn max_error(points: &[Point], params: &[f64], cubic: &Cubic) -> (f64, usize) {
    let mut worst = (0.0, points.len() / 2);
    for (index, (point, &t)) in points.iter().zip(params).enumerate().skip(1).take(points.len().saturating_sub(2)) {
        let squared = evaluate(cubic, t).distance_squared(*point);
        if squared > worst.0 {
            worst = (squared, index);
        }
    }
    worst
}

/// One Newton step per parameter towards the closest curve point.
fn reparameterize(points: &[Point], params: &mut [f64], cubic: &Cubic) {
    let d1 = [(cubic[1] - cubic[0]) * 3.0, (cubic[2] - cubic[1]) * 3.0, (cubic[3] - cubic[2]) * 3.0];
    let d2 = [(d1[1] - d1[0]) * 2.0, (d1[2] - d1[1]) * 2.0];
    for (point, param) in points.iter().zip(params.iter_mut()) {
        let t = *param;
        let mt = 1.0 - t;
        let q = evaluate(cubic, t);
        let q1 = d1[0] * (mt * mt) + d1[1] * (2.0 * mt * t) + d1[2] * (t * t);
        let q2 = d2[0] * mt + d2[1] * t;
        let diff = q - *point;
        let denominator = q1.dot(q1) + diff.dot(q2);
        if denominator.abs() > 1e-12 {
            *param = (t - diff.dot(q1) / denominator).clamp(0.0, 1.0);
        }
    }
}

/// Emits a nearly straight cubic as a line.
fn cubic_or_line(cubic: Cubic) -> Seg {
    let [p0, c1, c2, p3] = cubic;
    if p0 != p3 && distance_to_segment(c1, p0, p3) <= FIT_TOLERANCE * 0.5 && distance_to_segment(c2, p0, p3) <= FIT_TOLERANCE * 0.5 {
        Seg::Line(p3)
    } else {
        Seg::Cubic(c1, c2, p3)
    }
}

/// Distance from `point` to the segment `a`-`b`.
fn distance_to_segment(point: Point, a: Point, b: Point) -> f64 {
    let ab = b - a;
    let length2 = ab.hypot2();
    let t = if length2 > 0.0 { ((point - a).dot(ab) / length2).clamp(0.0, 1.0) } else { 0.0 };
    point.distance(a + ab * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn circle(radius: f64, count: u32) -> Vec<Point> {
        // Clockwise sampling (test geometry only).
        (0..count)
            .map(|index| {
                let angle = -2.0 * std::f64::consts::PI * f64::from(index) / f64::from(count);
                Point::new(500.0 + radius * angle.cos(), 400.0 + radius * angle.sin())
            })
            .collect()
    }

    #[test]
    fn a_circle_fits_with_few_smooth_segments_close_to_the_points() {
        let outline = fit_loops(vec![circle(200.0, 600)], 2.0);
        assert_eq!(outline.len(), 1);
        let segs = &outline[0].segs;
        assert!(segs.len() <= 12, "a circle needs few cubics, got {}", segs.len());
        for seg in segs {
            if let Seg::Cubic(c1, c2, end) = seg {
                for point in [c1, c2, end] {
                    let radius = point.distance(Point::new(500.0, 400.0));
                    assert!(radius < 260.0, "control point too far: {radius}");
                }
            }
        }
    }

    #[test]
    fn a_square_keeps_its_corners_and_straight_sides() {
        let mut points = Vec::new();
        for step in 0..100 {
            points.push(Point::new(0.0, f64::from(step) * 2.0));
        }
        for step in 0..100 {
            points.push(Point::new(f64::from(step) * 2.0, 200.0));
        }
        for step in 0..100 {
            points.push(Point::new(200.0, 200.0 - f64::from(step) * 2.0));
        }
        for step in 0..100 {
            points.push(Point::new(200.0 - f64::from(step) * 2.0, 0.0));
        }
        let outline = fit_loops(vec![points], 2.0);
        assert_eq!(outline[0].segs.len(), 4);
        assert!(outline[0].segs.iter().all(|seg| matches!(seg, Seg::Line(_))));
    }
}
