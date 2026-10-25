/*
File: crates/ms-custom-fonts/src/outline.rs

Purpose:
The crate's glyph outline model in normalized font units (`TARGET_UPM`, y up, `f64`) and the
conversions around it: reading from `ttf-parser`, contour reversal, flattening to polygons,
and the final quantization into TrueType quadratic contours.

Key structures:
- `Seg`, `Contour`, `Outline`: contours of line / quadratic / cubic segments; a contour is
  implicitly closed by a line from its last point back to `start`.
- `OutlineCollector`: `ttf_parser::OutlineBuilder` that scales into normalized units.
- `TtPoint` / `TtContour`: quantized TrueType points (on/off-curve, `i16`).

Key functions:
- `reverse_contour`, `flatten_contour_within` (point-budgeted; `flatten_contour`, its unbudgeted
  form, is the test-only reference).
- `to_truetype`: cubic -> quadratic (`kurbo`), rounding with range checks, cleanup of
  degenerate points and contours.

Notes:
Flattening uses uniform subdivision with a count from the control-polygon deviation, so the
polygon depends only on the curve (no adaptive error search).
*/

use kurbo::{CubicBez, Point};

use crate::error::CompileError;

/// Units per em of every compiled face; every source is scaled to it.
///
/// 1000 because user kerning is stored in thousandths of an em (so a kerning value in font
/// units IS the per-mille value) and the common sources (Noto, Source Han) are already 1000
/// units, which makes the "single source" paths exact.
pub(crate) const TARGET_UPM: u16 = 1000;

/// Accuracy (font units) of the cubic -> quadratic conversion of the output.
const CUBIC_TO_QUAD_ACCURACY: f64 = 0.5;

/// One outline segment, starting at the previous segment's end (or the contour start).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Seg {
    /// Straight line to the point.
    Line(Point),
    /// Quadratic Bézier: control, end.
    Quad(Point, Point),
    /// Cubic Bézier: control 1, control 2, end.
    Cubic(Point, Point, Point),
}

impl Seg {
    /// End point of the segment.
    pub(crate) fn end(&self) -> Point {
        match *self {
            Self::Line(end) | Self::Quad(_, end) | Self::Cubic(_, _, end) => end,
        }
    }

    /// Structural kind (0 line, 1 quad, 2 cubic) used by the compatibility check.
    pub(crate) fn kind(&self) -> u8 {
        match self {
            Self::Line(_) => 0,
            Self::Quad(..) => 1,
            Self::Cubic(..) => 2,
        }
    }
}

/// A closed contour: `start` then `segs`; closed by an implicit line back to `start`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Contour {
    /// First on-curve point.
    pub start: Point,
    /// Segments in drawing order.
    pub segs: Vec<Seg>,
}

/// A glyph outline: zero or more contours (nonzero fill rule).
pub(crate) type Outline = Vec<Contour>;

/// `ttf_parser::OutlineBuilder` collecting an `Outline`, scaling every coordinate by `scale`
/// (source units -> `TARGET_UPM`). Composite glyphs arrive already decomposed.
pub(crate) struct OutlineCollector {
    scale: f64,
    contours: Outline,
    current: Option<Contour>,
}

impl OutlineCollector {
    /// New collector with the source's unit scale.
    pub(crate) fn new(scale: f64) -> Self {
        Self { scale, contours: Vec::new(), current: None }
    }

    /// The collected outline (an unclosed trailing contour is closed implicitly).
    pub(crate) fn finish(mut self) -> Outline {
        self.flush();
        self.contours
    }

    fn point(&self, x: f32, y: f32) -> Point {
        Point::new(f64::from(x) * self.scale, f64::from(y) * self.scale)
    }

    fn flush(&mut self) {
        if let Some(contour) = self.current.take()
            && !contour.segs.is_empty()
        {
            self.contours.push(contour);
        }
    }

    fn push(&mut self, seg: Seg) {
        // A segment without a preceding move starts at the origin (malformed data, but drawable).
        let contour = self.current.get_or_insert_with(|| Contour { start: Point::ZERO, segs: Vec::new() });
        contour.segs.push(seg);
    }
}

impl ttf_parser::OutlineBuilder for OutlineCollector {
    fn move_to(&mut self, x: f32, y: f32) {
        self.flush();
        self.current = Some(Contour { start: self.point(x, y), segs: Vec::new() });
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let end = self.point(x, y);
        self.push(Seg::Line(end));
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let seg = Seg::Quad(self.point(x1, y1), self.point(x, y));
        self.push(seg);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let seg = Seg::Cubic(self.point(x1, y1), self.point(x2, y2), self.point(x, y));
        self.push(seg);
    }

    fn close(&mut self) {
        self.flush();
    }
}

/// Reverses a contour's direction, keeping its start point.
pub(crate) fn reverse_contour(contour: &Contour) -> Contour {
    let mut points = Vec::with_capacity(contour.segs.len() + 1);
    points.push(contour.start);
    points.extend(contour.segs.iter().map(Seg::end));
    let mut segs = Vec::with_capacity(contour.segs.len() + 1);
    // The implicit closing line becomes the first explicit segment of the reversed contour.
    if let Some(&last) = points.last()
        && last != contour.start
    {
        segs.push(Seg::Line(last));
    }
    for (index, seg) in contour.segs.iter().enumerate().rev() {
        let target = points[index];
        segs.push(match *seg {
            Seg::Line(_) => Seg::Line(target),
            Seg::Quad(control, _) => Seg::Quad(control, target),
            Seg::Cubic(control1, control2, _) => Seg::Cubic(control2, control1, target),
        });
    }
    Contour { start: contour.start, segs }
}

/// Flattens a contour into a closed polygon (the start is not repeated at the end) whose
/// deviation from the curves is at most about `tolerance` units.
///
/// Unbudgeted, so test-only: every runtime path flattens through `flatten_contour_within`
/// (via `hybrid::sdf::flatten_checked*`), which bounds the work a hostile outline can cause;
/// tests use this as the reference the budgeted form must equal.
#[cfg(test)]
pub(crate) fn flatten_contour(contour: &Contour, tolerance: f64, out: &mut Vec<Point>) {
    // With `usize::MAX` as the limit the check can never fire (`out` would exhaust memory long
    // before), so the result carries no information here.
    let unlimited = flatten_contour_within(contour, tolerance, usize::MAX, out);
    debug_assert!(unlimited.is_ok(), "an unlimited flatten cannot exceed its limit");
}

/// The flattening of `flatten_contour` exceeded the caller's point limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PointLimitExceeded;

/// `flatten_contour`, but checked against a point budget: the points of each segment are
/// counted BEFORE they are computed or pushed, so `out` never grows beyond `limit` points and
/// the work done is bounded by `limit` (plus one step count per segment). Within the limit the
/// output is identical to `flatten_contour`'s.
///
/// # Errors
/// `PointLimitExceeded` when the polygon would need more than `limit` points; `out` then holds
/// a partial polygon of at most `limit` points that the caller must discard.
pub(crate) fn flatten_contour_within(contour: &Contour, tolerance: f64, limit: usize, out: &mut Vec<Point>) -> Result<(), PointLimitExceeded> {
    out.clear();
    let reserve = |out: &Vec<Point>, needed: usize| if out.len().saturating_add(needed) > limit { Err(PointLimitExceeded) } else { Ok(()) };
    reserve(out, 1)?;
    out.push(contour.start);
    let mut current = contour.start;
    for seg in &contour.segs {
        match *seg {
            Seg::Line(end) => {
                reserve(out, 1)?;
                out.push(end);
            }
            Seg::Quad(control, end) => {
                // Max deviation of a uniform n-split is |p0 - 2p1 + p2| / (4 n^2).
                let deviation = (current.to_vec2() - 2.0 * control.to_vec2() + end.to_vec2()).hypot();
                let steps = subdivision_count(deviation / (4.0 * tolerance));
                reserve(out, usize::try_from(steps).unwrap_or(usize::MAX))?;
                for step in 1..steps {
                    let t = f64::from(step) / f64::from(steps);
                    let mt = 1.0 - t;
                    out.push(Point::new(
                        mt * mt * current.x + 2.0 * mt * t * control.x + t * t * end.x,
                        mt * mt * current.y + 2.0 * mt * t * control.y + t * t * end.y,
                    ));
                }
                out.push(end);
            }
            Seg::Cubic(control1, control2, end) => {
                let d1 = (current.to_vec2() - 2.0 * control1.to_vec2() + control2.to_vec2()).hypot();
                let d2 = (control1.to_vec2() - 2.0 * control2.to_vec2() + end.to_vec2()).hypot();
                let steps = subdivision_count(0.75 * d1.max(d2) / tolerance);
                reserve(out, usize::try_from(steps).unwrap_or(usize::MAX))?;
                for step in 1..steps {
                    let t = f64::from(step) / f64::from(steps);
                    let mt = 1.0 - t;
                    let basis = [mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t];
                    out.push(Point::new(
                        basis[0] * current.x + basis[1] * control1.x + basis[2] * control2.x + basis[3] * end.x,
                        basis[0] * current.y + basis[1] * control1.y + basis[2] * control2.y + basis[3] * end.y,
                    ));
                }
                out.push(end);
            }
        }
        current = seg.end();
    }
    // The closing point duplicates the start when the contour is explicitly closed.
    if out.len() > 1 && out.last() == Some(&contour.start) {
        out.pop();
    }
    Ok(())
}

/// `ceil(sqrt(ratio))` clamped to `1..=64`, as an exact small integer.
fn subdivision_count(ratio: f64) -> u32 {
    let root = if ratio.is_finite() && ratio > 0.0 { ratio.sqrt().ceil() } else { 1.0 };
    let mut steps = 1_u32;
    while steps < 64 && f64::from(steps) < root {
        steps += 1;
    }
    steps
}

/// A quantized TrueType point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TtPoint {
    /// X in font units.
    pub x: i16,
    /// Y in font units.
    pub y: i16,
    /// On-curve (`true`) or quadratic control point (`false`).
    pub on: bool,
}

/// A TrueType contour (implied on-curve midpoints between consecutive off-curve points).
pub(crate) type TtContour = Vec<TtPoint>;

/// Converts an outline to quantized TrueType contours: cubics become quadratic splines,
/// coordinates are rounded, duplicate/implied/degenerate points and degenerate contours are
/// removed. Orientation is kept as given.
///
/// # Errors
/// `CompileError::GlyphTooComplex` when a coordinate falls outside the `i16` range.
pub(crate) fn to_truetype(outline: &Outline, ch: char) -> Result<Vec<TtContour>, CompileError> {
    let mut contours = Vec::with_capacity(outline.len());
    for contour in outline {
        let mut raw: Vec<(Point, bool)> = Vec::with_capacity(contour.segs.len() * 2 + 1);
        raw.push((contour.start, true));
        let mut current = contour.start;
        for seg in &contour.segs {
            match *seg {
                Seg::Line(end) => raw.push((end, true)),
                Seg::Quad(control, end) => {
                    raw.push((control, false));
                    raw.push((end, true));
                }
                Seg::Cubic(control1, control2, end) => push_cubic(&mut raw, CubicBez::new(current, control1, control2, end)),
            }
            current = seg.end();
        }
        let mut points = raw
            .iter()
            .map(|&(point, on)| Ok(TtPoint { x: quantize(point.x, ch)?, y: quantize(point.y, ch)?, on }))
            .collect::<Result<Vec<_>, CompileError>>()?;
        clean_contour(&mut points);
        if points.len() >= 3 && twice_area(&points) != 0 {
            contours.push(points);
        }
    }
    Ok(contours)
}

/// Appends a cubic as a quadratic spline (off-curve controls, then the on-curve end).
fn push_cubic(raw: &mut Vec<(Point, bool)>, cubic: CubicBez) {
    if let Some(spline) = cubic.approx_spline(CUBIC_TO_QUAD_ACCURACY) {
        let points = spline.points();
        if let Some((last, inner)) = points.split_last()
            && let Some((_, controls)) = inner.split_first()
        {
            raw.extend(controls.iter().map(|&control| (control, false)));
            raw.push((*last, true));
            return;
        }
    }
    // No G1 spline within the split limit (extremely tight curve): independent quads, which
    // always meet the accuracy.
    for (_, _, quad) in cubic.to_quads(CUBIC_TO_QUAD_ACCURACY) {
        raw.push((quad.p1, false));
        raw.push((quad.p2, true));
    }
}

/// Rounds a coordinate to font units, rejecting values outside `i16`.
fn quantize(value: f64, ch: char) -> Result<i16, CompileError> {
    let rounded = value.round();
    if !(f64::from(i16::MIN)..=f64::from(i16::MAX)).contains(&rounded) {
        return Err(CompileError::GlyphTooComplex {
            ch,
            reason: format!("coordinate {value:.1} is outside the 16-bit range"),
        });
    }
    #[expect(clippy::cast_possible_truncation, reason = "range-checked against i16 just above")]
    Ok(rounded as i16)
}

/// Removes, until stable: consecutive points at the same position (merged; on-curve wins),
/// on-curve points that are the exact implied midpoint of two off-curve neighbours, and
/// off-curve points lying exactly on the chord between two on-curve neighbours.
fn clean_contour(points: &mut Vec<TtPoint>) {
    loop {
        let before = points.len();
        dedupe_positions(points);
        drop_redundant(points);
        if points.len() == before || points.len() < 3 {
            break;
        }
    }
    // The format allows an off-curve start, but readers are simpler with an on-curve one.
    if let Some(first_on) = points.iter().position(|point| point.on) {
        points.rotate_left(first_on);
    }
}

/// Cyclic merge of equal consecutive positions.
fn dedupe_positions(points: &mut Vec<TtPoint>) {
    let mut out: Vec<TtPoint> = Vec::with_capacity(points.len());
    for &point in points.iter() {
        match out.last_mut() {
            Some(last) if last.x == point.x && last.y == point.y => last.on |= point.on,
            _ => out.push(point),
        }
    }
    while out.len() > 1 {
        let (first, last) = (out[0], out[out.len() - 1]);
        if first.x == last.x && first.y == last.y {
            out[0].on |= last.on;
            out.pop();
        } else {
            break;
        }
    }
    *points = out;
}

/// One cyclic pass dropping implied midpoints and chord-collinear controls.
fn drop_redundant(points: &mut Vec<TtPoint>) {
    let count = points.len();
    if count < 3 {
        return;
    }
    let mut keep = vec![true; count];
    for index in 0..count {
        let prev = points[(index + count - 1) % count];
        let point = points[index];
        let next = points[(index + 1) % count];
        if !keep[(index + count - 1) % count] {
            // Decide against the original neighbours only once per pass.
            continue;
        }
        let (px, py, x, y, nx, ny) = (
            i32::from(prev.x),
            i32::from(prev.y),
            i32::from(point.x),
            i32::from(point.y),
            i32::from(next.x),
            i32::from(next.y),
        );
        let redundant = if point.on {
            !prev.on && !next.on && 2 * x == px + nx && 2 * y == py + ny
        } else {
            let cross = i64::from(x - px) * i64::from(ny - py) - i64::from(y - py) * i64::from(nx - px);
            prev.on
                && next.on
                && cross == 0
                && (px.min(nx)..=px.max(nx)).contains(&x)
                && (py.min(ny)..=py.max(ny)).contains(&y)
        };
        if redundant {
            keep[index] = false;
        }
    }
    let mut index = 0;
    points.retain(|_| {
        let kept = keep[index];
        index += 1;
        kept
    });
}

/// Twice the signed shoelace area of the control polygon (0 = degenerate).
pub(crate) fn twice_area(points: &[TtPoint]) -> i64 {
    let count = points.len();
    (0..count)
        .map(|index| {
            let a = points[index];
            let b = points[(index + 1) % count];
            i64::from(a.x) * i64::from(b.y) - i64::from(b.x) * i64::from(a.y)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> Contour {
        Contour {
            start: Point::new(0.0, 0.0),
            segs: vec![
                Seg::Line(Point::new(0.0, 100.0)),
                Seg::Line(Point::new(100.0, 100.0)),
                Seg::Line(Point::new(100.0, 0.0)),
            ],
        }
    }

    #[test]
    fn reverse_keeps_start_and_flips_area() {
        let reversed = reverse_contour(&square());
        assert_eq!(reversed.start, Point::new(0.0, 0.0));
        let forward = to_truetype(&vec![square()], 'x').expect("in range");
        let backward = to_truetype(&vec![reversed], 'x').expect("in range");
        assert_eq!(twice_area(&forward[0]), -twice_area(&backward[0]));
        assert!(twice_area(&forward[0]) < 0, "the square is clockwise (TrueType outer)");
    }

    #[test]
    fn truetype_cleanup_drops_degenerate_contours_and_points() {
        let flat = Contour { start: Point::new(0.0, 0.0), segs: vec![Seg::Line(Point::new(10.0, 0.0))] };
        assert_eq!(to_truetype(&vec![flat], 'x').expect("in range"), [] as [Vec<TtPoint>; 0]);

        let with_straight_quad = Contour {
            start: Point::new(0.0, 0.0),
            segs: vec![
                Seg::Quad(Point::new(0.0, 50.0), Point::new(0.0, 100.0)),
                Seg::Line(Point::new(100.0, 100.0)),
                Seg::Line(Point::new(100.0, 0.0)),
                Seg::Line(Point::new(0.0, 0.0)),
            ],
        };
        let contours = to_truetype(&vec![with_straight_quad], 'x').expect("in range");
        assert_eq!(contours[0].len(), 4, "collinear control and closing duplicate are removed");
        assert!(contours[0].iter().all(|point| point.on));

        let far = Contour { start: Point::new(0.0, 0.0), segs: vec![Seg::Line(Point::new(40_000.0, 0.0))] };
        assert!(matches!(to_truetype(&vec![far], 'x'), Err(CompileError::GlyphTooComplex { .. })));
    }

    #[test]
    fn flatten_stays_close_to_the_curve() {
        let arc = Contour {
            start: Point::new(0.0, 0.0),
            segs: vec![Seg::Cubic(Point::new(0.0, 552.0), Point::new(448.0, 1000.0), Point::new(1000.0, 1000.0))],
        };
        let mut polygon = Vec::new();
        flatten_contour(&arc, 0.1, &mut polygon);
        assert!(polygon.len() > 10);
        assert_eq!(polygon[0], Point::new(0.0, 0.0));
    }
}
