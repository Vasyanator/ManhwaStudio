/*
File: tabs/ps_editor/selection.rs

Purpose:
Image-space selection mask for the PS-like editor. A selection constrains painting to the marked
region (like a Photoshop marquee). Geometry is built by the selection tools; brushing reads it.

Key structures:
- `Selection`: page-sized binary mask (`0` = outside, `255` = inside), a tight bounding box, and the
  marquee outline traced from that mask.
- `SelectionBounds`: inclusive integer bounding box of the marked pixels.
- `SelectionOp`: Photoshop-style boolean combination mode (replace / add / subtract / intersect).

Key functions:
- `apply_rect()` / `apply_polygon()`: combine new geometry into the mask under a `SelectionOp`.
- `set_rect()` / `set_polygon()`: thin `SelectionOp::Replace` wrappers kept for existing callers.
- `outline_loops()`: closed marquee loops traced along the pixel edges of the mask.

Notes:
The mask uses image pixel coordinates. An empty selection (no pixels set) is represented by
`None` at the call site, never by an all-zero mask, so "no selection" means "paint everywhere".
Polygon (lasso) filling is not implemented here: it comes from the shared rasterizer
`crate::tools::fill_polygon_spans`, so the cleaning tools and this selection agree pixel for pixel.
The marquee is NOT the input path. After every op the outline is re-traced from the resulting MASK
along pixel edges, so the user sees exactly which pixels are selected (a staircase, not the smooth
lasso curve). Collinear boundary edges are merged, so a rectangle is still five points and a large
selection does not explode the per-frame screen-space path.
Every op preserves the invariant "the mask is all zero outside `bounds`". That is what lets Replace
clear only the previous bounding box and Subtract/Intersect touch only it, instead of sweeping a
whole ribbon page (800x19000 = ~15 MB of mask).
*/

use crate::tools::fill_polygon_spans;

/// Boundary walk directions on the vertex grid, in clockwise screen order (`y` grows downward).
/// The outer boundary of a filled region is walked right -> down -> left -> up, which reproduces
/// the vertex order `set_rect` emitted before the tracer existed.
const DIR_RIGHT: u8 = 0;
const DIR_DOWN: u8 = 1;
const DIR_LEFT: u8 = 2;
const DIR_UP: u8 = 3;

/// Inclusive integer bounding box in image pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct SelectionBounds {
    pub min_x: usize,
    pub min_y: usize,
    pub max_x: usize,
    pub max_y: usize,
}

/// How new geometry combines with the mask already in the selection (Photoshop's marquee modes).
///
/// Degenerate new geometry (fewer than three polygon points, or an empty rectangle) is handled
/// per mode: `Replace` and `Intersect` clear the selection, `Add` and `Subtract` leave it as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionOp {
    /// The mask becomes exactly the new geometry.
    #[default]
    Replace,
    /// Union: the new geometry is added to the mask.
    Add,
    /// Difference: the new geometry is removed from the mask.
    Subtract,
    /// Intersection: only pixels present in both the mask and the new geometry survive.
    Intersect,
}

/// Binary selection mask over a single page.
#[derive(Debug, Clone)]
pub struct Selection {
    width: usize,
    height: usize,
    mask: Vec<u8>,
    bounds: Option<SelectionBounds>,
    /// Closed boundary loops in image-pixel coordinates, used to draw the marching-ants marquee.
    /// Traced along the pixel edges of `mask`: empty when nothing is selected, one loop per
    /// connected component plus one per enclosed hole.
    outline: Vec<Vec<(f32, f32)>>,
}

impl Selection {
    /// Creates an empty selection sized to the page.
    #[must_use]
    pub fn empty(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            mask: vec![0u8; width.saturating_mul(height)],
            bounds: None,
            outline: Vec::new(),
        }
    }

    /// Tight bounding box of the selection, or `None` when nothing is selected.
    #[must_use]
    pub fn bounds(&self) -> Option<SelectionBounds> {
        self.bounds
    }

    /// Closed boundary loops (image-pixel coords) for drawing the selection marquee.
    ///
    /// Each loop runs along pixel edges: pixel `(x, y)` occupies `[x, x+1] x [y, y+1]`, so the
    /// marquee is a staircase that shows exactly which pixels are selected. Every loop repeats its
    /// first point as its last point (closed), consecutive collinear edges are merged, and a hole
    /// inside a selected region contributes its own loop. Empty when nothing is selected.
    #[must_use]
    pub fn outline_loops(&self) -> &[Vec<(f32, f32)>] {
        &self.outline
    }

    /// True when at least one pixel is selected.
    #[must_use]
    pub fn any(&self) -> bool {
        self.bounds.is_some()
    }

    /// Returns whether the image pixel `(x, y)` is inside the selection.
    #[must_use]
    pub fn contains(&self, x: usize, y: usize) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        self.mask[y * self.width + x] != 0
    }

    /// Combines an axis-aligned rectangle into the selection under `op`.
    ///
    /// The rectangle is inclusive of its `min` corner and exclusive of its `max` corner; the given
    /// corners may be in any order and are clamped to the page. An empty rectangle (after clamping)
    /// is degenerate geometry, handled per [`SelectionOp`]. `bounds` and `outline_loops` are
    /// recomputed from the resulting mask.
    pub fn apply_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, op: SelectionOp) {
        // `try_from` instead of `as`: a `usize -> i32` cast would wrap, and saturating at `i32::MAX`
        // is the correct clamp for a page that large (it cannot be exceeded anyway).
        let width_i32 = i32::try_from(self.width).unwrap_or(i32::MAX);
        let height_i32 = i32::try_from(self.height).unwrap_or(i32::MAX);
        let min_x = x0.min(x1).max(0);
        let min_y = y0.min(y1).max(0);
        let max_x = x0.max(x1).min(width_i32);
        let max_y = y0.max(y1).min(height_i32);
        if max_x <= min_x || max_y <= min_y {
            self.apply_spans(op, true, |_: &mut dyn FnMut(usize, usize, usize)| {});
            return;
        }
        // The four values are now inside `0..=width` / `0..=height`, so the casts are exact.
        let (rx0, ry0) = (min_x as usize, min_y as usize);
        let (rx1, ry1) = (max_x as usize, max_y as usize);
        self.apply_spans(op, false, |span: &mut dyn FnMut(usize, usize, usize)| {
            // Spans in increasing `y`, one per row, with an inclusive right end.
            for y in ry0..ry1 {
                span(y, rx0, rx1 - 1);
            }
        });
    }

    /// Combines the interior of a closed polygon (even-odd fill) into the selection under `op`.
    ///
    /// `points` are image-space vertices; the polygon is implicitly closed. Fewer than three points
    /// is degenerate geometry, handled per [`SelectionOp`]. Rasterization is delegated to
    /// [`crate::tools::fill_polygon_spans`], which owns the even-odd sampling contract shared with
    /// the cleaning tools. `bounds` and `outline_loops` are recomputed from the resulting mask.
    pub fn apply_polygon(&mut self, points: &[(f32, f32)], op: SelectionOp) {
        if points.len() < 3 {
            self.apply_spans(op, true, |_: &mut dyn FnMut(usize, usize, usize)| {});
            return;
        }
        // Copy the dimensions out before the span source borrows the mask.
        let (width, height) = (self.width, self.height);
        self.apply_spans(op, false, |span: &mut dyn FnMut(usize, usize, usize)| fill_polygon_spans(points, width, height, span));
    }

    /// Replaces the selection with an axis-aligned rectangle (inclusive of `min`, exclusive of
    /// `max`). Coordinates are clamped to the page; a degenerate rect clears the selection.
    pub fn set_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.apply_rect(x0, y0, x1, y1, SelectionOp::Replace);
    }

    /// Replaces the selection with the interior of a closed polygon (even-odd fill).
    ///
    /// `points` are image-space vertices; fewer than three points clears the selection.
    pub fn set_polygon(&mut self, points: &[(f32, f32)]) {
        self.apply_polygon(points, SelectionOp::Replace);
    }

    /// Applies `op` to the mask using a span source, then refreshes `bounds` and `outline`.
    ///
    /// `emit` is called at most once and must report the new geometry as spans `(y, sx, ex)` with
    /// an inclusive `sx..=ex`, already clamped to the page, in increasing `y` and increasing `sx`
    /// within a row - the contract of [`crate::tools::fill_polygon_spans`]. `degenerate` marks
    /// geometry that encloses nothing at all; `emit` is then ignored.
    ///
    /// No full-page temporary is ever allocated: `Add`/`Subtract` write spans straight into the
    /// mask, and `Intersect` streams (see below).
    fn apply_spans(&mut self, op: SelectionOp, degenerate: bool, emit: impl FnOnce(&mut dyn FnMut(usize, usize, usize))) {
        if degenerate {
            match op {
                // Nothing intersects an empty region, so Photoshop deselects here as well.
                SelectionOp::Replace | SelectionOp::Intersect => {
                    if let Some(area) = self.bounds {
                        self.clear_within(area);
                    }
                    self.bounds = None;
                    self.outline.clear();
                }
                // Adding or removing nothing leaves mask, bounds and outline valid as they are.
                SelectionOp::Add | SelectionOp::Subtract => {}
            }
            return;
        }
        match op {
            SelectionOp::Replace => {
                // Only the old bounding box can hold set pixels, so clearing it clears everything.
                if let Some(area) = self.bounds {
                    self.clear_within(area);
                }
                let width = self.width;
                let mask = &mut self.mask;
                let mut bounds: Option<SelectionBounds> = None;
                emit(&mut |y, sx, ex| {
                    let row = y * width;
                    mask[row + sx..=row + ex].fill(255);
                    Self::grow_bounds(&mut bounds, sx, ex, y);
                });
                self.bounds = bounds;
            }
            SelectionOp::Add => {
                let width = self.width;
                let mask = &mut self.mask;
                // The union's bounding box is exactly the union of the two bounding boxes, so it can
                // be grown incrementally - no rescan needed.
                let mut bounds = self.bounds;
                emit(&mut |y, sx, ex| {
                    let row = y * width;
                    mask[row + sx..=row + ex].fill(255);
                    Self::grow_bounds(&mut bounds, sx, ex, y);
                });
                self.bounds = bounds;
            }
            SelectionOp::Subtract => {
                let Some(area) = self.bounds else {
                    // Nothing is selected: removing geometry from it changes nothing.
                    return;
                };
                let width = self.width;
                let mask = &mut self.mask;
                emit(&mut |y, sx, ex| {
                    // Clip to the old bounding box: outside it the mask is already zero, so writing
                    // there would only cost time.
                    if !(area.min_y..=area.max_y).contains(&y) {
                        return;
                    }
                    let sx = sx.max(area.min_x);
                    let ex = ex.min(area.max_x);
                    if sx <= ex {
                        zero_span(mask, width, y, sx, ex);
                    }
                });
                // Removing pixels can shrink the box on any side; the survivors all live inside the
                // old box, so rescanning it is both sufficient and O(bbox) - the same order as the
                // outline trace that follows anyway.
                self.recompute_bounds_within(area);
            }
            SelectionOp::Intersect => {
                let Some(area) = self.bounds else {
                    // Nothing is selected: the intersection stays empty and the outline stays empty.
                    return;
                };
                let width = self.width;
                let mask = &mut self.mask;
                // Streaming complement clear. The span contract guarantees increasing `y` and
                // increasing `sx` within a row, so a single (row, column) cursor sweeping the old
                // bounding box can zero every gap *between* the incoming spans as they arrive. That
                // is what removes the need for any temporary buffer: the pixels the new geometry
                // does not cover are exactly the gaps, and everything outside the old box is already
                // zero.
                let mut cur_y = area.min_y;
                let mut cur_x = area.min_x;
                emit(&mut |y, sx, ex| {
                    if !(area.min_y..=area.max_y).contains(&y) {
                        return;
                    }
                    let sx = sx.max(area.min_x);
                    let ex = ex.min(area.max_x);
                    if ex < sx {
                        return;
                    }
                    if y > cur_y {
                        // Close the row in progress, then clear the rows the geometry skipped.
                        if cur_x <= area.max_x {
                            zero_span(mask, width, cur_y, cur_x, area.max_x);
                        }
                        for skipped in (cur_y + 1)..y {
                            zero_span(mask, width, skipped, area.min_x, area.max_x);
                        }
                        cur_y = y;
                        cur_x = area.min_x;
                    }
                    if sx > cur_x {
                        zero_span(mask, width, y, cur_x, sx - 1);
                    }
                    // `max` rather than a plain assignment: two spans of one row may share a pixel
                    // when a crossing pair lands exactly on an integer, and the cursor must never
                    // walk backwards.
                    cur_x = cur_x.max(ex + 1);
                });
                // Tail: the rest of the row in progress and every row after it.
                if cur_x <= area.max_x {
                    zero_span(mask, width, cur_y, cur_x, area.max_x);
                }
                for skipped in (cur_y + 1)..=area.max_y {
                    zero_span(mask, width, skipped, area.min_x, area.max_x);
                }
                self.recompute_bounds_within(area);
            }
        }
        self.rebuild_outline();
    }

    /// Zeroes every mask pixel inside `area`.
    fn clear_within(&mut self, area: SelectionBounds) {
        let width = self.width;
        for y in area.min_y..=area.max_y {
            zero_span(&mut self.mask, width, y, area.min_x, area.max_x);
        }
    }

    /// Recomputes the tight bounding box by scanning `area`, which must contain every set pixel.
    fn recompute_bounds_within(&mut self, area: SelectionBounds) {
        let mut bounds: Option<SelectionBounds> = None;
        for y in area.min_y..=area.max_y {
            let row = y * self.width;
            let slice = &self.mask[row + area.min_x..=row + area.max_x];
            let Some(first) = slice.iter().position(|&v| v != 0) else {
                continue;
            };
            // `rposition` cannot fail once `position` succeeded; `unwrap_or` keeps that provable
            // without a panic path.
            let last = slice.iter().rposition(|&v| v != 0).unwrap_or(first);
            Self::grow_bounds(&mut bounds, area.min_x + first, area.min_x + last, y);
        }
        self.bounds = bounds;
    }

    /// Re-traces `outline` from the mask along pixel edges.
    ///
    /// Costs O(bounding box) and never looks at the rest of the page. Emits the four directed
    /// boundary edges of every selected pixel whose neighbour on that side is unselected or off the
    /// page, with a clockwise winding, then links each edge to the outgoing edge of its end vertex
    /// (see [`next_boundary_dir`]).
    fn rebuild_outline(&mut self) {
        self.outline.clear();
        let Some(b) = self.bounds else {
            return;
        };
        // Vertex grid of the bounding box: pixel corners, one more than pixels on each axis.
        let gw = b.max_x - b.min_x + 2;
        let gh = b.max_y - b.min_y + 2;
        // One byte per vertex: low nibble = outgoing boundary edges present, high nibble = edges
        // already walked. Packing both into one grid halves the temporary, which matters for a
        // full-page ribbon selection; a HashMap would be far more expensive for the same job.
        let mut grid = vec![0u8; gw * gh];
        for py in b.min_y..=b.max_y {
            let row = py * self.width;
            let v_row = (py - b.min_y) * gw;
            for px in b.min_x..=b.max_x {
                if self.mask[row + px] == 0 {
                    continue;
                }
                let v = v_row + (px - b.min_x);
                // Clockwise winding (screen `y` grows downward): top edge left->right, right edge
                // downward, bottom edge right->left, left edge upward. Off-page neighbours count as
                // unselected, which is why the page edges close the loop.
                if py == 0 || self.mask[row - self.width + px] == 0 {
                    grid[v] |= 1u8 << DIR_RIGHT;
                }
                if px + 1 >= self.width || self.mask[row + px + 1] == 0 {
                    grid[v + 1] |= 1u8 << DIR_DOWN;
                }
                if py + 1 >= self.height || self.mask[row + self.width + px] == 0 {
                    grid[v + gw + 1] |= 1u8 << DIR_LEFT;
                }
                if px == 0 || self.mask[row + px - 1] == 0 {
                    grid[v + gw] |= 1u8 << DIR_UP;
                }
            }
        }
        for start in 0..grid.len() {
            for d0 in [DIR_RIGHT, DIR_DOWN, DIR_LEFT, DIR_UP] {
                let present = (grid[start] & (1u8 << d0)) != 0;
                let walked = (grid[start] & (1u8 << (d0 + 4))) != 0;
                if !present || walked {
                    continue;
                }
                if let Some(loop_pts) = Self::trace_loop(&mut grid, gw, b, start, d0) {
                    self.outline.push(loop_pts);
                }
            }
        }
    }

    /// Walks one closed boundary loop starting at the outgoing edge `(start, d0)`.
    ///
    /// Marks every walked edge in the high nibble of `grid` and returns the loop's corner vertices
    /// in image-pixel coordinates, closed by repeating the first point. Returns `None` for a loop
    /// too short to draw. Termination is unconditional: every step consumes a not-yet-walked edge.
    fn trace_loop(grid: &mut [u8], gw: usize, b: SelectionBounds, start: usize, d0: u8) -> Option<Vec<(f32, f32)>> {
        // Grid index -> pixel corner in image space. The casts are exact for any real page size
        // (`f32` represents integers exactly up to 2^24).
        let vertex_xy = |idx: usize| -> (f32, f32) { ((b.min_x + idx % gw) as f32, (b.min_y + idx / gw) as f32) };
        let mut pts: Vec<(f32, f32)> = Vec::new();
        let mut idx = start;
        let mut dir = d0;
        let mut prev_dir: Option<u8> = None;
        loop {
            grid[idx] |= 1u8 << (dir + 4);
            // A vertex is emitted only where the walk turns, so consecutive collinear edges collapse
            // into one segment and a 10x10 block stays five points instead of forty-one.
            if prev_dir != Some(dir) {
                pts.push(vertex_xy(idx));
            }
            prev_dir = Some(dir);
            idx = step_vertex(idx, dir, gw);
            let Some(next) = next_boundary_dir(grid[idx] & 0x0F, dir) else {
                break;
            };
            // An already-walked successor closes the cycle. With a consistent winding that is
            // exactly the edge the loop started on; checking the mark instead of the start edge
            // makes termination independent of that argument.
            if (grid[idx] & (1u8 << (next + 4))) != 0 {
                break;
            }
            dir = next;
        }
        // The walk may have started in the middle of a straight run; the starting vertex is then not
        // a corner, and the closing edge runs in the same direction as the first one.
        if pts.len() > 1 && prev_dir == Some(d0) {
            pts.remove(0);
        }
        let &first = pts.first()?;
        pts.push(first);
        Some(pts)
    }

    fn grow_bounds(bounds: &mut Option<SelectionBounds>, sx: usize, ex: usize, y: usize) {
        match bounds {
            Some(b) => {
                b.min_x = b.min_x.min(sx);
                b.max_x = b.max_x.max(ex);
                b.min_y = b.min_y.min(y);
                b.max_y = b.max_y.max(y);
            }
            None => {
                *bounds = Some(SelectionBounds {
                    min_x: sx,
                    min_y: y,
                    max_x: ex,
                    max_y: y,
                });
            }
        }
    }
}

/// Zeroes `mask` on row `y` from `x0` to `x1` inclusive. The range must be inside the buffer.
fn zero_span(mask: &mut [u8], width: usize, y: usize, x0: usize, x1: usize) {
    let row = y * width;
    mask[row + x0..=row + x1].fill(0);
}

/// Moves one boundary edge along `dir` on a vertex grid `gw` vertices wide.
///
/// In bounds by construction: an edge is emitted only when its far vertex exists in the
/// `(bbox_w + 1) x (bbox_h + 1)` grid.
fn step_vertex(idx: usize, dir: u8, gw: usize) -> usize {
    // `& 3` makes the match total without a silent fallback: the last arm *is* `DIR_UP`.
    match dir & 3 {
        DIR_RIGHT => idx + 1,
        DIR_DOWN => idx + gw,
        DIR_LEFT => idx - 1,
        _ => idx - gw,
    }
}

/// Picks the outgoing boundary edge to continue with after arriving along `incoming`.
///
/// `available` is the low nibble of a vertex cell (bit `d` set = an outgoing edge in direction `d`).
/// The preference order is: clockwise turn, straight on, counter-clockwise turn, reverse.
///
/// Only a diagonal pinch - two selected pixels meeting at a corner with the other two pixels of that
/// 2x2 block unselected - gives a vertex two outgoing edges. There the clockwise-first rule pairs
/// each of the two incoming edges with a *different* outgoing edge, so the pairing is a bijection on
/// directed edges: the boundary decomposes into disjoint closed loops, the walk always terminates,
/// and the two diagonal pixels are drawn as two separate loops rather than one self-touching loop.
///
/// Returns `None` only for a vertex with no outgoing edge, which the consistent winding makes
/// unreachable (in-degree equals out-degree at every vertex); the caller then closes the loop
/// instead of panicking.
fn next_boundary_dir(available: u8, incoming: u8) -> Option<u8> {
    for turn in [1u8, 0, 3, 2] {
        let dir = (incoming + turn) & 3;
        if (available & (1u8 << dir)) != 0 {
            return Some(dir);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The "U" lasso reused by several tests: two arms with a notch between them, closed by a
    /// bottom bar, so the shape is one connected component whose notch opens at the top.
    const U_LASSO: [(f32, f32); 8] = [(1.0, 1.0), (3.0, 1.0), (3.0, 6.0), (6.0, 6.0), (6.0, 1.0), (8.0, 1.0), (8.0, 8.0), (1.0, 8.0)];

    /// Asserts the marquee contract every traced loop must satisfy.
    fn assert_loops_are_closed(sel: &Selection) {
        for lp in sel.outline_loops() {
            assert!(lp.len() >= 5, "a traced loop is at least a closed square, got {lp:?}");
            assert_eq!(lp.first(), lp.last(), "a marquee loop must repeat its first point: {lp:?}");
        }
    }

    #[test]
    fn rect_selection_marks_interior_and_bounds() {
        let mut sel = Selection::empty(10, 10);
        sel.set_rect(2, 3, 6, 7);
        assert!(sel.any());
        assert!(sel.contains(2, 3));
        assert!(sel.contains(5, 6));
        assert!(!sel.contains(6, 7), "max edge is exclusive");
        assert!(!sel.contains(0, 0));
        // Interior corners are set; the exclusive max edge is not.
        assert!(sel.contains(2, 6));
        assert!(sel.contains(5, 3));
    }

    #[test]
    fn degenerate_rect_clears_selection() {
        let mut sel = Selection::empty(10, 10);
        sel.set_rect(4, 4, 4, 9);
        assert!(!sel.any());
        assert!(sel.outline_loops().is_empty());
    }

    #[test]
    fn polygon_triangle_fills_interior() {
        let mut sel = Selection::empty(10, 10);
        // Triangle covering the lower-left area.
        sel.set_polygon(&[(1.0, 1.0), (8.0, 1.0), (1.0, 8.0)]);
        assert!(sel.any());
        assert!(sel.contains(2, 2));
        assert!(!sel.contains(7, 7));
    }

    #[test]
    fn concave_polygon_keeps_the_notch_unselected() {
        // A "U" lasso: the notch between the arms must stay outside the selection, and the
        // bounding box must still span the whole shape.
        let mut sel = Selection::empty(9, 9);
        sel.set_polygon(&U_LASSO);
        assert!(sel.any());
        assert!(sel.contains(2, 3), "left arm");
        assert!(sel.contains(7, 3), "right arm");
        assert!(!sel.contains(4, 3), "notch stays unselected");
        assert!(sel.contains(4, 7), "bottom bar spans the notch columns");
        let b = sel.bounds().expect("non-empty selection has bounds");
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (1, 1, 8, 7));
        // The marquee is traced from the MASK, not from the lasso path: one connected component
        // (the notch opens at the top, so it is not a hole), staircased around the filled pixels.
        assert_eq!(sel.outline_loops().len(), 1);
        assert_loops_are_closed(&sel);
        let outline = &sel.outline_loops()[0];
        assert!(outline.len() > 5, "the notch makes the loop more than a rectangle: {outline:?}");
        assert!(outline.iter().all(|&(x, y)| x.fract() == 0.0 && y.fract() == 0.0), "the outline runs on pixel edges: {outline:?}");
    }

    #[test]
    fn rect_outline_is_the_pixel_edge_rectangle() {
        // Regression guard: a rectangle must trace to exactly the loop the old outline emitted.
        let mut sel = Selection::empty(10, 10);
        sel.set_rect(2, 3, 6, 7);
        assert_eq!(sel.outline_loops().len(), 1);
        assert_eq!(sel.outline_loops()[0], vec![(2.0, 3.0), (6.0, 3.0), (6.0, 7.0), (2.0, 7.0), (2.0, 3.0)]);
    }

    #[test]
    fn single_pixel_outline_has_five_points() {
        let mut sel = Selection::empty(5, 5);
        sel.set_rect(2, 2, 3, 3);
        assert_eq!(sel.outline_loops().len(), 1);
        assert_eq!(sel.outline_loops()[0], vec![(2.0, 2.0), (3.0, 2.0), (3.0, 3.0), (2.0, 3.0), (2.0, 2.0)]);
    }

    #[test]
    fn collinear_boundary_edges_are_merged() {
        // A 10x10 block has 40 boundary edges; merging collinear runs must leave five points.
        let mut sel = Selection::empty(12, 12);
        sel.set_rect(1, 1, 11, 11);
        assert_eq!(sel.outline_loops().len(), 1);
        assert_eq!(sel.outline_loops()[0].len(), 5);
        // The same holds when the selection touches every page edge (off-page neighbours count as
        // unselected, so the loop still closes along the page border).
        let mut full = Selection::empty(12, 12);
        full.set_rect(0, 0, 12, 12);
        assert_eq!(full.outline_loops()[0], vec![(0.0, 0.0), (12.0, 0.0), (12.0, 12.0), (0.0, 12.0), (0.0, 0.0)]);
    }

    #[test]
    fn subtract_punches_a_hole_and_traces_two_loops() {
        let mut sel = Selection::empty(12, 12);
        sel.set_rect(1, 1, 11, 11);
        sel.apply_rect(4, 4, 7, 7, SelectionOp::Subtract);
        assert!(sel.contains(1, 1), "the ring survives");
        assert!(!sel.contains(5, 5), "the hole is punched out");
        let b = sel.bounds().expect("the ring is not empty");
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (1, 1, 10, 10), "the outer box is unchanged");
        assert_eq!(sel.outline_loops().len(), 2, "outer boundary plus the hole");
        assert_loops_are_closed(&sel);
        assert_eq!(sel.outline_loops()[0], vec![(1.0, 1.0), (11.0, 1.0), (11.0, 11.0), (1.0, 11.0), (1.0, 1.0)]);
        // The hole is wound the other way round (interior of the selection stays on the same side).
        assert_eq!(sel.outline_loops()[1], vec![(4.0, 4.0), (4.0, 7.0), (7.0, 7.0), (7.0, 4.0), (4.0, 4.0)]);
    }

    #[test]
    fn two_disjoint_blocks_trace_two_loops() {
        let mut sel = Selection::empty(10, 10);
        sel.set_rect(0, 0, 2, 2);
        sel.apply_rect(5, 5, 8, 8, SelectionOp::Add);
        let b = sel.bounds().expect("two blocks are not empty");
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (0, 0, 7, 7));
        assert_eq!(sel.outline_loops().len(), 2);
        assert_loops_are_closed(&sel);
        assert_eq!(sel.outline_loops()[0], vec![(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0), (0.0, 0.0)]);
        assert_eq!(sel.outline_loops()[1], vec![(5.0, 5.0), (8.0, 5.0), (8.0, 8.0), (5.0, 8.0), (5.0, 5.0)]);
    }

    #[test]
    fn diagonally_touching_pixels_trace_two_closed_loops() {
        // The pinch case: the two pixels share exactly one vertex, where two boundary edges could
        // continue. The clockwise-first turn rule separates them into two loops and terminates.
        let mut sel = Selection::empty(4, 4);
        sel.set_rect(0, 0, 1, 1);
        sel.apply_rect(1, 1, 2, 2, SelectionOp::Add);
        assert!(sel.contains(0, 0) && sel.contains(1, 1));
        assert!(!sel.contains(1, 0) && !sel.contains(0, 1));
        assert_eq!(sel.outline_loops().len(), 2);
        assert_loops_are_closed(&sel);
        assert_eq!(sel.outline_loops()[0], vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]);
        assert_eq!(sel.outline_loops()[1], vec![(1.0, 1.0), (2.0, 1.0), (2.0, 2.0), (1.0, 2.0), (1.0, 1.0)]);
    }

    /// Selection covering pixels `1..=4` on both axes of a 10x10 page.
    fn rect_a() -> Selection {
        let mut sel = Selection::empty(10, 10);
        sel.apply_rect(1, 1, 5, 5, SelectionOp::Replace);
        sel
    }

    #[test]
    fn replace_drops_the_previous_geometry() {
        let mut sel = rect_a();
        sel.apply_rect(3, 3, 7, 7, SelectionOp::Replace);
        assert!(!sel.contains(1, 1), "the previous rectangle is gone");
        assert!(sel.contains(3, 3) && sel.contains(6, 6));
        let b = sel.bounds().expect("non-empty");
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (3, 3, 6, 6));
    }

    #[test]
    fn add_unions_the_two_rectangles() {
        let mut sel = rect_a();
        sel.apply_rect(3, 3, 7, 7, SelectionOp::Add);
        assert!(sel.contains(1, 1), "only in A");
        assert!(sel.contains(6, 6), "only in B");
        assert!(sel.contains(3, 3), "in both");
        assert!(!sel.contains(1, 6), "in neither");
        let b = sel.bounds().expect("non-empty");
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (1, 1, 6, 6));
    }

    #[test]
    fn subtract_removes_the_overlap_and_shrinks_nothing_else() {
        let mut sel = rect_a();
        sel.apply_rect(3, 3, 7, 7, SelectionOp::Subtract);
        assert!(sel.contains(2, 2), "outside B");
        assert!(sel.contains(4, 2) && sel.contains(2, 4), "the L-shaped remainder");
        assert!(!sel.contains(3, 3) && !sel.contains(4, 4), "the overlap is gone");
        assert!(!sel.contains(6, 6), "B never belonged to the selection");
        let b = sel.bounds().expect("non-empty");
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (1, 1, 4, 4), "the L still reaches the old box");
    }

    #[test]
    fn intersect_keeps_only_the_overlap() {
        let mut sel = rect_a();
        sel.apply_rect(3, 3, 7, 7, SelectionOp::Intersect);
        assert!(sel.contains(3, 3) && sel.contains(4, 4), "the overlap survives");
        assert!(!sel.contains(2, 2), "only in A");
        assert!(!sel.contains(5, 5), "only in B");
        let b = sel.bounds().expect("the overlap is not empty");
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (3, 3, 4, 4));
        assert_eq!(sel.outline_loops()[0], vec![(3.0, 3.0), (5.0, 3.0), (5.0, 5.0), (3.0, 5.0), (3.0, 3.0)]);
    }

    #[test]
    fn subtracting_everything_empties_the_selection() {
        let mut sel = rect_a();
        sel.apply_rect(0, 0, 10, 10, SelectionOp::Subtract);
        assert!(!sel.any(), "an all-zero mask must report no selection");
        assert!(sel.bounds().is_none());
        assert!(sel.outline_loops().is_empty());
    }

    #[test]
    fn intersect_with_a_disjoint_rect_empties_the_selection() {
        let mut sel = rect_a();
        sel.apply_rect(6, 6, 9, 9, SelectionOp::Intersect);
        assert!(!sel.any());
        assert!(sel.outline_loops().is_empty());
    }

    #[test]
    fn degenerate_geometry_is_handled_per_op() {
        // Add / Subtract with nothing to add or remove leave the selection untouched.
        for op in [SelectionOp::Add, SelectionOp::Subtract] {
            let mut sel = rect_a();
            sel.apply_rect(4, 4, 4, 9, op);
            assert!(sel.contains(1, 1) && sel.contains(4, 4), "{op:?} with an empty rect must not change the mask");
            let b = sel.bounds().expect("still non-empty");
            assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (1, 1, 4, 4));
            assert_eq!(sel.outline_loops().len(), 1, "{op:?} must leave the marquee intact");
            let mut poly = rect_a();
            poly.apply_polygon(&[(1.0, 1.0), (2.0, 2.0)], op);
            assert!(poly.contains(1, 1), "{op:?} with fewer than three points must not change the mask");
        }
        // Replace / Intersect with nothing deselect (Photoshop intersects to an empty selection).
        for op in [SelectionOp::Replace, SelectionOp::Intersect] {
            let mut sel = rect_a();
            sel.apply_rect(4, 4, 4, 9, op);
            assert!(!sel.any(), "{op:?} with an empty rect must clear the selection");
            assert!(sel.outline_loops().is_empty());
            let mut poly = rect_a();
            poly.apply_polygon(&[(1.0, 1.0)], op);
            assert!(!poly.any(), "{op:?} with fewer than three points must clear the selection");
        }
    }

    #[test]
    fn intersecting_the_full_page_with_a_lasso_equals_a_plain_fill() {
        // Exercises the streaming intersect against a concave polygon: rows with two spans, rows
        // with none, and gaps on both ends of a row.
        let mut streamed = Selection::empty(9, 9);
        streamed.set_rect(0, 0, 9, 9);
        streamed.apply_polygon(&U_LASSO, SelectionOp::Intersect);
        let mut direct = Selection::empty(9, 9);
        direct.set_polygon(&U_LASSO);
        for y in 0..9 {
            for x in 0..9 {
                assert_eq!(streamed.contains(x, y), direct.contains(x, y), "pixel ({x}, {y}) differs");
            }
        }
        let sb = streamed.bounds().expect("non-empty");
        let db = direct.bounds().expect("non-empty");
        assert_eq!((sb.min_x, sb.min_y, sb.max_x, sb.max_y), (db.min_x, db.min_y, db.max_x, db.max_y));
        assert_eq!(streamed.outline_loops(), direct.outline_loops());
    }
}
