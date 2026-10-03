/*
File: crates/ms-raster/src/polygon.rs

Purpose:
Even-odd scanline rasterization of a closed polygon into horizontal spans. The primitive is
buffer-agnostic: it emits clamped `(y, x0, x1)` spans and lets the caller decide what to write
(a `u8` selection mask, a `Vec<bool>` region mask, an accumulator, ...).

Key functions:
- `fill_polygon_spans()`: the rasterizer.

Notes:
This is the single implementation of lasso/polygon filling shared by the PS-editor selection
(`crates/ms-tab-ps-editor/src/selection.rs`), the cleaning tools and the `ms-tools` patch core
(all reach it through the `ms_tools::fill_polygon_spans` re-export). Its numeric behavior is a contract,
not an implementation detail: two callers rasterizing the same polygon into differently sized
buffers must agree pixel for pixel, so the sampling rule (scanline centre, even-odd, inclusive
`ceil`/`floor` span ends) must not be "improved" without updating every caller and its tests.
*/

/// Rasterizes a closed polygon with the even-odd rule and reports the filled spans.
///
/// `points` are image-space vertices of an implicitly closed polygon (the last vertex is joined
/// back to the first); fewer than three points emits nothing, and so does a zero-sized buffer.
///
/// For every scanline the closure is called as `span(y, x0, x1)` with `x0..=x1` **inclusive**,
/// already clamped to `0..width` / `0..height`, in increasing `y`, and never with `x1 < x0`.
/// A scanline may produce several spans (a concave or self-intersecting polygon), reported in
/// increasing `x0`. Insideness is sampled at the scanline centre `y + 0.5` under the even-odd
/// rule, so a doubly wound part of a self-intersecting polygon stays unfilled.
///
/// The polygon may extend outside the buffer on any side; only the visible part is reported.
/// Performance: one allocation for the crossing list, reused across scanlines.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "the saturating float<->int casts below are the documented sampling contract; each is bounded or clamped as its inline comment explains"
)]
pub fn fill_polygon_spans(points: &[(f32, f32)], width: usize, height: usize, mut span: impl FnMut(usize, usize, usize)) {
    if points.len() < 3 || width == 0 || height == 0 {
        return;
    }
    // The scanline loop runs in `i32` because polygon vertices may sit far outside the buffer on
    // either side. `try_from` instead of `as`: a `usize -> i32` cast would wrap, and saturating at
    // `i32::MAX` is the correct clamp for a buffer that large (it cannot be exceeded anyway).
    let height_i32 = i32::try_from(height).unwrap_or(i32::MAX);
    // `f32 as i32` is saturating in Rust, so out-of-range vertex coordinates clamp instead of
    // wrapping; the explicit `clamp` then confines the sweep to the buffer.
    let min_y = points.iter().map(|p| p.1.floor() as i32).min().unwrap_or(0).clamp(0, height_i32);
    let max_y = points.iter().map(|p| p.1.ceil() as i32).max().unwrap_or(0).clamp(0, height_i32);
    let max_x_f = width as f32 - 1.0;
    let mut crossings: Vec<f32> = Vec::new();
    for y in min_y..max_y {
        // Scanline center; test edges crossing this horizontal line (even-odd rule).
        let yc = y as f32 + 0.5;
        crossings.clear();
        for i in 0..points.len() {
            let (x0, y0) = points[i];
            let (x1, y1) = points[(i + 1) % points.len()];
            // Half-open vertical test (`<=` on one end, `>` on the other): a vertex that lies
            // exactly on the scanline is counted once, not twice, and horizontal edges are
            // skipped entirely. This is what keeps the parity correct at shared vertices.
            if (y0 <= yc && y1 > yc) || (y1 <= yc && y0 > yc) {
                let t = (yc - y0) / (y1 - y0);
                crossings.push(x0 + t * (x1 - x0));
            }
        }
        crossings.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let row = y as usize; // `y >= min_y >= 0` and `y < max_y <= height`, so the cast is exact.
        for pair in crossings.chunks_exact(2) {
            // Pixel centres inside the crossing interval: the first integer >= entry and the last
            // integer <= exit. Clamping happens in the float domain on purpose — `f32::min`/`max`
            // return the non-NaN operand, so a degenerate (infinite/NaN) edge still clamps into
            // the buffer instead of collapsing to 0 as an integer-domain clamp would.
            let sx = pair[0].ceil().max(0.0) as i32;
            let ex = pair[1].floor().min(max_x_f) as i32;
            if ex < sx {
                continue;
            }
            span(row, sx as usize, ex as usize);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rasterizes into a `width * height` boolean buffer and additionally asserts the span
    /// contract (in-bounds, non-inverted, non-decreasing `y`).
    fn raster(points: &[(f32, f32)], width: usize, height: usize) -> Vec<bool> {
        let mut buf = vec![false; width * height];
        let mut last_y: Option<usize> = None;
        fill_polygon_spans(points, width, height, |y, x0, x1| {
            assert!(y < height, "span row {y} out of bounds");
            assert!(x0 <= x1, "inverted span {x0}..={x1}");
            assert!(x1 < width, "span end {x1} out of bounds");
            assert!(last_y.is_none_or(|prev| prev <= y), "spans not in increasing y order");
            last_y = Some(y);
            for x in x0..=x1 {
                buf[y * width + x] = true;
            }
        });
        buf
    }

    #[test]
    fn triangle_fills_interior_only() {
        let buf = raster(&[(1.0, 1.0), (8.0, 1.0), (1.0, 8.0)], 10, 10);
        let at = |x: usize, y: usize| buf[y * 10 + x];
        assert!(at(2, 2), "interior pixel must be filled");
        assert!(at(2, 6), "interior pixel near the hypotenuse must be filled");
        assert!(!at(7, 7), "pixel beyond the hypotenuse must stay empty");
        assert!(!at(0, 0), "pixel outside the triangle must stay empty");
        // The hypotenuse runs x = 8 - 7*(y-1)/7; at y=2 (centre 2.5) it exits at x = 6.5.
        assert!(at(6, 2), "last pixel centre inside the span must be filled");
        assert!(!at(7, 2), "first pixel centre past the exit must stay empty");
    }

    #[test]
    fn concave_u_shape_leaves_the_notch_empty() {
        // A "U": left arm x in 1..=3, right arm x in 6..=8, bottom bar spanning both.
        let u = [(1.0, 1.0), (3.0, 1.0), (3.0, 6.0), (6.0, 6.0), (6.0, 1.0), (8.0, 1.0), (8.0, 8.0), (1.0, 8.0)];
        let buf = raster(&u, 9, 9);
        let at = |x: usize, y: usize| buf[y * 9 + x];
        // Row 3 crosses the notch: a convex/hull fill would wrongly fill x = 4..5 here.
        assert!(at(2, 3), "left arm must be filled");
        assert!(at(7, 3), "right arm must be filled");
        assert!(!at(4, 3), "notch must stay empty");
        assert!(!at(5, 3), "notch must stay empty");
        // Row 7 is below the notch and spans the whole bar.
        assert!(at(4, 7), "bottom bar must be filled across the notch columns");
        assert!(at(1, 7) && at(8, 7), "bottom bar must reach both arms");
    }

    #[test]
    fn polygon_larger_than_the_buffer_is_clipped_on_every_side() {
        // Rectangle overhanging all four edges: every pixel is inside, nothing may escape bounds.
        let buf = raster(&[(-3.0, -3.0), (8.0, -3.0), (8.0, 8.0), (-3.0, 8.0)], 5, 5);
        assert!(buf.iter().all(|&v| v), "an all-covering polygon must fill the whole buffer");
    }

    #[test]
    fn polygon_overhanging_two_sides_fills_only_the_visible_part() {
        // Rectangle hanging off the top-left corner, ending at x = 2.0 / y = 2.0.
        let buf = raster(&[(-2.0, -2.0), (2.0, -2.0), (2.0, 2.0), (-2.0, 2.0)], 5, 5);
        let at = |x: usize, y: usize| buf[y * 5 + x];
        assert!(at(0, 0) && at(2, 0) && at(0, 1) && at(2, 1), "visible part must be filled");
        assert!(!at(3, 0), "column past the right edge of the polygon must stay empty");
        assert!(!at(0, 2), "row past the bottom edge of the polygon must stay empty");
    }

    #[test]
    fn degenerate_inputs_emit_nothing() {
        for pts in [&[][..], &[(1.0, 1.0)][..], &[(1.0, 1.0), (5.0, 5.0)][..]] {
            let mut calls = 0usize;
            fill_polygon_spans(pts, 8, 8, |_, _, _| calls += 1);
            assert_eq!(calls, 0, "fewer than three vertices must emit nothing");
        }
        // A zero-sized buffer is a no-op even for a valid polygon.
        let square = [(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)];
        let mut calls = 0usize;
        fill_polygon_spans(&square, 0, 8, |_, _, _| calls += 1);
        fill_polygon_spans(&square, 8, 0, |_, _, _| calls += 1);
        assert_eq!(calls, 0, "a zero-sized buffer must emit nothing");
    }

    #[test]
    fn self_intersecting_polygon_leaves_the_doubly_wound_part_empty() {
        // One loop that traces an outer square and then an inner square in the same direction;
        // the inner area is wound twice, which the even-odd rule excludes.
        let keyhole = [(1.0, 1.0), (9.0, 1.0), (9.0, 9.0), (1.0, 9.0), (3.0, 3.0), (7.0, 3.0), (7.0, 7.0), (3.0, 7.0)];
        let buf = raster(&keyhole, 12, 12);
        let at = |x: usize, y: usize| buf[y * 12 + x];
        assert!(!at(5, 5), "doubly wound centre must stay empty under the even-odd rule");
        assert!(at(8, 5), "singly wound ring between the squares must be filled");
        assert!(at(5, 2), "singly wound band above the inner square must be filled");
    }
}
