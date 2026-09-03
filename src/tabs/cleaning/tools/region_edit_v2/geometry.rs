/*
File: region_edit_v2/geometry.rs

Purpose:
GUI-free geometry core of the on-canvas region frame: every rule of the frame that is maths
rather than painting or input. Nothing here touches `egui::Ui`, `egui::Context`,
`egui::Painter`, a texture or any input state, which is what makes all of it unit-testable
without a window; `Rect` / `Pos2` / `Vec2` are used as plain geometry only (the
`src/widgets/panel_dock/` precedent).

Key structures:
- `FrameConstraints`, `SizeViolation`: the consumer's size requirements and how a size fails
- `FrameChrome`: the strip above the frame, the two rows below it, and the handle margin
- `PageView`, `PageChoice`: one page as the frame sees it, and the page-transition verdict
- `OffscreenArrow`: where to point when a locked frame has scrolled out of view

Key functions:
- `check_size()`, `nearest_valid_size()`: page-pixel size validation and snapping
- `hitbox_rect()`: the frame plus its handles and chrome, i.e. what must stay in the viewport
- `usable_viewport_for()`: the viewport minus the dock panels, cut relative to the hitbox
- `keep_in_view_delta()`: the per-frame clamp, where the page wins over the viewport
- `choose_page()`: the page-transition rule
- `offscreen_arrow()`: the off-screen indicator

Notes:
Two unit systems meet here and must not be mixed: `check_size` / `nearest_valid_size` work in
SOURCE PAGE PIXELS (the units of `crate::canvas::types::OverlayRectPx`, which is what the
frame stores and what `CanvasView::replace_overlay_region_px` consumes), everything else in
SCREEN POINTS. Every function is pure: no interior mutability, no globals, same output for
the same input. Design and the decisions behind it: `dev-docs/region_edit_v2_plan.md`
(§1, §2 D2/D3/D6, §4).
*/

use egui::{Pos2, Rect, Vec2, pos2, vec2};

/// Distance in screen points between the viewport border and the tip of the off-screen
/// arrow, so that the arrow head is drawn inside the viewport instead of on its edge.
pub const ARROW_INSET: f32 = 12.0;

// ---------------------------------------------------------------------------------------
// Size constraints
// ---------------------------------------------------------------------------------------

/// Size requirements a consumer (an AI model, or the step-1 stub) imposes on the frame.
///
/// All fields are in SOURCE PAGE PIXELS. A consumer's declaration reaches this module as
/// plain data, so out-of-range values are sanitized rather than rejected — a pure geometry
/// call must never panic on them: `multiple` below 1 reads as 1, `min_side` below 1 reads as
/// 1, a non-finite `max_aspect` is ignored, and a `max_aspect` below 1.0 reads as 1.0 (a
/// square, the least steep rectangle there is).
#[derive(Debug, Clone, Copy)]
pub struct FrameConstraints {
    /// Both sides must be whole multiples of this. `1` means "no grid".
    pub multiple: usize,
    /// Smallest allowed side length.
    pub min_side: usize,
    /// Largest allowed `w * h`, in page pixels squared. `None` means unlimited.
    pub max_area: Option<u64>,
    /// Largest allowed longest/shortest side ratio, e.g. `8.0`. `None` means unlimited.
    pub max_aspect: Option<f32>,
}

/// How a size fails its `FrameConstraints`.
///
/// `check_size` reports the FIRST failure in the order `nearest_valid_size` resolves them,
/// so the reported violation is always the one snapping would fix first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeViolation {
    /// A side is not a whole multiple of `FrameConstraints::multiple`.
    NotMultiple,
    /// A side is shorter than `FrameConstraints::min_side`.
    TooSmall,
    /// `w * h` exceeds `FrameConstraints::max_area`.
    AreaTooLarge,
    /// The longest/shortest side ratio exceeds `FrameConstraints::max_aspect`.
    AspectTooSteep,
}

/// `c.multiple`, sanitized to at least 1 so the grid maths can never divide by zero.
#[inline]
fn grid_step(c: &FrameConstraints) -> usize {
    c.multiple.max(1)
}

/// `c.min_side`, sanitized to at least 1: a zero-sized frame is not a frame.
#[inline]
fn min_side(c: &FrameConstraints) -> usize {
    c.min_side.max(1)
}

/// `c.max_aspect`, sanitized: `None` for an absent or non-finite limit, otherwise at least
/// 1.0, because a limit below 1.0 would forbid every rectangle including the square.
#[inline]
fn max_aspect(c: &FrameConstraints) -> Option<f32> {
    c.max_aspect.filter(|r| r.is_finite()).map(|r| r.max(1.0))
}

/// Saturating widening of a pixel count for the area maths. A count that does not fit `u64`
/// cannot describe a real page, and saturating keeps the comparison monotonic instead of
/// wrapping into a small number that would pass the area check.
#[inline]
fn as_u64(v: usize) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

/// Saturating narrowing of an area-budget result back to a side length.
#[inline]
fn as_usize(v: u64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

/// A pixel count as `f64`, for the aspect and area-scale maths — the only places a fraction
/// is unavoidable, because `max_aspect` is itself a float and the area scale is a square
/// root. Page-pixel counts are many orders of magnitude below 2^53, where `f64` still
/// represents every integer exactly, so this conversion cannot lose a bit for any input this
/// module can be handed.
#[inline]
fn px_f64(v: usize) -> f64 {
    v as f64
}

/// `None` when the size satisfies every constraint, otherwise the first violation in the
/// resolution order of `nearest_valid_size` (grid, minimum side, area, aspect).
///
/// `w` and `h` are in source page pixels.
#[must_use]
pub fn check_size(w: usize, h: usize, c: &FrameConstraints) -> Option<SizeViolation> {
    let step = grid_step(c);
    if !w.is_multiple_of(step) || !h.is_multiple_of(step) {
        return Some(SizeViolation::NotMultiple);
    }
    let min = min_side(c);
    if w < min || h < min {
        return Some(SizeViolation::TooSmall);
    }
    if let Some(max_area) = c.max_area
        && as_u64(w).saturating_mul(as_u64(h)) > max_area
    {
        return Some(SizeViolation::AreaTooLarge);
    }
    if let Some(ratio) = max_aspect(c) {
        // The minimum-side check above guarantees both sides are at least 1, so the shorter
        // side can never be zero here and the ratio is always defined.
        let (short, long) = if w <= h { (w, h) } else { (h, w) };
        if px_f64(long) > px_f64(short) * f64::from(ratio) {
            return Some(SizeViolation::AspectTooSteep);
        }
    }
    None
}

/// The nearest size that satisfies every constraint, in source page pixels. Used while a
/// resize handle is dragged.
///
/// Resolution order — grid, minimum side, area, aspect — and why it is that order: every
/// step after the first works in whole GRID UNITS and only ever shrinks a side, never below
/// the minimum, so no later step can undo an earlier one. Rounding up to the grid may push a
/// size past the area budget, and the area step then pulls it back WITHOUT leaving the grid;
/// shrinking for the area may steepen the aspect, and the aspect step then shrinks the
/// longer side, which cannot re-break the area.
///
/// The one pair of constraints that can genuinely contradict each other is `min_side` versus
/// `max_area` (`min_side²` may already exceed the budget). **The minimum side wins**: the
/// returned size is then still rejected by `check_size`, which is exactly the state the
/// frame renders in red rather than a size no consumer could use.
///
/// "Nearest" means the result of this sequence, not a global minimum-distance optimum.
#[must_use]
pub fn nearest_valid_size(w: usize, h: usize, c: &FrameConstraints) -> (usize, usize) {
    let step = grid_step(c);
    // Counting in grid units (one unit = `step` page pixels) is what makes the multiple
    // constraint unbreakable: no later step can produce a fraction of a unit.
    let min_units = min_side(c).div_ceil(step).max(1);

    // 1. Grid: nearest unit, halves up. This is the step that follows the drag.
    let mut w_u = round_units(w, step);
    let mut h_u = round_units(h, step);

    // 2. Minimum side, rounded UP to the grid, so a `min_side` that is not itself a multiple
    //    (100 on a grid of 16, say) yields the first legal size above it rather than below.
    w_u = w_u.max(min_units);
    h_u = h_u.max(min_units);

    // 3. Area budget.
    if let Some(max_area) = c.max_area {
        let (nw, nh) = shrink_to_area(w_u, h_u, min_units, step, max_area);
        w_u = nw;
        h_u = nh;
    }

    // 4. Aspect limit.
    if let Some(ratio) = max_aspect(c) {
        let (nw, nh) = shrink_to_aspect(w_u, h_u, ratio);
        w_u = nw;
        h_u = nh;
    }

    (w_u.saturating_mul(step), h_u.saturating_mul(step))
}

/// `v` in whole grid units, rounded to the nearest unit with halves going up.
#[inline]
fn round_units(v: usize, step: usize) -> usize {
    // Saturating, because a near-`usize::MAX` side must not wrap to zero units.
    v.saturating_add(step / 2) / step
}

/// The largest grid size within `max_area` obtained by shrinking `w_u` x `h_u`.
///
/// Never grows a side beyond the one it was given and never returns a side below
/// `min_units`; when `min_units` alone already exceeds the budget the constraints are
/// contradictory and the minimum side wins (see `nearest_valid_size`).
fn shrink_to_area(w_u: usize, h_u: usize, min_units: usize, step: usize, max_area: u64) -> (usize, usize) {
    // Budget in whole grid cells. `w_u * h_u <= max_area / step²` is EXACT rather than
    // conservative: the left-hand side is an integer, so flooring the right-hand side
    // excludes nothing that would have fitted.
    let cell = as_u64(step).saturating_mul(as_u64(step)).max(1);
    let budget = max_area / cell;
    let current = as_u64(w_u).saturating_mul(as_u64(h_u));
    if current <= budget {
        return (w_u, h_u);
    }

    // A uniform scale gives the width its starting point, so a square stays square instead
    // of collapsing onto one axis. `current > budget >= 0` here, so the division is safe and
    // the scale is at most 1.
    let scale = (px_f64(as_usize(budget)) / px_f64(as_usize(current))).sqrt();
    let scaled_w = float_floor_units(px_f64(w_u) * scale).max(min_units);

    // The other axis then follows EXACTLY, in integers, which is what keeps the float above
    // from deciding anything: `budget / width` is the largest height that fits beside that
    // width, and taking it also hands back the unit that flooring the scale tends to lose.
    // The width is re-derived the same way from that height. Capping each at the incoming
    // size keeps this a SHRINK: without the cap a 3x3 under a budget of 8 cells would come
    // out as 2x4, distorting a shape the caller never asked to change.
    let fitted_h = as_usize(budget / as_u64(scaled_w).max(1)).min(h_u).max(min_units);
    let fitted_w = as_usize(budget / as_u64(fitted_h).max(1)).min(w_u).max(min_units);
    (fitted_w, fitted_h)
}

/// Floor of a non-negative, finite scaled side length, in grid units.
#[inline]
fn float_floor_units(v: f64) -> usize {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    // `v` is a scaled-down side length, so it is bounded by the side it came from and the
    // truncation lands well inside `usize`; a float-to-integer cast saturates in Rust and
    // therefore cannot produce a wrapped value even if that bound were ever wrong.
    v.floor() as usize
}

/// Brings the longest/shortest side ratio within `ratio` (>= 1.0) by shrinking the LONGER
/// side only.
///
/// Shrinking rather than growing is what keeps the area step that ran before valid, and the
/// shrink stops at the shorter side, where the ratio is 1 and within any limit — so this
/// step always succeeds and can never drop a side below the minimum.
fn shrink_to_aspect(w_u: usize, h_u: usize, ratio: f32) -> (usize, usize) {
    let (short, long) = if w_u <= h_u { (w_u, h_u) } else { (h_u, w_u) };
    let allowed = px_f64(short) * f64::from(ratio);
    if px_f64(long) <= allowed {
        return (w_u, h_u);
    }
    // Here `short <= allowed < long`, so the floor is a valid unit count in that range.
    let long_new = float_floor_units(allowed).max(short);
    if w_u <= h_u { (w_u, long_new) } else { (long_new, h_u) }
}

// ---------------------------------------------------------------------------------------
// Chrome and hitbox
// ---------------------------------------------------------------------------------------

/// Sizes, in screen points, of the parts of the hitbox that are not the frame itself: the
/// rows above and below it, and the margin its resize handles stick out into.
///
/// Negative and NaN values are read as zero, so a caller that computes a height from a
/// layout that has not run yet cannot produce an infinite or inverted hitbox.
#[derive(Debug, Clone, Copy)]
pub struct FrameChrome {
    /// The drag grip / mask-layer strip above the frame.
    pub top_strip_h: f32,
    /// The action button row below the frame.
    pub buttons_h: f32,
    /// The status line below the button row.
    pub status_h: f32,
    /// Spacing inserted at each of the three seams: strip/frame, frame/buttons,
    /// buttons/status.
    pub gap: f32,
    /// Smallest width, in screen points, the chrome rows may have.
    ///
    /// The rows carry a status sentence and three captioned buttons, so they cannot inherit
    /// the frame's SCREEN width: a minimum-side frame at the canvas' minimum zoom is a few
    /// points wide, and rows that narrow either spill their text over the artwork or floor
    /// three buttons at one point each. The rows are widened symmetrically about the frame's
    /// centre to reach this width, and the hitbox grows with them so keep-in-view still holds
    /// them on screen.
    pub min_row_w: f32,
    /// How far the resize handles stick OUT of the frame, on every side.
    ///
    /// The handles are drawn and hit-tested entirely outside the frame — half-discs on the
    /// side midpoints, three-quarter discs on the corners — so that neither their paint nor
    /// their click area intrudes into the interior, where the pointer paints the mask. That
    /// puts them outside the frame rectangle and inside the hitbox: the hitbox grows by this
    /// margin on all four sides, so keep-in-view holds the handles on screen (a handle at the
    /// viewport border would otherwise be unreachable) and the chrome rows are pushed clear
    /// of them instead of overlapping them.
    pub handle_margin: f32,
}

impl FrameChrome {
    /// Height the chrome adds ABOVE the frame: the handle margin, the top strip and one gap.
    #[must_use]
    pub fn above(&self) -> f32 {
        non_negative(self.handle_margin) + non_negative(self.top_strip_h) + non_negative(self.gap)
    }

    /// Height the chrome adds BELOW the frame: the handle margin, then gap, button row, gap,
    /// status line.
    #[must_use]
    pub fn below(&self) -> f32 {
        non_negative(self.handle_margin)
            + non_negative(self.gap)
            + non_negative(self.buttons_h)
            + non_negative(self.gap)
            + non_negative(self.status_h)
    }
}

/// A chrome height that is guaranteed non-negative and not NaN.
///
/// `f32::max` returns the non-NaN operand when one side is NaN, so this collapses NaN to
/// zero as well as clamping negatives.
#[inline]
fn non_negative(v: f32) -> f32 {
    v.max(0.0)
}

/// The frame plus its resize handles, its top strip and its two rows below — what must stay
/// inside the viewport.
///
/// The rows are as wide as the frame, or `FrameChrome::min_row_w` when the frame is narrower
/// than that on screen; the extra width is split evenly on both sides, so the chrome stays
/// centred on the frame. The horizontal growth is part of the hitbox on purpose: it is what
/// keeps the widened rows inside the viewport under the keep-in-view clamp, and what makes
/// the pointer over them belong to the frame rather than to the canvas.
///
/// The result also always clears `FrameChrome::handle_margin` on every side, because the
/// handles are drawn and hit-tested outside the frame: a hitbox that stopped at the frame
/// would let the keep-in-view clamp park a handle beyond the viewport border, where nothing
/// could grab it, and would leave the handles outside the `egui::Area` the frame senses in.
#[must_use]
pub fn hitbox_rect(frame: Rect, chrome: &FrameChrome) -> Rect {
    let margin = non_negative(chrome.handle_margin);
    // The rows may be narrower than the frame-plus-handles when the frame is wide, so the
    // horizontal extent is the larger of the two, never just the row width.
    let width = chrome_row_width(frame.width(), chrome).max(non_negative(frame.width()) + margin * 2.0);
    let half = width * 0.5;
    let center_x = frame.center().x;
    Rect::from_min_max(
        pos2(center_x - half, frame.min.y - chrome.above()),
        pos2(center_x + half, frame.max.y + chrome.below()),
    )
}

/// Width of the chrome rows for a frame `frame_w` screen points wide.
///
/// One place, so the hitbox the viewport clamp works on and the rows the frame paints can
/// never disagree about how wide the chrome is.
#[must_use]
pub fn chrome_row_width(frame_w: f32, chrome: &FrameChrome) -> f32 {
    non_negative(frame_w).max(non_negative(chrome.min_row_w))
}

// ---------------------------------------------------------------------------------------
// Viewport
// ---------------------------------------------------------------------------------------

/// The viewport edge one dock panel is cut from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CutEdge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Which viewport edge a panel must be cut from, and how deep that cut is, given where the
/// hitbox sits. `None` when the panel cannot hide the hitbox from any direction.
///
/// `hit` is the panel already intersected with the viewport, so all four slabs are within
/// `[0, viewport side]`. All rects are in screen points.
///
/// The rule is stated per AXIS, because a cut is a full-width or full-height band and a band
/// only obstructs a frame that shares the band's other axis:
/// - shares neither the hitbox's columns nor its rows (diagonally offset): cut nothing;
/// - shares its columns only: the panel is wholly above or wholly below, so cut that edge;
/// - shares its rows only: the panel is wholly left or wholly right, so cut that edge;
/// - shares both (the panel really is over the hitbox): cut the edge that removes the LEAST
///   AREA — `slab × viewport height` for left/right against `slab × viewport width` for
///   top/bottom. Comparing bare slabs instead is what made a right-docked panel that starts
///   near the top of the viewport cut a full-width top band and cap the frame there.
///
/// Ties resolve in the fixed order left, right, top, bottom. The verdict depends only on this
/// panel, the hitbox and the viewport, never on the other panels, which is what keeps
/// `usable_viewport_for` order-independent.
fn panel_cut(hit: Rect, hitbox: Rect, viewport: Rect) -> Option<(CutEdge, f32)> {
    let left = hit.max.x - viewport.min.x;
    let right = viewport.max.x - hit.min.x;
    let top = hit.max.y - viewport.min.y;
    let bottom = viewport.max.y - hit.min.y;

    // Strict comparisons: a panel that only touches a hitbox edge shares zero columns/rows
    // with it and therefore cannot hide it along that axis.
    let shares_columns = hit.min.x < hitbox.max.x && hit.max.x > hitbox.min.x;
    let shares_rows = hit.min.y < hitbox.max.y && hit.max.y > hitbox.min.y;

    match (shares_columns, shares_rows) {
        (false, false) => None,
        (true, false) => Some(if hit.max.y <= hitbox.min.y { (CutEdge::Top, top) } else { (CutEdge::Bottom, bottom) }),
        (false, true) => Some(if hit.max.x <= hitbox.min.x { (CutEdge::Left, left) } else { (CutEdge::Right, right) }),
        (true, true) => {
            let vw = non_negative(viewport.width());
            let vh = non_negative(viewport.height());
            let candidates = [
                (CutEdge::Left, left, left * vh),
                (CutEdge::Right, right, right * vh),
                (CutEdge::Top, top, top * vw),
                (CutEdge::Bottom, bottom, bottom * vw),
            ];
            // Strictly-less keeps the first candidate on a tie, and leaves the first one
            // standing when an area is NaN rather than propagating the NaN into the choice.
            let mut best = candidates[0];
            for candidate in &candidates[1..] {
                if candidate.2 < best.2 {
                    best = *candidate;
                }
            }
            Some((best.0, best.1))
        }
    }
}

/// The viewport minus every dock panel that could hide `hitbox` from some direction.
///
/// `hitbox` is the frame's hitbox (`hitbox_rect`) in screen points — the rectangle the
/// keep-in-view clamp holds inside the result. The cut is HITBOX-RELATIVE and per axis: a
/// panel that shares neither the hitbox's columns nor its rows costs nothing, one that shares
/// exactly one axis is cut from the edge it lies on, and one that is genuinely over the hitbox
/// is cut from the edge that removes the least AREA. `panel_cut` states the rule in full and
/// explains why area, not slab depth, decides.
///
/// The largest cut per edge wins and each panel is judged on its own, so the result does not
/// depend on the order the caller collected the panels in.
///
/// The result may be EMPTY (zero width and/or height) when the panels cover the viewport; it
/// is never inverted.
///
/// Because the cut set depends on where the hitbox is, a keep-in-view correction can change
/// it on the NEXT frame. That converges rather than oscillating: a correction only ever
/// pushes the hitbox AWAY from the edge that cut it, and the clamp is one-directional — a cut
/// that later disappears never pulls the frame back.
#[must_use]
pub fn usable_viewport_for(hitbox: Rect, viewport: Rect, panels: &[Rect]) -> Rect {
    let mut cut_left = 0.0_f32;
    let mut cut_right = 0.0_f32;
    let mut cut_top = 0.0_f32;
    let mut cut_bottom = 0.0_f32;

    for panel in panels {
        let hit = viewport.intersect(*panel);
        // A panel outside the viewport — or one that only touches its border — cuts nothing.
        if !hit.is_positive() {
            continue;
        }
        let Some((edge, slab)) = panel_cut(hit, hitbox, viewport) else {
            continue;
        };
        match edge {
            CutEdge::Left => cut_left = cut_left.max(slab),
            CutEdge::Right => cut_right = cut_right.max(slab),
            CutEdge::Top => cut_top = cut_top.max(slab),
            CutEdge::Bottom => cut_bottom = cut_bottom.max(slab),
        }
    }

    let min_x = viewport.min.x + cut_left;
    let min_y = viewport.min.y + cut_top;
    // Opposite cuts can overlap; collapse onto the min-edge cut instead of inverting, so
    // callers always get `min <= max`.
    let max_x = (viewport.max.x - cut_right).max(min_x);
    let max_y = (viewport.max.y - cut_bottom).max(min_y);
    Rect::from_min_max(pos2(min_x, min_y), pos2(max_x, max_y))
}

/// Translation that keeps `frame` inside `page` and, as far as that allows, its hitbox
/// inside `viewport`.
///
/// The page constraint WINS (D2): a frame may never leave the page it edits, even when that
/// leaves part of its chrome outside the viewport. When an axis does not fit the viewport at
/// all, the hitbox is aligned to that axis' MIN edge, so the top strip and the frame itself
/// stay reachable and only the rows below are cut off. All rects are in screen points.
///
/// Non-finite inputs propagate into the result rather than being invented away; the caller
/// is handing this function a rect it derived from the canvas, and a NaN there is a bug
/// worth seeing rather than hiding.
#[must_use]
pub fn keep_in_view_delta(frame: Rect, chrome: &FrameChrome, page: Rect, viewport: Rect) -> Vec2 {
    let hitbox = hitbox_rect(frame, chrome);
    vec2(
        axis_delta(
            (hitbox.min.x, hitbox.max.x),
            (viewport.min.x, viewport.max.x),
            (frame.min.x, frame.max.x),
            (page.min.x, page.max.x),
        ),
        axis_delta(
            (hitbox.min.y, hitbox.max.y),
            (viewport.min.y, viewport.max.y),
            (frame.min.y, frame.max.y),
            (page.min.y, page.max.y),
        ),
    )
}

/// One axis of `keep_in_view_delta`. Every pair is `(min, max)` on that axis.
fn axis_delta(hitbox: (f32, f32), viewport: (f32, f32), frame: (f32, f32), page: (f32, f32)) -> f32 {
    // 1. Viewport: the smallest move that makes the hitbox fully visible.
    let hitbox_len = hitbox.1 - hitbox.0;
    let viewport_len = viewport.1 - viewport.0;
    let wanted = if hitbox_len > viewport_len {
        // Does not fit at all: align to the viewport's min edge (D3).
        viewport.0 - hitbox.0
    } else if hitbox.0 < viewport.0 {
        viewport.0 - hitbox.0
    } else if hitbox.1 > viewport.1 {
        viewport.1 - hitbox.1
    } else {
        0.0
    };

    // 2. Page wins: clamp the wanted move to what keeps the FRAME (not the hitbox, whose
    //    chrome is allowed to hang over the page) inside the page.
    let frame_len = frame.1 - frame.0;
    let page_len = page.1 - page.0;
    if frame_len > page_len {
        // Larger than the page on this axis, so no position satisfies it. Align to the
        // page's min edge deterministically rather than leaving the frame where it was.
        return page.0 - frame.0;
    }
    // `frame_len <= page_len` makes `to_page_min <= to_page_max`, so `clamp` cannot panic.
    let to_page_min = page.0 - frame.0;
    let to_page_max = page.1 - frame.1;
    wanted.clamp(to_page_min, to_page_max)
}

// ---------------------------------------------------------------------------------------
// Page transition
// ---------------------------------------------------------------------------------------

/// One page as the frame sees it this frame. All rects are in screen points.
///
/// Only the VISIBLE part is carried: the transition rule of the design is stated entirely in
/// visible areas, and a second rect nothing reads would be a field the caller has to keep
/// truthful for no one's benefit.
#[derive(Debug, Clone, Copy)]
pub struct PageView {
    /// Index of the page in the canvas page list.
    pub page_idx: usize,
    /// The page's screen rect intersected with the usable viewport. May be empty, and may
    /// arrive INVERTED from `Rect::intersect` when the page is fully scrolled away.
    pub visible: Rect,
}

/// Verdict of `choose_page`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageChoice {
    /// Keep editing the current page.
    Stay,
    /// Re-anchor the frame to this page index.
    MoveTo(usize),
}

/// The page-transition rule of the design, in full:
/// 1. the frame still fits inside `current.visible` -> `Stay`
/// 2. otherwise take the candidate with the largest visible area:
///    a. its visible area is not larger than the current one -> `Stay`
///    b. it is smaller than the frame in width or height -> `Stay`, unless `current.visible`
///    is empty, in which case `MoveTo`
///    c. otherwise -> `MoveTo`
/// 3. `current.visible` empty and a candidate exists -> `MoveTo` the largest
///
/// Rule 3 falls out of rule 2b's exception; it is not a separate branch. Candidates carrying
/// `current.page_idx` are ignored (a move to the current page is not a move) and so are
/// candidates with no visible area at all — without that filter, two invisible pages would
/// trip rule 2b's exception and move the frame to a page nobody can see.
///
/// `frame_screen_size` is the frame's size in screen points, without its chrome: the rows
/// above and below may hang outside the page.
#[must_use]
pub fn choose_page(current: &PageView, frame_screen_size: Vec2, candidates: &[PageView]) -> PageChoice {
    // 1.
    if fits_in(frame_screen_size, current.visible) {
        return PageChoice::Stay;
    }

    // 2. The best other page; ties keep the earliest entry of the slice.
    let current_area = visible_area(current.visible);
    let mut best: Option<&PageView> = None;
    for candidate in candidates {
        if candidate.page_idx == current.page_idx || visible_area(candidate.visible) <= 0.0 {
            continue;
        }
        match best {
            None => best = Some(candidate),
            Some(previous) if visible_area(candidate.visible) > visible_area(previous.visible) => {
                best = Some(candidate);
            }
            Some(_) => {}
        }
    }
    let Some(best) = best else {
        return PageChoice::Stay;
    };

    // 2a.
    if visible_area(best.visible) <= current_area {
        return PageChoice::Stay;
    }
    // 2b. The candidate cannot hold the frame: wait until the current page is gone entirely
    //     (rule 3) instead of moving onto a page the frame does not fit either.
    if !fits_in(frame_screen_size, best.visible) {
        return if current_area <= 0.0 { PageChoice::MoveTo(best.page_idx) } else { PageChoice::Stay };
    }
    // 2c.
    PageChoice::MoveTo(best.page_idx)
}

/// Area of a visible slice, in square screen points.
///
/// `Rect::intersect` returns an INVERTED rect for two rects that do not overlap, and
/// `Rect::area` would multiply two negative sides back into a positive number, so both sides
/// are floored at zero first.
#[inline]
fn visible_area(r: Rect) -> f32 {
    r.width().max(0.0) * r.height().max(0.0)
}

/// Whether a rect of `size` fits inside `r`, treating an inverted `r` as empty.
#[inline]
fn fits_in(size: Vec2, r: Rect) -> bool {
    size.x <= r.width().max(0.0) && size.y <= r.height().max(0.0)
}

// ---------------------------------------------------------------------------------------
// Off-screen indicator
// ---------------------------------------------------------------------------------------

/// Where to draw the "the frame is over there" arrow.
#[derive(Debug, Clone, Copy)]
pub struct OffscreenArrow {
    /// Tip of the arrow, on the viewport border inset by `ARROW_INSET`.
    pub tip: Pos2,
    /// Unit vector from the viewport centre towards the target.
    pub dir: Vec2,
}

/// Where to draw the off-screen arrow, or `None` while the frame is visible.
///
/// "Visible" means the overlap of `target` and `viewport` has POSITIVE AREA: a frame that
/// merely touches the viewport border paints nothing on screen and still deserves an arrow.
/// A zero-sized target is off-screen by the same rule, which no real frame is (its minimum
/// side is at least one page pixel).
///
/// The tip is placed where the ray from the viewport centre towards the target's centre
/// leaves the viewport inset by `ARROW_INSET`. Returns `None` for a degenerate direction,
/// which only a non-finite input rect can produce.
#[must_use]
pub fn offscreen_arrow(target: Rect, viewport: Rect) -> Option<OffscreenArrow> {
    if viewport.intersect(target).is_positive() {
        return None;
    }
    let center = viewport.center();
    let dir = (target.center() - center).normalized();
    // `Vec2::normalized` returns the input unchanged for a zero vector, so a zero here means
    // the two centres coincide and there is no direction to point in.
    if !dir.is_finite() || dir == Vec2::ZERO {
        return None;
    }

    // Parametric ray/border intersection against the inset viewport. A viewport narrower
    // than twice the inset collapses that axis onto the centre instead of inverting.
    let half_w = (viewport.width() * 0.5 - ARROW_INSET).max(0.0);
    let half_h = (viewport.height() * 0.5 - ARROW_INSET).max(0.0);
    let t_x = if dir.x == 0.0 { f32::INFINITY } else { half_w / dir.x.abs() };
    let t_y = if dir.y == 0.0 { f32::INFINITY } else { half_h / dir.y.abs() };
    // `dir` is a unit vector, so at most one component is zero and `t` is finite.
    let t = t_x.min(t_y);
    Some(OffscreenArrow { tip: center + dir * t, dir })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The size contract FLUX.2 klein declares, reused as a realistic constraint set.
    fn klein() -> FrameConstraints {
        FrameConstraints { multiple: 16, min_side: 128, max_area: Some(1_000_000), max_aspect: Some(8.0) }
    }

    fn unconstrained() -> FrameConstraints {
        FrameConstraints { multiple: 1, min_side: 1, max_area: None, max_aspect: None }
    }

    fn chrome() -> FrameChrome {
        FrameChrome { top_strip_h: 20.0, buttons_h: 24.0, status_h: 16.0, gap: 4.0, min_row_w: 240.0, handle_margin: 8.0 }
    }

    fn rect(min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> Rect {
        Rect::from_min_max(pos2(min_x, min_y), pos2(max_x, max_y))
    }

    // -- check_size ---------------------------------------------------------------------

    #[test]
    fn a_side_off_the_grid_is_reported_as_not_multiple() {
        assert_eq!(check_size(250, 256, &klein()), Some(SizeViolation::NotMultiple));
        assert_eq!(check_size(256, 250, &klein()), Some(SizeViolation::NotMultiple));
    }

    #[test]
    fn the_grid_is_reported_before_the_minimum_side() {
        // 100 is both off the grid and below the minimum; snapping fixes the grid first, so
        // that is what the user is told about first.
        assert_eq!(check_size(100, 100, &klein()), Some(SizeViolation::NotMultiple));
    }

    #[test]
    fn a_side_below_the_minimum_is_reported_as_too_small() {
        assert_eq!(check_size(64, 256, &klein()), Some(SizeViolation::TooSmall));
    }

    #[test]
    fn an_area_over_the_budget_is_reported_as_area_too_large() {
        // 1600 x 800 = 1.28 Mpx against a 1 Mpx budget; both sides are legal on their own.
        assert_eq!(check_size(1600, 800, &klein()), Some(SizeViolation::AreaTooLarge));
    }

    #[test]
    fn a_ratio_over_the_limit_is_reported_as_aspect_too_steep() {
        // 16:1 against a limit of 8:1, with an area well inside the budget.
        assert_eq!(check_size(2048, 128, &klein()), Some(SizeViolation::AspectTooSteep));
        // Exactly at the limit is legal.
        assert_eq!(check_size(1024, 128, &klein()), None);
    }

    #[test]
    fn a_zero_side_never_passes_even_without_a_minimum() {
        let c = FrameConstraints { multiple: 1, min_side: 0, max_area: None, max_aspect: None };
        assert_eq!(check_size(0, 10, &c), Some(SizeViolation::TooSmall));
    }

    #[test]
    fn a_multiple_of_zero_is_read_as_one_and_does_not_panic() {
        let c = FrameConstraints { multiple: 0, min_side: 1, max_area: None, max_aspect: None };
        assert_eq!(check_size(7, 13, &c), None);
        assert_eq!(nearest_valid_size(7, 13, &c), (7, 13));
    }

    // -- nearest_valid_size -------------------------------------------------------------

    #[test]
    fn a_size_that_satisfies_every_constraint_is_returned_unchanged() {
        assert_eq!(nearest_valid_size(256, 512, &klein()), (256, 512));
        assert_eq!(nearest_valid_size(7, 13, &unconstrained()), (7, 13));
    }

    #[test]
    fn an_off_grid_size_snaps_to_the_nearest_multiple() {
        assert_eq!(nearest_valid_size(250, 262, &klein()), (256, 256));
        // Halves round up.
        assert_eq!(nearest_valid_size(264, 264, &klein()), (272, 272));
    }

    #[test]
    fn a_size_below_the_minimum_is_raised_to_it() {
        assert_eq!(nearest_valid_size(64, 64, &klein()), (128, 128));
    }

    #[test]
    fn a_minimum_side_off_the_grid_is_rounded_up_to_the_next_multiple() {
        // 100 is not a multiple of 16: the first legal size at or above it is 112.
        let c = FrameConstraints { multiple: 16, min_side: 100, max_area: None, max_aspect: None };
        let (w, h) = nearest_valid_size(10, 10, &c);
        assert_eq!((w, h), (112, 112));
        assert_eq!(w % 16, 0);
        assert!(w >= 100);
    }

    #[test]
    fn rounding_up_to_the_grid_never_leaves_the_area_budget_broken() {
        // 255 rounds up to 256, and 256 x 256 = 65536 exceeds the 65000 budget, so the area
        // step has to pull one axis back — without leaving the grid.
        let c = FrameConstraints { multiple: 16, min_side: 16, max_area: Some(65_000), max_aspect: None };
        let (w, h) = nearest_valid_size(255, 255, &c);
        assert_eq!(check_size(w, h, &c), None);
        assert_eq!((w % 16, h % 16), (0, 0));
        assert!(w * h <= 65_000, "{w}x{h} is over the budget");
    }

    #[test]
    fn shrinking_to_the_area_budget_stays_on_the_grid_and_within_every_other_rule() {
        let (w, h) = nearest_valid_size(1600, 800, &klein());
        assert_eq!(check_size(w, h, &klein()), None, "got {w}x{h}");
        assert_eq!((w % 16, h % 16), (0, 0));
        assert_eq!((w, h), (1408, 704));
    }

    #[test]
    fn a_square_stays_square_when_the_area_budget_shrinks_it() {
        // The area fix-up must not hand the freed units to one axis: 10x10 under 50 cells
        // becomes 7x7, not 10x5.
        let c = FrameConstraints { multiple: 1, min_side: 1, max_area: Some(50), max_aspect: None };
        assert_eq!(nearest_valid_size(10, 10, &c), (7, 7));
    }

    #[test]
    fn a_steep_aspect_is_fixed_by_shrinking_the_longer_side() {
        // The shorter side is left alone, so the result is 8:1 at 128 px, not 256 x 2048.
        assert_eq!(nearest_valid_size(2048, 128, &klein()), (1024, 128));
        assert_eq!(nearest_valid_size(128, 2048, &klein()), (128, 1024));
    }

    #[test]
    fn the_aspect_fix_up_never_drops_a_side_below_the_minimum() {
        let c = FrameConstraints { multiple: 1, min_side: 100, max_area: None, max_aspect: Some(1.0) };
        let (w, h) = nearest_valid_size(100, 400, &c);
        assert_eq!((w, h), (100, 100));
        assert_eq!(check_size(w, h, &c), None);
    }

    #[test]
    fn the_minimum_side_wins_over_a_contradictory_area_budget() {
        // min_side² = 16384 can never fit a budget of 1000: the frame keeps a usable size
        // and stays INVALID, which is the state the UI paints red.
        let c = FrameConstraints { multiple: 1, min_side: 128, max_area: Some(1_000), max_aspect: None };
        let (w, h) = nearest_valid_size(500, 500, &c);
        assert_eq!((w, h), (128, 128));
        assert_eq!(check_size(w, h, &c), Some(SizeViolation::AreaTooLarge));
    }

    #[test]
    fn degenerate_sizes_snap_to_the_first_legal_size() {
        assert_eq!(nearest_valid_size(0, 0, &unconstrained()), (1, 1));
        assert_eq!(nearest_valid_size(1, 1, &unconstrained()), (1, 1));
        let grid = FrameConstraints { multiple: 16, min_side: 0, max_area: None, max_aspect: None };
        // Rounding alone would give zero units; the floor of one unit rescues it.
        assert_eq!(nearest_valid_size(0, 1, &grid), (16, 16));
    }

    #[test]
    fn every_satisfiable_snap_result_passes_check_size() {
        let sets = [
            klein(),
            unconstrained(),
            FrameConstraints { multiple: 8, min_side: 32, max_area: Some(120_000), max_aspect: Some(3.0) },
            FrameConstraints { multiple: 64, min_side: 64, max_area: Some(500_000), max_aspect: Some(1.5) },
        ];
        for c in &sets {
            for w in [0_usize, 1, 15, 63, 130, 777, 1500, 4000] {
                for h in [0_usize, 1, 17, 64, 129, 800, 2600, 5000] {
                    let (sw, sh) = nearest_valid_size(w, h, c);
                    assert_eq!(check_size(sw, sh, c), None, "{w}x{h} -> {sw}x{sh} for {c:?}");
                }
            }
        }
    }

    // -- hitbox_rect --------------------------------------------------------------------

    #[test]
    fn the_hitbox_adds_the_handle_margin_the_strip_above_and_both_rows_below() {
        // 300 pt wide: already wider than the chrome minimum, so the horizontal growth is
        // the handle margin alone.
        let frame = rect(100.0, 100.0, 400.0, 200.0);
        let hit = hitbox_rect(frame, &chrome());
        // above = handles 8 + strip 20 + gap 4; below = handles 8 + gap 4 + buttons 24 +
        // gap 4 + status 16.
        assert_eq!(hit.min.y, 68.0);
        assert_eq!(hit.max.y, 256.0);
        assert_eq!(hit.min.x, frame.min.x - 8.0);
        assert_eq!(hit.max.x, frame.max.x + 8.0);
    }

    #[test]
    fn the_hitbox_always_clears_the_handles_that_stick_out_of_the_frame() {
        // The handles are drawn and hit-tested OUTSIDE the frame, so every one of them has
        // to be inside the hitbox — otherwise the keep-in-view clamp can park one beyond the
        // viewport border where nothing can grab it.
        for frame in [rect(100.0, 100.0, 400.0, 200.0), rect(500.0, 100.0, 512.8, 200.0), rect(0.0, 0.0, 1.0, 1.0)] {
            let hit = hitbox_rect(frame, &chrome());
            assert!(hit.contains_rect(frame.expand(8.0)), "{frame:?} handles escape {hit:?}");
        }
    }

    #[test]
    fn a_frame_narrower_than_the_chrome_widens_the_hitbox_around_its_centre() {
        // The case the canvas' 0.2 zoom floor produces: a minimum-side frame is a few points
        // wide, and rows that narrow could hold neither the status sentence nor three buttons.
        let frame = rect(500.0, 100.0, 512.8, 200.0);
        let hit = hitbox_rect(frame, &chrome());
        assert_eq!(chrome_row_width(frame.width(), &chrome()), 240.0);
        assert!((hit.width() - 240.0).abs() < 1e-3, "{hit:?}");
        assert!((hit.center().x - frame.center().x).abs() < 1e-3, "the chrome stays centred on the frame");
        // The vertical extent is unchanged by the widening.
        assert_eq!(hit.min.y, 68.0);
        assert_eq!(hit.max.y, 256.0);
    }

    #[test]
    fn negative_and_nan_chrome_sizes_are_read_as_zero() {
        let odd = FrameChrome {
            top_strip_h: -50.0,
            buttons_h: f32::NAN,
            status_h: 10.0,
            gap: -1.0,
            min_row_w: -8.0,
            handle_margin: f32::NAN,
        };
        let hit = hitbox_rect(rect(0.0, 0.0, 10.0, 10.0), &odd);
        assert_eq!(hit.min.y, 0.0);
        assert_eq!(hit.max.y, 20.0);
        // A negative minimum row width and a NaN handle margin are both read as zero, so the
        // rows stay the frame's width.
        assert_eq!(hit.min.x, 0.0);
        assert_eq!(hit.max.x, 10.0);
    }

    // -- usable_viewport_for ------------------------------------------------------------

    fn viewport() -> Rect {
        rect(0.0, 0.0, 1000.0, 800.0)
    }

    /// A hitbox in the middle of `viewport()`, overlapping none of the edge panels below.
    fn middle_hitbox() -> Rect {
        rect(400.0, 300.0, 600.0, 500.0)
    }

    #[test]
    fn no_panels_leave_the_viewport_untouched() {
        assert_eq!(usable_viewport_for(middle_hitbox(), viewport(), &[]), viewport());
    }

    #[test]
    fn a_full_edge_panel_cuts_only_its_own_slab() {
        // Each of these spans the whole opposite axis, so it shares that axis with any hitbox
        // and is cut from the edge it is docked against.
        let left = rect(0.0, 0.0, 200.0, 800.0);
        let right = rect(800.0, 0.0, 1000.0, 800.0);
        let top = rect(0.0, 0.0, 1000.0, 100.0);
        let bottom = rect(0.0, 700.0, 1000.0, 800.0);
        let hitbox = middle_hitbox();
        assert_eq!(usable_viewport_for(hitbox, viewport(), &[left]), rect(200.0, 0.0, 1000.0, 800.0));
        assert_eq!(usable_viewport_for(hitbox, viewport(), &[right]), rect(0.0, 0.0, 800.0, 800.0));
        assert_eq!(usable_viewport_for(hitbox, viewport(), &[top]), rect(0.0, 100.0, 1000.0, 800.0));
        assert_eq!(usable_viewport_for(hitbox, viewport(), &[bottom]), rect(0.0, 0.0, 1000.0, 700.0));
        assert_eq!(usable_viewport_for(hitbox, viewport(), &[left, right, top, bottom]), rect(200.0, 100.0, 800.0, 700.0));
    }

    #[test]
    fn a_panel_diagonally_offset_from_the_hitbox_cuts_nothing() {
        // Top-left panel, hitbox at the bottom right: it shares neither columns nor rows, so
        // it cannot hide the frame along either axis.
        let panel = rect(0.0, 0.0, 300.0, 200.0);
        let hitbox = rect(600.0, 500.0, 800.0, 700.0);
        assert_eq!(usable_viewport_for(hitbox, viewport(), &[panel]), viewport());
    }

    #[test]
    fn a_panel_in_the_hitboxs_columns_cuts_the_top_above_and_the_bottom_below() {
        let hitbox = middle_hitbox();
        let above = rect(350.0, 0.0, 650.0, 150.0);
        let below = rect(350.0, 650.0, 650.0, 800.0);
        assert_eq!(usable_viewport_for(hitbox, viewport(), &[above]), rect(0.0, 150.0, 1000.0, 800.0));
        assert_eq!(usable_viewport_for(hitbox, viewport(), &[below]), rect(0.0, 0.0, 1000.0, 650.0));
    }

    #[test]
    fn a_panel_in_the_hitboxs_rows_cuts_the_left_or_the_right() {
        let hitbox = middle_hitbox();
        let to_the_left = rect(0.0, 250.0, 150.0, 550.0);
        let to_the_right = rect(850.0, 250.0, 1000.0, 550.0);
        assert_eq!(usable_viewport_for(hitbox, viewport(), &[to_the_left]), rect(150.0, 0.0, 1000.0, 800.0));
        assert_eq!(usable_viewport_for(hitbox, viewport(), &[to_the_right]), rect(0.0, 0.0, 850.0, 800.0));
    }

    #[test]
    fn a_panel_over_the_hitbox_is_cut_from_the_least_area_edge() {
        // Overlaps the hitbox in both axes. Slabs: left 450, right 620, top 400, bottom 480.
        // Areas: left 450*800 = 360_000, right 620*800 = 496_000, top 400*1000 = 400_000,
        // bottom 480*1000 = 480_000 — the left is the cheapest cut, and here it is also the
        // smallest slab, so the two readings agree.
        let panel = rect(380.0, 320.0, 450.0, 400.0);
        assert_eq!(usable_viewport_for(middle_hitbox(), viewport(), &[panel]), rect(450.0, 0.0, 1000.0, 800.0));
    }

    #[test]
    fn least_area_beats_smallest_slab_when_the_two_disagree() {
        // A WIDE viewport: a full-width top band is expensive, so a deeper side cut is still
        // the cheaper one. Panel over the hitbox in both axes; slabs left 200, right 1900,
        // top 150, bottom 400. Smallest slab is the top (150), least area is the left
        // (200*400 = 80_000 against the top band's 150*2000 = 300_000).
        let wide = rect(0.0, 0.0, 2000.0, 400.0);
        let wide_hitbox = rect(100.0, 100.0, 400.0, 300.0);
        let wide_panel = rect(0.0, 0.0, 200.0, 150.0);
        assert_eq!(usable_viewport_for(wide_hitbox, wide, &[wide_panel]), rect(200.0, 0.0, 2000.0, 400.0));

        // The mirror image on a TALL viewport, so the rule is pinned in both directions:
        // slabs left 150, right 400, top 200, bottom 2000. Smallest slab is the left (150),
        // least area is the top (200*400 = 80_000 against the left band's 150*2000 = 300_000).
        let tall = rect(0.0, 0.0, 400.0, 2000.0);
        let tall_hitbox = rect(100.0, 100.0, 300.0, 400.0);
        let tall_panel = rect(0.0, 0.0, 150.0, 200.0);
        assert_eq!(usable_viewport_for(tall_hitbox, tall, &[tall_panel]), rect(0.0, 200.0, 400.0, 2000.0));
    }

    #[test]
    fn a_right_docked_panel_near_the_top_never_cuts_a_top_band() {
        // THE REPORTED BUG, at the real layout's proportions: a 2000x1020 pt canvas viewport
        // with the «Инструменты клина» dock panel docked at the RIGHT but starting near the
        // top. Its top slab (310) is smaller than its right slab (380), so the old
        // smallest-slab rule cut a full-width band off the top and the on-canvas frame could
        // not be dragged above y = 310 even though the canvas there was empty.
        let viewport = rect(0.0, 0.0, 2000.0, 1020.0);
        let panel = rect(1620.0, 45.0, 1960.0, 310.0);
        let top_slab = panel.max.y - viewport.min.y;
        let right_slab = viewport.max.x - panel.min.x;
        assert!(top_slab < right_slab, "the top slab must be the smaller one, or this is not the reported case");

        // a) The frame in the middle of the canvas: the panel shares neither its columns nor
        //    its rows, so it costs the frame nothing at all.
        let middle = rect(700.0, 400.0, 1100.0, 700.0);
        assert_eq!(usable_viewport_for(middle, viewport, &[panel]), viewport);

        // b) The frame dragged up into the panel's rows but still left of it: rows only, so
        //    the cut is the RIGHT one and the full viewport height stays available.
        let dragged_up = rect(700.0, 60.0, 1100.0, 360.0);
        assert_eq!(usable_viewport_for(dragged_up, viewport, &[panel]), rect(0.0, 0.0, 1620.0, 1020.0));

        // c) The frame reaching under the panel, overlapping it in BOTH axes — the case the
        //    old rule really did resolve as a top band. Least area picks the right:
        //    380*1020 = 387_600 against the top band's 310*2000 = 620_000.
        let under_panel = rect(1000.0, 100.0, 1700.0, 400.0);
        let usable = usable_viewport_for(under_panel, viewport, &[panel]);
        assert_eq!(usable.min.y, viewport.min.y, "a right-docked panel must never cut a top band");
        assert_eq!(usable.max.y, viewport.max.y, "nor a bottom one");
        assert_eq!(usable, rect(0.0, 0.0, 1620.0, 1020.0));
    }

    #[test]
    fn two_panels_on_the_same_edge_keep_the_largest_cut() {
        let narrow = rect(0.0, 0.0, 100.0, 800.0);
        let wide = rect(0.0, 0.0, 250.0, 800.0);
        assert_eq!(usable_viewport_for(middle_hitbox(), viewport(), &[narrow, wide]), rect(250.0, 0.0, 1000.0, 800.0));
    }

    #[test]
    fn a_panel_that_does_not_touch_the_viewport_cuts_nothing() {
        let away = rect(2000.0, 0.0, 2200.0, 800.0);
        let touching = rect(-200.0, 0.0, 0.0, 800.0);
        assert_eq!(usable_viewport_for(middle_hitbox(), viewport(), &[away, touching]), viewport());
    }

    #[test]
    fn a_panel_that_only_touches_the_hitbox_edge_still_cuts_its_own_side() {
        // The panel's right edge is exactly the hitbox's left edge: zero shared columns, so
        // the rows decide and the cut is the left one, not a top band.
        let panel = rect(0.0, 200.0, 400.0, 600.0);
        assert_eq!(usable_viewport_for(middle_hitbox(), viewport(), &[panel]), rect(400.0, 0.0, 1000.0, 800.0));
    }

    #[test]
    fn a_panel_covering_the_viewport_collapses_the_usable_area() {
        let usable = usable_viewport_for(middle_hitbox(), viewport(), &[viewport()]);
        assert!(!usable.is_positive(), "expected an empty usable area, got {usable:?}");
        assert!(usable.min.x <= usable.max.x && usable.min.y <= usable.max.y, "must not invert");
    }

    #[test]
    fn the_panel_order_does_not_change_the_result() {
        let panels = [
            rect(0.0, 0.0, 200.0, 800.0),
            rect(0.0, 700.0, 1000.0, 800.0),
            rect(0.0, 0.0, 100.0, 800.0),
            rect(800.0, 0.0, 1000.0, 800.0),
            rect(0.0, 0.0, 1000.0, 100.0),
            // Two hitbox-relative cases as well, so the order-independence claim covers the
            // branches that read the hitbox rather than only the full-edge ones.
            rect(380.0, 320.0, 450.0, 400.0),
            rect(0.0, 0.0, 300.0, 200.0),
        ];
        let forward = usable_viewport_for(middle_hitbox(), viewport(), &panels);
        let mut reversed = panels;
        reversed.reverse();
        assert_eq!(forward, usable_viewport_for(middle_hitbox(), viewport(), &reversed));
    }

    #[test]
    fn the_hitbox_relative_cut_and_the_clamp_reach_a_fixed_point() {
        // The cut set depends on where the hitbox is, so a correction on one frame can change
        // the cut set on the next. This drives the real loop — cut the viewport for the
        // current hitbox, clamp, repeat — through exactly that: the frame starts off the left
        // edge inside a left panel's rows, is pushed horizontally out of them, which puts it
        // into a top panel's columns, and is then pushed vertically. It must settle.
        let left_panel = rect(0.0, 0.0, 200.0, 400.0);
        let top_panel = rect(200.0, 0.0, 700.0, 150.0);
        let panels = [left_panel, top_panel];
        let page = boundless_page();
        let mut frame = rect(-60.0, 100.0, 140.0, 300.0);
        let mut settled = None;
        for step in 0..8 {
            let usable = usable_viewport_for(hitbox_rect(frame, &chrome()), viewport(), &panels);
            let delta = keep_in_view_delta(frame, &chrome(), page, usable);
            if delta == Vec2::ZERO {
                settled = Some(step);
                break;
            }
            frame = frame.translate(delta);
        }
        let steps = settled.expect("the cut/clamp loop must reach a fixed point");
        // The lower bound is part of the test: a run that settles immediately would prove
        // nothing about the loop, only that this start position was already legal.
        assert!((1..=4).contains(&steps), "expected a fixed point after 1..=4 iterations, took {steps}");
        // At the fixed point the hitbox is inside the viewport minus whatever still cuts it.
        let usable = usable_viewport_for(hitbox_rect(frame, &chrome()), viewport(), &panels);
        let hitbox = hitbox_rect(frame, &chrome());
        assert!(usable.contains_rect(hitbox), "settled outside its own usable area: {hitbox:?} in {usable:?}");
    }

    // -- keep_in_view_delta -------------------------------------------------------------

    /// A page large enough that the page constraint never binds.
    fn boundless_page() -> Rect {
        rect(-100_000.0, -100_000.0, 100_000.0, 100_000.0)
    }

    #[test]
    fn a_frame_whose_hitbox_is_already_inside_is_not_moved() {
        let delta = keep_in_view_delta(rect(100.0, 100.0, 200.0, 200.0), &chrome(), boundless_page(), viewport());
        assert_eq!(delta, Vec2::ZERO);
    }

    #[test]
    fn a_frame_past_an_edge_is_pushed_back_in_from_that_side() {
        let page = boundless_page();
        // Wider than the chrome minimum, so the hitbox is the frame's own width plus the
        // 8 pt the side handles stick out on each side.
        let left = keep_in_view_delta(rect(-50.0, 100.0, 250.0, 200.0), &chrome(), page, viewport());
        assert_eq!(left, vec2(58.0, 0.0));
        let right = keep_in_view_delta(rect(750.0, 100.0, 1050.0, 200.0), &chrome(), page, viewport());
        assert_eq!(right, vec2(-58.0, 0.0));
        // The handle margin and the top strip are both part of the hitbox, so the frame stops
        // 32 pt below the edge.
        let top = keep_in_view_delta(rect(100.0, 10.0, 200.0, 110.0), &chrome(), page, viewport());
        assert_eq!(top, vec2(0.0, 22.0));
        let bottom = keep_in_view_delta(rect(100.0, 750.0, 200.0, 850.0), &chrome(), page, viewport());
        assert_eq!(bottom, vec2(0.0, -106.0));
    }

    #[test]
    fn chrome_below_the_frame_forces_an_upward_correction() {
        // The frame itself ends at 780, well inside the 800-tall viewport; only the button
        // row and the status line below it stick out.
        let frame = rect(100.0, 700.0, 200.0, 780.0);
        let delta = keep_in_view_delta(frame, &chrome(), boundless_page(), viewport());
        assert_eq!(delta, vec2(0.0, -36.0));
        let moved = hitbox_rect(frame, &chrome()).translate(delta);
        assert!(moved.max.y <= viewport().max.y);
    }

    #[test]
    fn an_axis_that_does_not_fit_the_viewport_aligns_to_its_min_edge() {
        let short_viewport = rect(0.0, 0.0, 1000.0, 100.0);
        let frame = rect(100.0, 200.0, 200.0, 300.0);
        let delta = keep_in_view_delta(frame, &chrome(), boundless_page(), short_viewport);
        let hit = hitbox_rect(frame, &chrome());
        assert_eq!(delta.y, short_viewport.min.y - hit.min.y);
        assert_eq!(hit.translate(delta).min.y, short_viewport.min.y);
    }

    #[test]
    fn the_page_constraint_overrides_the_viewport_constraint() {
        // The page sits above the viewport; pulling the hitbox fully into view would drag
        // the frame off the page, so the move stops at the page's bottom edge.
        let page = rect(0.0, -300.0, 1000.0, -100.0);
        let frame = rect(100.0, -250.0, 200.0, -150.0);
        let delta = keep_in_view_delta(frame, &chrome(), page, viewport());
        assert_eq!(delta, vec2(0.0, 50.0));
        assert_eq!(frame.translate(delta).max.y, page.max.y);
    }

    #[test]
    fn the_widened_chrome_is_what_keep_in_view_holds_inside_the_viewport() {
        // A frame narrower than the chrome minimum: the clamp must work on the WIDENED
        // hitbox, otherwise the rows hang outside the viewport where nothing can reach them.
        let frame = rect(10.0, 100.0, 30.0, 200.0);
        let delta = keep_in_view_delta(frame, &chrome(), boundless_page(), viewport());
        let moved = hitbox_rect(frame, &chrome()).translate(delta);
        assert!(viewport().contains_rect(moved), "hitbox {moved:?} outside {:?}", viewport());
        assert!((moved.width() - 240.0).abs() < 1e-3, "the chrome keeps its minimum width: {moved:?}");
    }

    #[test]
    fn keep_in_view_holds_the_resize_handles_inside_the_viewport() {
        // One frame per border, each hanging over it. The handles are drawn and hit-tested
        // outside the frame, so it is the frame EXPANDED by the handle margin that has to
        // come back in: correcting only the frame itself leaves the handle on that border
        // beyond the viewport, where the pointer can never reach it.
        let page = boundless_page();
        for frame in [
            rect(-30.0, 400.0, 70.0, 500.0),
            rect(960.0, 400.0, 1060.0, 500.0),
            rect(400.0, -30.0, 500.0, 70.0),
            rect(400.0, 760.0, 500.0, 860.0),
        ] {
            let delta = keep_in_view_delta(frame, &chrome(), page, viewport());
            let moved = frame.translate(delta);
            assert!(
                viewport().contains_rect(moved.expand(8.0)),
                "the handles of {moved:?} are outside {:?}",
                viewport()
            );
            let hitbox = hitbox_rect(frame, &chrome()).translate(delta);
            assert!(viewport().contains_rect(hitbox), "hitbox {hitbox:?} outside {:?}", viewport());
        }
    }

    #[test]
    fn a_frame_larger_than_the_page_aligns_to_the_page_min_edge() {
        let page = rect(0.0, 0.0, 50.0, 50.0);
        let frame = rect(10.0, 10.0, 110.0, 110.0);
        let delta = keep_in_view_delta(frame, &chrome(), page, viewport());
        assert_eq!(delta, vec2(-10.0, -10.0));
    }

    // -- choose_page --------------------------------------------------------------------

    fn page_view(idx: usize, visible: Rect) -> PageView {
        PageView { page_idx: idx, visible }
    }

    const FRAME_SIZE: Vec2 = Vec2 { x: 100.0, y: 100.0 };

    #[test]
    fn case_1_a_frame_that_still_fits_the_current_page_stays() {
        let current = page_view(3, rect(0.0, 0.0, 200.0, 200.0));
        let bigger = page_view(4, rect(0.0, 0.0, 900.0, 900.0));
        assert_eq!(choose_page(&current, FRAME_SIZE, &[bigger]), PageChoice::Stay);
    }

    #[test]
    fn case_2a_a_candidate_no_larger_than_the_current_page_does_not_attract_the_frame() {
        let current = page_view(3, rect(0.0, 0.0, 50.0, 300.0));
        let smaller = page_view(4, rect(0.0, 0.0, 100.0, 100.0));
        assert_eq!(choose_page(&current, FRAME_SIZE, &[smaller]), PageChoice::Stay);
    }

    #[test]
    fn case_2b_a_candidate_too_small_for_the_frame_is_refused_while_the_current_page_shows() {
        let current = page_view(3, rect(0.0, 0.0, 50.0, 300.0));
        // Larger in area, but only 80 pt wide against a 100 pt frame.
        let narrow = page_view(4, rect(0.0, 0.0, 80.0, 400.0));
        assert_eq!(choose_page(&current, FRAME_SIZE, &[narrow]), PageChoice::Stay);
    }

    #[test]
    fn case_2b_a_candidate_too_small_for_the_frame_is_taken_once_the_current_page_is_gone() {
        let current = page_view(3, rect(0.0, 0.0, 0.0, 0.0));
        let narrow = page_view(4, rect(0.0, 0.0, 80.0, 400.0));
        assert_eq!(choose_page(&current, FRAME_SIZE, &[narrow]), PageChoice::MoveTo(4));
    }

    #[test]
    fn case_2c_a_larger_candidate_that_holds_the_frame_wins() {
        let current = page_view(3, rect(0.0, 0.0, 50.0, 300.0));
        let roomy = page_view(4, rect(0.0, 0.0, 300.0, 300.0));
        assert_eq!(choose_page(&current, FRAME_SIZE, &[roomy]), PageChoice::MoveTo(4));
    }

    #[test]
    fn case_3_an_empty_current_page_moves_to_the_largest_candidate() {
        // `Rect::intersect` hands back an inverted rect for a page that scrolled away.
        let current = page_view(3, rect(100.0, 100.0, 50.0, 50.0));
        let small = page_view(4, rect(0.0, 0.0, 120.0, 120.0));
        let large = page_view(5, rect(0.0, 0.0, 400.0, 400.0));
        assert_eq!(choose_page(&current, FRAME_SIZE, &[small, large]), PageChoice::MoveTo(5));
    }

    #[test]
    fn an_invisible_candidate_is_no_candidate_at_all() {
        let current = page_view(3, rect(100.0, 100.0, 50.0, 50.0));
        let gone = page_view(4, rect(10.0, 10.0, 0.0, 0.0));
        assert_eq!(choose_page(&current, FRAME_SIZE, &[gone]), PageChoice::Stay);
    }

    #[test]
    fn the_current_page_listed_as_its_own_candidate_is_never_a_move() {
        let current = page_view(3, rect(0.0, 0.0, 50.0, 300.0));
        assert_eq!(choose_page(&current, FRAME_SIZE, &[current]), PageChoice::Stay);
    }

    // -- offscreen_arrow ----------------------------------------------------------------

    #[test]
    fn a_visible_target_needs_no_arrow() {
        assert!(offscreen_arrow(rect(400.0, 400.0, 600.0, 600.0), viewport()).is_none());
        // Partly visible is still visible.
        assert!(offscreen_arrow(rect(-50.0, 400.0, 50.0, 600.0), viewport()).is_none());
    }

    #[test]
    fn an_arrow_points_from_the_viewport_centre_towards_the_target() {
        let view = rect(0.0, 0.0, 1000.0, 1000.0);
        let cases = [
            (rect(450.0, -300.0, 550.0, -200.0), vec2(0.0, -1.0), pos2(500.0, ARROW_INSET)),
            (rect(450.0, 1200.0, 550.0, 1300.0), vec2(0.0, 1.0), pos2(500.0, 1000.0 - ARROW_INSET)),
            (rect(-300.0, 450.0, -200.0, 550.0), vec2(-1.0, 0.0), pos2(ARROW_INSET, 500.0)),
            (rect(1200.0, 450.0, 1300.0, 550.0), vec2(1.0, 0.0), pos2(1000.0 - ARROW_INSET, 500.0)),
        ];
        for (target, dir, tip) in cases {
            let arrow = offscreen_arrow(target, view).expect("target is off-screen");
            assert_eq!(arrow.dir, dir, "target {target:?}");
            assert!((arrow.tip - tip).length() < 1e-3, "tip {:?} != {tip:?}", arrow.tip);
        }
    }

    #[test]
    fn a_diagonal_target_puts_the_tip_on_the_inset_corner() {
        let view = rect(0.0, 0.0, 1000.0, 1000.0);
        let arrow = offscreen_arrow(rect(1500.0, 1500.0, 1600.0, 1600.0), view).expect("off-screen");
        let diagonal = std::f32::consts::FRAC_1_SQRT_2;
        assert!((arrow.dir.x - diagonal).abs() < 1e-4 && (arrow.dir.y - diagonal).abs() < 1e-4);
        let expected = pos2(1000.0 - ARROW_INSET, 1000.0 - ARROW_INSET);
        assert!((arrow.tip - expected).length() < 1e-3, "tip {:?} != {expected:?}", arrow.tip);
    }

    #[test]
    fn the_arrow_tip_never_leaves_a_viewport_smaller_than_the_inset() {
        let tiny = rect(0.0, 0.0, 10.0, 10.0);
        let arrow = offscreen_arrow(rect(500.0, 500.0, 600.0, 600.0), tiny).expect("off-screen");
        assert!(tiny.contains(arrow.tip), "tip {:?} left {tiny:?}", arrow.tip);
    }
}
