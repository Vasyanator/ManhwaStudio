/*
File: region_edit_v2/input.rs

Purpose:
Hit-target geometry and drag maths of the on-canvas region frame: where the eight resize
handles sit, what a drag of one of them does to the frame rectangle, and what a drag of the
top strip does to its position. Everything here works on PLAIN DATA — screen rects for the
handle geometry, source page pixels for the rectangle maths — so the whole resize/move
contract is unit-testable without an `egui::Context`.

Key structures:
- `HandleKind`: the eight resize handles, in the clockwise order the repo already paints
  bubble handles in (`canvas/bubble_on_top_ui.rs`)
- `DragKind`, `DragState`: which gesture is in flight and the anchor it is measured from

Key functions:
- `handle_points()`, `handle_hit_rects()`: handle centres and their hit rectangles
- `moved_rect_px()`: the top-strip drag
- `resized_rect_px()`: a handle drag, snapped through `geometry::nearest_valid_size`

Notes:
Every handle lives entirely OUTSIDE the frame — see `handle_hit_rects` for why and for the
L-shaped corner case that makes a handle need two rectangles.
A drag is ALWAYS measured from an anchor captured on `drag_started` (`DragState::start` plus
`DragState::origin`), never accumulated from per-frame deltas. That is what lets the
per-frame keep-in-view clamp of `frame.rs` fight the drag without the two drifting apart: the
clamp rewrites the live rectangle, the next frame re-derives it from the untouched anchor,
and the frame simply stays pinned at the border while the pointer is beyond it.
Design: `dev-docs/region_edit_v2_plan.md` (§1, §2 D3/D4, §10).
*/

use super::geometry::{FrameConstraints, nearest_valid_size};
use crate::canvas::OverlayRectPx;
use egui::{Pos2, Rect, pos2};

/// Radius of a handle, in screen points — both painted and hit-tested.
///
/// Twice the 4 pt the bubble rect handles use (`canvas/bubble_on_top_ui.rs`): a handle here
/// occupies only the OUTER side of the frame border, so at the bubble radius it would be
/// half the target it looks like. The same number is the frame's handle margin
/// (`geometry::FrameChrome::handle_margin`), so the hitbox grows exactly as far as the
/// handles reach.
pub(super) const HANDLE_RADIUS: f32 = 8.0;

/// Angular sweep of a side-midpoint handle: a half disc, flat side along the frame edge.
const SIDE_SWEEP: f32 = std::f32::consts::PI;

/// Angular sweep of a corner handle: three quadrants, the fourth being the one that would
/// fall inside the frame.
const CORNER_SWEEP: f32 = 1.5 * std::f32::consts::PI;

/// The eight resize handles: four corners and four side midpoints.
///
/// The order of `ALL` is the order of `handle_points`, and both are the clockwise walk
/// starting at the top-left corner that `rect_handle_points` in
/// `canvas/bubble_on_top_ui.rs` already uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HandleKind {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

impl HandleKind {
    /// Every handle, in the order `handle_points` returns their centres.
    pub(super) const ALL: [Self; 8] = [
        Self::TopLeft,
        Self::Top,
        Self::TopRight,
        Self::Right,
        Self::BottomRight,
        Self::Bottom,
        Self::BottomLeft,
        Self::Left,
    ];

    /// Whether dragging this handle moves the frame's LEFT edge.
    #[must_use]
    pub(super) fn moves_left(self) -> bool {
        matches!(self, Self::TopLeft | Self::Left | Self::BottomLeft)
    }

    /// Whether dragging this handle moves the frame's RIGHT edge.
    #[must_use]
    pub(super) fn moves_right(self) -> bool {
        matches!(self, Self::TopRight | Self::Right | Self::BottomRight)
    }

    /// Whether dragging this handle moves the frame's TOP edge.
    #[must_use]
    pub(super) fn moves_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::Top | Self::TopRight)
    }

    /// Whether dragging this handle moves the frame's BOTTOM edge.
    #[must_use]
    pub(super) fn moves_bottom(self) -> bool {
        matches!(self, Self::BottomLeft | Self::Bottom | Self::BottomRight)
    }

    /// Which way this handle points AWAY from the frame, as a unit step per axis.
    ///
    /// `-1` is left / up, `+1` is right / down, `0` is "this axis is a side midpoint". A
    /// corner has both components non-zero; a side midpoint has exactly one.
    #[must_use]
    fn outward(self) -> (f32, f32) {
        let x = if self.moves_left() {
            -1.0
        } else if self.moves_right() {
            1.0
        } else {
            0.0
        };
        let y = if self.moves_top() {
            -1.0
        } else if self.moves_bottom() {
            1.0
        } else {
            0.0
        };
        (x, y)
    }

    /// Whether this handle sits on a corner rather than on a side midpoint.
    #[must_use]
    pub(super) fn is_corner(self) -> bool {
        let (x, y) = self.outward();
        x != 0.0 && y != 0.0
    }

    /// A stable, non-localized suffix identifying this handle inside an `egui::Id`.
    ///
    /// A widget id must not change with the interface language (`egui-docs/05-ids-and-i18n.md`),
    /// so the id source is this literal and never the handle's caption.
    #[must_use]
    pub(super) fn id_suffix(self) -> &'static str {
        match self {
            Self::TopLeft => "tl",
            Self::Top => "t",
            Self::TopRight => "tr",
            Self::Right => "r",
            Self::BottomRight => "br",
            Self::Bottom => "b",
            Self::BottomLeft => "bl",
            Self::Left => "l",
        }
    }
}

/// Centres of the eight handles of `rect`, in the order of `HandleKind::ALL`.
#[must_use]
pub(super) fn handle_points(rect: Rect) -> [Pos2; 8] {
    let center = rect.center();
    [
        pos2(rect.left(), rect.top()),
        pos2(center.x, rect.top()),
        pos2(rect.right(), rect.top()),
        pos2(rect.right(), center.y),
        pos2(rect.right(), rect.bottom()),
        pos2(center.x, rect.bottom()),
        pos2(rect.left(), rect.bottom()),
        pos2(rect.left(), center.y),
    ]
}

/// Hit rectangles of the handle at `point`, together covering exactly what it PAINTS.
///
/// A handle never reaches into the frame: the interior belongs to mask painting, and a hit
/// area that overlapped it would steal strokes meant for the mask. So a side midpoint is the
/// outer half of its `2r x 2r` box — one rectangle — and a corner is the outer THREE
/// quadrants, which is L-shaped and cannot be one rectangle. The corner's two rectangles
/// meet along a full edge and do not overlap, so their union is exactly the painted
/// three-quarter disc's bounding footprint and never enters the interior.
///
/// The second rectangle is `None` for a side midpoint. Both rectangles of a corner drive the
/// SAME handle: the caller senses each with its own stable `egui::Id` and takes the drag from
/// whichever `Response` reports one.
///
/// Handles touch the frame border but never cross it, so every returned rectangle meets the
/// frame in a zero-area sliver at most.
#[must_use]
pub(super) fn handle_hit_rects(point: Pos2, handle: HandleKind) -> (Rect, Option<Rect>) {
    let r = HANDLE_RADIUS;
    let (hx, hy) = handle.outward();
    if hy == 0.0 {
        // Left / right midpoint: the outer half-box, full height, half width.
        return (Rect::from_min_max(pos2(point.x.min(point.x + hx * r), point.y - r), pos2(point.x.max(point.x + hx * r), point.y + r)), None);
    }
    // Top / bottom midpoint and both bands of a corner share this one: full width, the half
    // height that lies on the outward side of the frame edge.
    let band = Rect::from_min_max(pos2(point.x - r, point.y.min(point.y + hy * r)), pos2(point.x + r, point.y.max(point.y + hy * r)));
    if hx == 0.0 {
        return (band, None);
    }
    // The corner's second quadrant: beside the frame's vertical edge, level with its top (or
    // bottom) — the quadrant the band above does not cover and the interior does not own.
    let stub = Rect::from_min_max(
        pos2(point.x.min(point.x + hx * r), point.y.min(point.y - hy * r)),
        pos2(point.x.max(point.x + hx * r), point.y.max(point.y - hy * r)),
    );
    (band, Some(stub))
}

/// Start angle and sweep, in radians, of the partial disc a handle is painted as.
///
/// The disc is centred on the handle point and spans the sweep centred on the handle's
/// OUTWARD direction, so the omitted part is exactly what would fall inside the frame: half
/// a disc for a side midpoint, three quarters for a corner. Angles are measured with
/// `x = cos`, `y = sin` in egui's y-down screen space.
#[must_use]
pub(super) fn handle_arc(handle: HandleKind) -> (f32, f32) {
    let (hx, hy) = handle.outward();
    let sweep = if handle.is_corner() { CORNER_SWEEP } else { SIDE_SWEEP };
    (hy.atan2(hx) - sweep * 0.5, sweep)
}

/// Which gesture a live drag is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DragKind {
    /// The top strip is being dragged: the frame moves, its size does not change.
    Move,
    /// One resize handle is being dragged.
    Resize(HandleKind),
}

/// A drag in flight, together with the anchor every frame of it is measured from.
///
/// `start` is the frame rectangle as it was when the drag began and `origin` the pointer
/// position at that moment; the live rectangle is always `f(start, pointer - origin)` and is
/// never accumulated, so a keep-in-view correction applied in between cannot make the frame
/// drift away from the pointer.
#[derive(Debug, Clone, Copy)]
pub(super) struct DragState {
    pub(super) kind: DragKind,
    pub(super) origin: Pos2,
    pub(super) start: OverlayRectPx,
}

/// Converts a screen-point offset to source page pixels at `zoom`.
///
/// A non-finite or non-positive `zoom` yields no movement rather than a wild jump: the
/// canvas has not established a transform yet and every derived position would be garbage.
#[must_use]
pub(super) fn screen_delta_to_px(delta: egui::Vec2, zoom: f32) -> (i64, i64) {
    if !zoom.is_finite() || zoom <= 0.0 {
        return (0, 0);
    }
    (round_to_i64(delta.x / zoom), round_to_i64(delta.y / zoom))
}

/// Converts a keep-in-view CORRECTION from screen points to source page pixels at `zoom`,
/// rounding TOWARD ZERO.
///
/// Rounding a correction to the nearest pixel overshoots, and an overshoot at exactly half a
/// pixel oscillates forever: at zoom 1.25 a `+0.625` pt request becomes `+0.5` px, rounds to
/// `+1`, and the frame's other edge is then `0.625` pt outside, so the next frame asks for
/// `-0.625` pt and the origin alternates between two values for as long as the frame is
/// drawn. Truncating instead UNDERSHOOTS: a sub-pixel residual overhang is left uncorrected,
/// and tolerating it is exactly what makes the clamp reach a fixed point.
///
/// A non-finite or non-positive `zoom` yields no movement, as in `screen_delta_to_px`.
#[must_use]
pub(super) fn correction_delta_to_px(delta: egui::Vec2, zoom: f32) -> (i64, i64) {
    if !zoom.is_finite() || zoom <= 0.0 {
        return (0, 0);
    }
    (trunc_to_i64(delta.x / zoom), trunc_to_i64(delta.y / zoom))
}

/// Rounds a screen-derived offset to whole page pixels, saturating instead of wrapping.
///
/// A float-to-integer cast saturates in Rust, and the value is a pointer offset divided by a
/// zoom factor, so it is bounded by the viewport in practice; the guard is for the
/// non-finite case a degenerate transform could produce.
#[must_use]
fn round_to_i64(v: f32) -> i64 {
    if !v.is_finite() {
        return 0;
    }
    v.round() as i64
}

/// Truncates a screen-derived offset toward zero, saturating instead of wrapping. Same
/// saturation argument as `round_to_i64`.
#[must_use]
fn trunc_to_i64(v: f32) -> i64 {
    if !v.is_finite() {
        return 0;
    }
    v.trunc() as i64
}

/// The frame rectangle after a top-strip drag of `delta` page pixels.
///
/// The rectangle keeps its size and is clamped so that it stays fully inside a page of
/// `page_w` x `page_h` source pixels (D2: the frame may never leave the page it edits). A
/// frame wider or taller than the page is pinned to the page origin on that axis.
#[must_use]
pub(super) fn moved_rect_px(start: OverlayRectPx, delta: (i64, i64), page_w: usize, page_h: usize) -> OverlayRectPx {
    OverlayRectPx {
        x: clamp_origin(to_i64(start.x) + delta.0, start.w, page_w),
        y: clamp_origin(to_i64(start.y) + delta.1, start.h, page_h),
        w: start.w,
        h: start.h,
    }
}

/// Clamps an origin so that `origin + size` stays inside `page`, flooring at zero.
#[must_use]
fn clamp_origin(origin: i64, size: usize, page: usize) -> usize {
    // A frame larger than the page cannot satisfy both ends; pin it to the origin so the
    // result is deterministic rather than dependent on the drag direction.
    let max = to_i64(page).saturating_sub(to_i64(size)).max(0);
    to_usize(origin.clamp(0, max))
}

/// The frame rectangle after dragging `handle` by `delta` page pixels.
///
/// The edges the handle owns move, the opposite edges stay put, the resulting size is snapped
/// through `nearest_valid_size` and the rectangle is then re-anchored on the edges that did
/// NOT move, so a resize never drags the frame sideways. The result is always inside the
/// page.
///
/// Snapping rounds to the NEAREST legal size and can therefore ask for more than the page
/// holds; the size is then reduced to the largest grid multiple that fits, and never to more
/// than the page itself. When the result is below `FrameConstraints::min_side` or off the
/// grid, the size is left illegal on purpose — the frame paints red and says so (D6), which
/// is more useful than silently editing a region the consumer would refuse, and far more
/// useful than a legal size that leaves the page.
#[must_use]
pub(super) fn resized_rect_px(
    start: OverlayRectPx,
    handle: HandleKind,
    delta: (i64, i64),
    page_w: usize,
    page_h: usize,
    constraints: &FrameConstraints,
) -> OverlayRectPx {
    let (x0, x1) = drag_edges(
        (to_i64(start.x), to_i64(start.x) + to_i64(start.w)),
        (handle.moves_left(), handle.moves_right()),
        delta.0,
        to_i64(page_w),
    );
    let (y0, y1) = drag_edges(
        (to_i64(start.y), to_i64(start.y) + to_i64(start.h)),
        (handle.moves_top(), handle.moves_bottom()),
        delta.1,
        to_i64(page_h),
    );

    let (w, h) = nearest_valid_size(to_usize(x1 - x0), to_usize(y1 - y0), constraints);
    let w = fit_side_into_page(w, page_w, constraints.multiple);
    let h = fit_side_into_page(h, page_h, constraints.multiple);

    // Re-anchor on the edge the handle did NOT move, so the fixed corner stays fixed.
    let x = anchor_origin(x0, x1, w, handle.moves_left(), page_w);
    let y = anchor_origin(y0, y1, h, handle.moves_top(), page_h);
    OverlayRectPx { x, y, w, h }
}

/// One axis of a handle drag: moves whichever edges the handle owns, keeps the interval
/// non-empty and keeps it inside `[0, page]`.
#[must_use]
fn drag_edges(edges: (i64, i64), moves: (bool, bool), delta: i64, page: i64) -> (i64, i64) {
    let (mut lo, mut hi) = edges;
    if moves.0 {
        // The moving edge may not cross or touch the opposite one: a zero-width interval
        // would make the snap below meaningless.
        lo = lo.saturating_add(delta).clamp(0, (hi - 1).max(0));
    }
    if moves.1 {
        hi = hi.saturating_add(delta).clamp(lo + 1, page.max(lo + 1));
    }
    (lo, hi)
}

/// The largest grid multiple of `step` that is at most `page`, or `side` when it already fits.
///
/// The result NEVER exceeds `page`: a frame may not leave the page it edits (D2), and that
/// invariant outranks the grid. A page narrower than one grid unit therefore yields the page
/// itself — an illegal size that `check_size` reports and the frame paints red, exactly as
/// `nearest_valid_size` already resolves a `min_side` that contradicts `max_area`.
#[must_use]
fn fit_side_into_page(side: usize, page: usize, step: usize) -> usize {
    if side <= page {
        return side;
    }
    let step = step.max(1);
    let fitted = (page / step).saturating_mul(step);
    // Zero units fit only when the page is narrower than the grid; the page itself is then
    // the largest side that stays on the page. Floored at one so no caller sees a zero-sized
    // frame on a degenerate (zero-pixel) page.
    if fitted == 0 { page.max(1) } else { fitted }
}

/// Origin of one axis after a resize: anchored on the far edge when the near edge moved.
#[must_use]
fn anchor_origin(lo: i64, hi: i64, size: usize, near_edge_moved: bool, page: usize) -> usize {
    let origin = if near_edge_moved { hi - to_i64(size) } else { lo };
    clamp_origin(origin, size, page)
}

/// Widens a page-pixel count for the signed edge maths, saturating instead of wrapping.
#[inline]
#[must_use]
fn to_i64(v: usize) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Narrows a non-negative edge coordinate back to a page-pixel count.
#[inline]
#[must_use]
fn to_usize(v: i64) -> usize {
    usize::try_from(v.max(0)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::vec2;

    fn free_constraints() -> FrameConstraints {
        FrameConstraints { multiple: 1, min_side: 1, max_area: None, max_aspect: None }
    }

    fn grid_constraints(multiple: usize, min_side: usize) -> FrameConstraints {
        FrameConstraints { multiple, min_side, max_area: None, max_aspect: None }
    }

    fn rect(x: usize, y: usize, w: usize, h: usize) -> OverlayRectPx {
        OverlayRectPx { x, y, w, h }
    }

    #[test]
    fn handle_points_follow_the_kind_order() {
        let r = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 50.0));
        let points = handle_points(r);
        assert_eq!(points[0], pos2(0.0, 0.0));
        assert_eq!(points[2], pos2(100.0, 0.0));
        assert_eq!(points[4], pos2(100.0, 50.0));
        assert_eq!(points[6], pos2(0.0, 50.0));
        assert_eq!(HandleKind::ALL[0], HandleKind::TopLeft);
        assert_eq!(HandleKind::ALL[4], HandleKind::BottomRight);
    }

    /// A frame far larger than a handle, so every quadrant test below is unambiguous.
    fn handle_frame() -> Rect {
        Rect::from_min_max(pos2(100.0, 200.0), pos2(400.0, 500.0))
    }

    /// Every hit rectangle of every handle, paired with the handle it drives.
    fn all_hit_rects(frame: Rect) -> Vec<(HandleKind, Rect)> {
        HandleKind::ALL
            .into_iter()
            .zip(handle_points(frame))
            .flat_map(|(handle, point)| {
                let (primary, secondary) = handle_hit_rects(point, handle);
                std::iter::once((handle, primary)).chain(secondary.map(|rect| (handle, rect)))
            })
            .collect()
    }

    #[test]
    fn no_handle_hit_rect_reaches_into_the_frame() {
        // The interior is where the pointer paints the mask, so a handle that overlapped it
        // would swallow strokes. Asserted against the frame rect itself, never against
        // literals: the rule is "outside the frame", whatever the frame is.
        let frame = handle_frame();
        for (handle, rect) in all_hit_rects(frame) {
            assert!(
                !rect.intersect(frame).is_positive(),
                "{handle:?}: {rect:?} overlaps the frame interior {frame:?}"
            );
            // And it must still TOUCH the frame, or the handle would float away from the
            // edge it resizes.
            assert!(rect.intersects(frame), "{handle:?}: {rect:?} does not touch {frame:?}");
            assert!(frame.expand(HANDLE_RADIUS).contains_rect(rect), "{handle:?}: {rect:?} exceeds the handle margin");
        }
    }

    #[test]
    fn a_handle_covers_every_outer_quadrant_of_its_box_and_none_of_the_inner_one() {
        // Each handle owns the `2r x 2r` box around its point. A quadrant of that box whose
        // centre is inside the frame is the one that must stay uncovered; every other
        // quadrant must be inside one of the handle's hit rectangles. For a side midpoint
        // that leaves a half disc's worth, for a corner three quarters — the L shape that is
        // exactly why a corner needs two rectangles.
        let frame = handle_frame();
        let r = HANDLE_RADIUS;
        for (handle, point) in HandleKind::ALL.into_iter().zip(handle_points(frame)) {
            let (primary, secondary) = handle_hit_rects(point, handle);
            assert_eq!(secondary.is_some(), handle.is_corner(), "{handle:?}: only a corner needs two rects");
            for (dx, dy) in [(-1.0_f32, -1.0_f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let quadrant = Rect::from_two_pos(point, pos2(point.x + dx * r, point.y + dy * r));
                let covered = primary.contains_rect(quadrant) || secondary.is_some_and(|rect| rect.contains_rect(quadrant));
                if frame.contains(quadrant.center()) {
                    assert!(!covered, "{handle:?}: the inner quadrant {quadrant:?} is grabbable");
                    assert!(!primary.intersect(quadrant).is_positive(), "{handle:?}: {primary:?} enters {quadrant:?}");
                    assert!(
                        !secondary.is_some_and(|rect| rect.intersect(quadrant).is_positive()),
                        "{handle:?}: the second rect enters {quadrant:?}"
                    );
                } else {
                    assert!(covered, "{handle:?}: the outer quadrant {quadrant:?} is not grabbable");
                }
            }
        }
    }

    #[test]
    fn a_corners_two_rects_meet_without_overlapping() {
        // Their union is the L the handle paints; an overlap would mean two widgets fighting
        // for the same points, and a gap a dead strip inside the drawn disc.
        let frame = handle_frame();
        for (handle, point) in HandleKind::ALL.into_iter().zip(handle_points(frame)) {
            if !handle.is_corner() {
                continue;
            }
            let (band, stub) = handle_hit_rects(point, handle);
            let stub = stub.expect("a corner handle has two hit rectangles");
            assert!(!band.intersect(stub).is_positive(), "{handle:?}: {band:?} and {stub:?} overlap");
            // They share a full edge: three quadrants of area, in one connected piece.
            assert!((band.area() + stub.area() - 3.0 * HANDLE_RADIUS * HANDLE_RADIUS).abs() < 1e-3, "{handle:?}");
            assert!(band.intersects(stub), "{handle:?}: the two rects are disconnected");
        }
    }

    #[test]
    fn the_painted_arc_of_a_handle_stays_outside_the_frame() {
        // The drawing and the hit area have to agree, or the user grabs somewhere other than
        // where the handle looks. Sampled rather than reasoned about: the arc is the thing
        // `render::paint_handles` actually paints.
        let frame = handle_frame();
        for (handle, point) in HandleKind::ALL.into_iter().zip(handle_points(frame)) {
            let (start, sweep) = handle_arc(handle);
            for step in 0..=32_u16 {
                let angle = start + sweep * f32::from(step) / 32.0;
                let on_arc = point + egui::vec2(angle.cos(), angle.sin()) * HANDLE_RADIUS;
                assert!(!frame.shrink(1e-3).contains(on_arc), "{handle:?}: the arc at {angle} enters {frame:?}");
            }
            // And the omitted sweep really is the part inside the frame: its midpoint —
            // half a turn past the arc's own midpoint — has to be in there.
            let opposite = start + sweep * 0.5 + std::f32::consts::PI;
            let inside = point + egui::vec2(opposite.cos(), opposite.sin()) * HANDLE_RADIUS;
            assert!(frame.contains(inside), "{handle:?}: the omitted part is not the inner one");
        }
    }

    #[test]
    fn screen_delta_converts_through_zoom_and_refuses_a_degenerate_one() {
        assert_eq!(screen_delta_to_px(vec2(20.0, -10.0), 2.0), (10, -5));
        assert_eq!(screen_delta_to_px(vec2(20.0, -10.0), 0.0), (0, 0));
        assert_eq!(screen_delta_to_px(vec2(20.0, -10.0), f32::NAN), (0, 0));
    }

    #[test]
    fn a_keep_in_view_correction_truncates_toward_zero_in_both_directions() {
        // Half a pixel in either direction must produce NO movement: rounding it away from
        // zero is what made the clamp alternate between two origins forever.
        assert_eq!(correction_delta_to_px(vec2(0.625, -0.625), 1.25), (0, 0));
        assert_eq!(correction_delta_to_px(vec2(1.875, -1.875), 1.25), (1, -1));
        assert_eq!(correction_delta_to_px(vec2(20.0, -10.0), 2.0), (10, -5));
        assert_eq!(correction_delta_to_px(vec2(20.0, -10.0), 0.0), (0, 0));
        assert_eq!(correction_delta_to_px(vec2(f32::NAN, 1.0), 1.0), (0, 1));
    }

    #[test]
    fn move_keeps_the_size_and_stays_inside_the_page() {
        let start = rect(10, 10, 100, 80);
        let moved = moved_rect_px(start, (50, 30), 1000, 1000);
        assert_eq!((moved.x, moved.y, moved.w, moved.h), (60, 40, 100, 80));

        // Past the right/bottom edge: the frame stops at the border, keeping its size.
        let clamped = moved_rect_px(start, (10_000, 10_000), 200, 150);
        assert_eq!((clamped.x, clamped.y, clamped.w, clamped.h), (100, 70, 100, 80));

        // Past the origin.
        let clamped = moved_rect_px(start, (-10_000, -10_000), 200, 150);
        assert_eq!((clamped.x, clamped.y), (0, 0));
    }

    #[test]
    fn resize_moves_only_the_edges_the_handle_owns() {
        let start = rect(100, 100, 200, 200);
        let c = free_constraints();

        let r = resized_rect_px(start, HandleKind::Right, (40, 999), 1000, 1000, &c);
        assert_eq!((r.x, r.y, r.w, r.h), (100, 100, 240, 200), "Right must ignore the y delta");

        let r = resized_rect_px(start, HandleKind::TopLeft, (-20, -30), 1000, 1000, &c);
        assert_eq!((r.x, r.y, r.w, r.h), (80, 70, 220, 230));

        let r = resized_rect_px(start, HandleKind::Bottom, (999, -50), 1000, 1000, &c);
        assert_eq!((r.x, r.y, r.w, r.h), (100, 100, 200, 150), "Bottom must ignore the x delta");
    }

    #[test]
    fn resize_snaps_through_nearest_valid_size_and_keeps_the_fixed_corner() {
        let start = rect(100, 100, 64, 64);
        let c = grid_constraints(16, 16);
        // Dragging the left edge by -5 asks for 69 px, which snaps to 64: the RIGHT edge
        // (164) must stay where it was, so the origin comes back to 100.
        let r = resized_rect_px(start, HandleKind::Left, (-5, 0), 1000, 1000, &c);
        assert_eq!(r.w, 64);
        assert_eq!(r.x + r.w, 164);

        // -12 asks for 76, which snaps UP to 80, again anchored on the right edge.
        let r = resized_rect_px(start, HandleKind::Left, (-12, 0), 1000, 1000, &c);
        assert_eq!(r.w, 80);
        assert_eq!(r.x + r.w, 164);
    }

    #[test]
    fn resize_never_leaves_the_page() {
        let c = grid_constraints(16, 16);
        let start = rect(0, 0, 64, 64);
        let r = resized_rect_px(start, HandleKind::BottomRight, (10_000, 10_000), 100, 100, &c);
        assert!(r.x + r.w <= 100 && r.y + r.h <= 100, "got {r:?}");
        assert_eq!((r.w, r.h), (96, 96), "the largest multiple of 16 that fits 100 px");
    }

    #[test]
    fn resize_cannot_collapse_the_frame() {
        let c = free_constraints();
        let start = rect(100, 100, 50, 50);
        let r = resized_rect_px(start, HandleKind::Right, (-10_000, 0), 1000, 1000, &c);
        assert!(r.w >= 1, "got {r:?}");
        let r = resized_rect_px(start, HandleKind::Left, (10_000, 0), 1000, 1000, &c);
        assert!(r.w >= 1, "got {r:?}");
    }

    #[test]
    fn a_page_narrower_than_the_grid_yields_the_whole_page() {
        // Deliberately illegal: `check_size` then reports it and the frame paints red, rather
        // than this function returning a grid unit the frame could not hold without leaving
        // the page — the one invariant that outranks the grid (D2).
        assert_eq!(fit_side_into_page(64, 10, 16), 10);
        // A side that already fits is returned untouched, on or off the grid.
        assert_eq!(fit_side_into_page(64, 100, 16), 64);
        // One that does not falls back to the largest grid multiple the page holds.
        assert_eq!(fit_side_into_page(112, 100, 16), 96);
        assert_eq!(fit_side_into_page(112, 100, 1), 100);
    }

    #[test]
    fn resizing_on_a_page_narrower_than_the_grid_stays_on_the_page() {
        use super::super::geometry::check_size;
        let c = grid_constraints(16, 16);
        // A 10x10 page cannot hold a single grid unit. The resize must still not escape it;
        // the size it settles on is illegal and the frame reports that in red.
        let start = rect(0, 0, 10, 10);
        for handle in HandleKind::ALL {
            let r = resized_rect_px(start, handle, (10_000, 10_000), 10, 10, &c);
            assert!(r.x + r.w <= 10 && r.y + r.h <= 10, "handle {handle:?} escaped the page: {r:?}");
            let r = resized_rect_px(start, handle, (-10_000, -10_000), 10, 10, &c);
            assert!(r.x + r.w <= 10 && r.y + r.h <= 10, "handle {handle:?} escaped the page: {r:?}");
        }
        let r = resized_rect_px(start, HandleKind::BottomRight, (10_000, 10_000), 10, 10, &c);
        assert!(check_size(r.w, r.h, &c).is_some(), "the size must be reported as invalid, not hidden");
    }
}
