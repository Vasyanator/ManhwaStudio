/*
File: tabs/page_manager/crop_layout.rs

Purpose:
GUI-free core of the "crop page" feature: the crop rectangle and its
invariants, the 8 resize handles plus the move region and where they sit on
screen, how a pointer DRAG moves the frame without letting it invert or leave
the canvas, aspect-ratio locking, the centred fit helpers, and the rotation
arithmetic (quarter turns + a fine straightening angle) the window needs.
Contains no egui code and performs no I/O, so every rule here is unit-testable.

Key structures:
- CropFrame: the crop rect in ROTATED-CANVAS pixels, valid by construction.
- CropHandle: the 8 edge/corner handles plus the Move region.
- ScreenRect: an axis-aligned rect in board/screen POINTS (egui-free).
- AspectRatio: free, or a locked `w:h`.
- CropLayoutError: why a crop request is not a legal engine request.

Key functions:
- handle_rects() / move_rect() / hit_test(): the grab geometry and its priority.
- apply_drag() / apply_drag_with_ratio(): the pointer drag, clamped per edge.
- largest_centred_frame() / CropFrame::full(): the fit helpers.
- normalize_rotation() / clamp_fine_angle(): the rotation arithmetic.
- validate(): the engine's own preconditions, checked before the confirm button.

Notes:
The coordinate model, fixed for the whole feature and shared with the engine:
a source page of `W x H` px is first turned by `quarter_turns` CLOCKWISE 90°
steps, then straightened by `angle_deg` (clockwise-positive, strictly inside
±45°) about the centre of the quarter-turned page. The ROTATED CANVAS is the
axis-aligned bounding box of that rotated page, with the page centred in it,
and the crop rect lives in the canvas's own pixel space.

This module deliberately does NOT compute the canvas size: the bounding-box
formula belongs to the engine (`page_ops::RotatedPage`), and every function here
takes the canvas as a plain `canvas: [u32; 2]` parameter instead. The WINDOW
must call the engine's helper and pass the result down — a second copy of the
formula here would let the dialog and the engine disagree about which crops are
legal, which is exactly the failure `stitch_layout`'s imported bounds exist to
prevent. `MAX_FINE_ANGLE_DEG` is imported from the engine for the same reason.

Two unit systems meet here and are never mixed: canvas PIXELS (`u32` state,
`i64`/`f64` while a drag is being resolved) and screen POINTS (`f32`, only in
`ScreenRect` and the handle geometry). Handles keep a fixed size in points at
every zoom, so their pixel footprint is not a constant and cannot be baked into
the frame.
*/

/// Largest fine straightening angle, exclusive: a request must satisfy
/// `-45 < angle_deg < 45`. Anything beyond is expressed as another quarter turn.
///
/// The bound is the ENGINE's, taken from `page_ops` rather than restated (the
/// same rule `stitch_layout`'s canvas bounds follow): an angle this module
/// accepts must be one the engine's own validation accepts too.
pub(super) const MAX_FINE_ANGLE_DEG: f64 = crate::page_ops::MAX_FINE_ANGLE_DEG;

/// How far inside [`MAX_FINE_ANGLE_DEG`] a clamped angle is placed.
///
/// The valid range is OPEN, so an exact ±45° cannot be represented and has to be
/// nudged toward zero. 1e-6° is six orders of magnitude below the 0.01° step the
/// window's spin box offers, so the nudge is invisible to the user while keeping
/// [`validate`] satisfied.
pub(super) const ANGLE_EPSILON_DEG: f64 = 1e-6;

/// Smallest crop side the window offers, in canvas pixels.
///
/// Passed as `min_size` to [`apply_drag`]; it is an interaction floor, not an
/// engine precondition, which is why [`validate`] does not check it.
pub(super) const MIN_FRAME_SIDE_PX: u32 = 8;

/// How much longer than wide an EDGE handle's grab rect is, along its edge.
/// Corners stay square, so a corner is never swallowed by its neighbours.
const EDGE_HANDLE_LENGTH_FACTOR: f32 = 1.4;

/// Inset of the move region from the frame's border, in handle sizes. Keeps the
/// move region clear of the edge handles' grab rects.
const MOVE_REGION_INSET_FACTOR: f32 = 0.75;

/// Slack allowed when checking an inscribed-rect candidate against the rotated
/// page, in pixels. It absorbs the candidate's own floating-point rounding; the
/// FLOOR applied to the realised side keeps the rect inside the page regardless.
const INSCRIBED_SLACK_PX: f64 = 1e-9;

/// Bound applied before every float→integer pixel conversion. 2^32 covers the
/// whole `u32` coordinate range with room to spare and is exactly representable
/// in `f64`, so the conversion below it is lossless.
const PX_LIMIT: f64 = 4_294_967_296.0;

/// Why a crop request is not a legal engine request.
///
/// Every variant is a refusal, never a silently corrected value: the window
/// turns it into a disabled confirm button plus a localized message, and the
/// engine would reject the same request with its own validation.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub(super) enum CropLayoutError {
    /// The rotated canvas has a zero side, so it holds no pixel to crop.
    #[error("the rotated canvas is {width}x{height} px and holds no pixels")]
    CanvasEmpty { width: u32, height: u32 },
    /// The crop rect has a zero side, which would produce an empty page.
    #[error("the crop rect is {width}x{height} px and would produce an empty page")]
    FrameEmpty { width: u32, height: u32 },
    /// The crop rect reaches outside the rotated canvas.
    #[error("the crop rect {rect:?} does not fit inside the {canvas:?} px canvas")]
    FrameOutsideCanvas { rect: [u32; 4], canvas: [u32; 2] },
    /// `quarter_turns` is not one of the four 90° steps.
    #[error("{quarter_turns} quarter turns requested, only 0..=3 exist")]
    QuarterTurnsOutOfRange { quarter_turns: u8 },
    /// The fine angle is not finite or not strictly inside ±45°.
    #[error("the straightening angle {angle_deg}° is not strictly inside ±{MAX_FINE_ANGLE_DEG}°")]
    AngleOutOfRange { angle_deg: f64 },
}

/// The crop rectangle, in ROTATED-CANVAS pixels.
///
/// Valid by construction and kept valid by every function in this module:
/// `width >= 1`, `height >= 1`, and `x + width <= canvas[0]`,
/// `y + height <= canvas[1]` for the canvas it was built against. The frame
/// carries no canvas of its own — the window owns the canvas size and passes it
/// to every operation, because a rotation change resizes the canvas underneath
/// an existing frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CropFrame {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl CropFrame {
    /// Builds a frame from an engine-shaped `[x, y, w, h]` rect, checking the
    /// invariants against `canvas`.
    ///
    /// # Errors
    /// [`CropLayoutError::CanvasEmpty`] for a zero-sided canvas,
    /// [`CropLayoutError::FrameEmpty`] for a zero-sided rect, and
    /// [`CropLayoutError::FrameOutsideCanvas`] when the rect reaches outside it.
    pub(super) fn new(canvas: [u32; 2], rect: [u32; 4]) -> Result<Self, CropLayoutError> {
        validate_rect(canvas, rect)?;
        Ok(Self {
            x: rect[0],
            y: rect[1],
            w: rect[2],
            h: rect[3],
        })
    }

    /// The default frame of a freshly opened window: the whole canvas.
    ///
    /// # Errors
    /// [`CropLayoutError::CanvasEmpty`] when `canvas` has a zero side.
    pub(super) fn full(canvas: [u32; 2]) -> Result<Self, CropLayoutError> {
        Self::new(canvas, [0, 0, canvas[0], canvas[1]])
    }

    /// The engine-shaped `[x, y, w, h]` rect this frame stands for.
    #[must_use]
    pub(super) const fn rect(self) -> [u32; 4] {
        [self.x, self.y, self.w, self.h]
    }

    /// X of the frame's first pixel column, in canvas px.
    #[must_use]
    pub(super) const fn x(self) -> u32 {
        self.x
    }

    /// Y of the frame's first pixel row, in canvas px.
    #[must_use]
    pub(super) const fn y(self) -> u32 {
        self.y
    }

    /// Frame width in canvas px; always `>= 1`.
    #[must_use]
    pub(super) const fn width(self) -> u32 {
        self.w
    }

    /// Frame height in canvas px; always `>= 1`.
    #[must_use]
    pub(super) const fn height(self) -> u32 {
        self.h
    }

    /// X just past the frame's last pixel column (exclusive), in canvas px.
    #[must_use]
    pub(super) const fn right(self) -> u32 {
        self.x + self.w
    }

    /// Y just past the frame's last pixel row (exclusive), in canvas px.
    #[must_use]
    pub(super) const fn bottom(self) -> u32 {
        self.y + self.h
    }
}

/// One grab region of the crop frame: the 8 resize handles plus the interior
/// that translates the whole frame.
///
/// Exhaustive on purpose: a new grab region must force every decision site
/// ([`CropHandle::edges`], the priority table, the drag) to be reconsidered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CropHandle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    /// The frame's interior: a drag here translates the frame, keeping its size.
    Move,
}

/// Hit-test order, and the order [`handle_rects`] returns its pairs in: corners
/// beat edges, and [`CropHandle::Move`] (tested last, separately) beats nothing.
/// A corner's grab rect overlaps both adjacent edge rects, so without this order
/// a corner would be unreachable on a small frame.
const HANDLE_PRIORITY: [CropHandle; 8] = [
    CropHandle::TopLeft,
    CropHandle::TopRight,
    CropHandle::BottomRight,
    CropHandle::BottomLeft,
    CropHandle::Top,
    CropHandle::Right,
    CropHandle::Bottom,
    CropHandle::Left,
];

impl CropHandle {
    /// Which of the frame's four edges this handle moves, as
    /// `(left, top, right, bottom)`.
    ///
    /// [`CropHandle::Move`] moves none of them — it translates the frame — which
    /// is what makes the drag code able to treat "no edge moves" as "translate".
    #[must_use]
    pub(super) const fn edges(self) -> (bool, bool, bool, bool) {
        match self {
            Self::TopLeft => (true, true, false, false),
            Self::Top => (false, true, false, false),
            Self::TopRight => (false, true, true, false),
            Self::Right => (false, false, true, false),
            Self::BottomRight => (false, false, true, true),
            Self::Bottom => (false, false, false, true),
            Self::BottomLeft => (true, false, false, true),
            Self::Left => (true, false, false, false),
            Self::Move => (false, false, false, false),
        }
    }

    /// Whether this handle moves one horizontal AND one vertical edge.
    #[must_use]
    pub(super) const fn is_corner(self) -> bool {
        let (left, top, right, bottom) = self.edges();
        (left || right) && (top || bottom)
    }

    /// Whether this handle translates the frame instead of resizing it.
    #[must_use]
    pub(super) const fn is_move(self) -> bool {
        matches!(self, Self::Move)
    }
}

/// An axis-aligned rectangle in board/screen POINTS.
///
/// Deliberately egui-free so this module stays testable without a GUI context;
/// the window converts to `egui::Rect` at the call site, which is a one-liner in
/// both directions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ScreenRect {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

impl ScreenRect {
    /// A rect from its two corners, with the coordinates ordered if they arrive
    /// swapped, so a rect built from a drag is never negative-sized.
    #[must_use]
    pub(super) fn from_min_max(min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> Self {
        Self {
            min_x: min_x.min(max_x),
            min_y: min_y.min(max_y),
            max_x: min_x.max(max_x),
            max_y: min_y.max(max_y),
        }
    }

    /// A rect of `width` x `height` points centred on `(center_x, center_y)`.
    #[must_use]
    pub(super) fn from_center_size(center_x: f32, center_y: f32, width: f32, height: f32) -> Self {
        let half_w = width.abs() * 0.5;
        let half_h = height.abs() * 0.5;
        Self {
            min_x: center_x - half_w,
            min_y: center_y - half_h,
            max_x: center_x + half_w,
            max_y: center_y + half_h,
        }
    }

    /// Width in points; never negative.
    #[must_use]
    pub(super) fn width(self) -> f32 {
        self.max_x - self.min_x
    }

    /// Height in points; never negative.
    #[must_use]
    pub(super) fn height(self) -> f32 {
        self.max_y - self.min_y
    }

    /// Centre point, as `(x, y)`.
    #[must_use]
    pub(super) fn center(self) -> (f32, f32) {
        (
            (self.min_x + self.max_x) * 0.5,
            (self.min_y + self.max_y) * 0.5,
        )
    }

    /// Whether both sides are strictly positive, i.e. the rect can be hit at all.
    #[must_use]
    pub(super) fn is_positive(self) -> bool {
        self.width() > 0.0 && self.height() > 0.0
    }

    /// Whether `(x, y)` lies inside the rect, borders included.
    #[must_use]
    pub(super) fn contains(self, x: f32, y: f32) -> bool {
        (self.min_x..=self.max_x).contains(&x) && (self.min_y..=self.max_y).contains(&y)
    }

    /// The rect inset by `amount` points on every side, or `None` when that
    /// would collapse it.
    #[must_use]
    pub(super) fn shrink(self, amount: f32) -> Option<Self> {
        let shrunk = Self {
            min_x: self.min_x + amount,
            min_y: self.min_y + amount,
            max_x: self.max_x - amount,
            max_y: self.max_y - amount,
        };
        shrunk.is_positive().then_some(shrunk)
    }
}

/// Grab rects of the 8 resize handles, in the same board/screen space as
/// `frame_screen`, ordered by [`HANDLE_PRIORITY`].
///
/// `handle_size_pt` is a size in POINTS and is NOT scaled by the board's zoom:
/// the handles must stay equally grabbable at every zoom, exactly as the split
/// window's cut handles do. Corner rects are `handle_size_pt` squares centred on
/// the corners; edge rects are centred on the edge midpoints and stretched
/// [`EDGE_HANDLE_LENGTH_FACTOR`] along their edge.
///
/// The rects OVERLAP on a small frame — a corner rect always covers part of its
/// two neighbouring edge rects — which is what [`HANDLE_PRIORITY`] and
/// [`hit_test`] resolve. A window that turns them into separate interaction
/// rects must reproduce that priority itself, whichever way its framework
/// resolves overlapping widgets.
#[must_use]
pub(super) fn handle_rects(
    frame_screen: ScreenRect,
    handle_size_pt: f32,
) -> [(CropHandle, ScreenRect); 8] {
    let size = handle_size_pt.abs().max(f32::EPSILON);
    let long = size * EDGE_HANDLE_LENGTH_FACTOR;
    let (center_x, center_y) = frame_screen.center();
    HANDLE_PRIORITY.map(|handle| {
        let rect = match handle {
            CropHandle::TopLeft => {
                ScreenRect::from_center_size(frame_screen.min_x, frame_screen.min_y, size, size)
            }
            CropHandle::TopRight => {
                ScreenRect::from_center_size(frame_screen.max_x, frame_screen.min_y, size, size)
            }
            CropHandle::BottomRight => {
                ScreenRect::from_center_size(frame_screen.max_x, frame_screen.max_y, size, size)
            }
            CropHandle::BottomLeft => {
                ScreenRect::from_center_size(frame_screen.min_x, frame_screen.max_y, size, size)
            }
            CropHandle::Top => {
                ScreenRect::from_center_size(center_x, frame_screen.min_y, long, size)
            }
            CropHandle::Right => {
                ScreenRect::from_center_size(frame_screen.max_x, center_y, size, long)
            }
            CropHandle::Bottom => {
                ScreenRect::from_center_size(center_x, frame_screen.max_y, long, size)
            }
            CropHandle::Left => {
                ScreenRect::from_center_size(frame_screen.min_x, center_y, size, long)
            }
            // Not a resize handle: it has no border rect, only the interior one
            // `move_rect` returns. `HANDLE_PRIORITY` never contains it.
            CropHandle::Move => frame_screen,
        };
        (handle, rect)
    })
}

/// Grab rect of the move region: the frame's interior, inset far enough to clear
/// the edge handles, or `None` when the frame is too small on screen to hold one.
///
/// A `None` here is not a defect: on a frame only a few points wide every pixel
/// belongs to a resize handle, and the user zooms in to move it.
#[must_use]
pub(super) fn move_rect(frame_screen: ScreenRect, handle_size_pt: f32) -> Option<ScreenRect> {
    frame_screen.shrink(handle_size_pt.abs() * MOVE_REGION_INSET_FACTOR)
}

/// Resolves a pointer position in board/screen points to the region it grabs.
///
/// Priority: corners, then edges, then the move region — a corner rect overlaps
/// its two neighbouring edge rects, so the reverse order would make corners
/// unreachable. Returns `None` when the pointer is outside every grab rect.
///
/// Tests exactly the rects [`handle_rects`] and [`move_rect`] return, so a
/// window that draws and interacts those rects can never disagree with it.
#[must_use]
pub(super) fn hit_test(
    frame_screen: ScreenRect,
    handle_size_pt: f32,
    pointer_x: f32,
    pointer_y: f32,
) -> Option<CropHandle> {
    for (handle, rect) in handle_rects(frame_screen, handle_size_pt) {
        if rect.contains(pointer_x, pointer_y) {
            return Some(handle);
        }
    }
    move_rect(frame_screen, handle_size_pt)
        .filter(|rect| rect.contains(pointer_x, pointer_y))
        .map(|_| CropHandle::Move)
}

/// The aspect ratio a drag must preserve.
///
/// The window builds [`AspectRatio::Locked`] from one of its presets: the
/// quarter-turned page's own `w:h` (taken from the ENGINE's canvas at a zero fine
/// angle) or `1:1`. A `Locked` with a zero side is degenerate and is treated as
/// [`AspectRatio::Free`] everywhere in this module; construct through
/// [`AspectRatio::locked`] to avoid making one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AspectRatio {
    /// Width and height move independently.
    Free,
    /// The frame keeps `w:h` while it is resized.
    Locked { w: u32, h: u32 },
}

impl AspectRatio {
    /// A locked `w:h` ratio, or `None` when either side is zero.
    #[must_use]
    pub(super) const fn locked(w: u32, h: u32) -> Option<Self> {
        if w == 0 || h == 0 {
            None
        } else {
            Some(Self::Locked { w, h })
        }
    }

    /// The ratio as `width / height`, or `None` when it is free or degenerate.
    #[must_use]
    pub(super) fn value(self) -> Option<f64> {
        match self {
            Self::Free => None,
            Self::Locked { w, h } => {
                if w == 0 || h == 0 {
                    None
                } else {
                    Some(f64::from(w) / f64::from(h))
                }
            }
        }
    }
}

/// Applies a pointer drag to the crop frame, with no ratio constraint.
///
/// `frame` is the frame as it was when the drag STARTED and `delta` is the
/// pointer's total movement since then, in ROTATED-CANVAS pixels — never its
/// absolute position. At a ribbon's fit zoom one screen point is tens of source
/// pixels, so snapping the frame to the pointer would throw the grab offset away
/// as a jump of hundreds of pixels; resolving from the start state also keeps a
/// long drag free of accumulation drift.
///
/// `min_size` is the smallest side the drag may leave, itself capped by the
/// canvas. Every edge is clamped independently: the frame stays inside `canvas`,
/// never inverts, and never falls below the minimum — dragging an edge past its
/// opposite one stops it there instead of flipping the frame.
/// [`CropHandle::Move`] keeps the size and clamps the position.
///
/// Total: any `delta` (NaN included, read as zero) and any `canvas` yield a valid
/// frame; it never panics.
#[must_use]
pub(super) fn apply_drag(
    frame: CropFrame,
    handle: CropHandle,
    delta: [f64; 2],
    canvas: [u32; 2],
    min_size: u32,
) -> CropFrame {
    let canvas_w = i64::from(canvas[0].max(1));
    let canvas_h = i64::from(canvas[1].max(1));
    // A minimum larger than the canvas would make every clamp inconsistent, so
    // the canvas wins over the interaction floor.
    let min = i64::from(min_size.max(1)).min(canvas_w).min(canvas_h);
    let dx = round_to_px(delta[0]);
    let dy = round_to_px(delta[1]);

    let mut left = i64::from(frame.x);
    let mut top = i64::from(frame.y);
    let mut right = i64::from(frame.right());
    let mut bottom = i64::from(frame.bottom());

    let (moves_left, moves_top, moves_right, moves_bottom) = handle.edges();
    if handle.is_move() {
        let width = right - left;
        let height = bottom - top;
        left = (left + dx).clamp(0, (canvas_w - width).max(0));
        top = (top + dy).clamp(0, (canvas_h - height).max(0));
        right = left + width;
        bottom = top + height;
    } else {
        // Each edge is clamped against the OPPOSITE one, which cannot move in the
        // same drag, so the two clamps never fight and the frame cannot invert.
        if moves_left {
            left = (left + dx).clamp(0, (right - min).max(0));
        }
        if moves_right {
            right = (right + dx).clamp((left + min).min(canvas_w), canvas_w);
        }
        if moves_top {
            top = (top + dy).clamp(0, (bottom - min).max(0));
        }
        if moves_bottom {
            bottom = (bottom + dy).clamp((top + min).min(canvas_h), canvas_h);
        }
    }
    frame_from_edges(canvas, left, top, right, bottom, min)
}

/// Applies a pointer drag while keeping `ratio`.
///
/// Delegates to [`apply_drag`] for [`AspectRatio::Free`], for a degenerate lock,
/// and for [`CropHandle::Move`] (a translation preserves any ratio).
///
/// Which dimension leads:
/// * **Corner** — neither: the frame grows to COVER the pointer. The width
///   implied by the horizontal movement and the width implied by the vertical
///   movement are both computed, and the larger wins. The two coincide exactly at
///   the crossover, so the frame never jumps when the pointer changes direction,
///   and a purely vertical corner drag still resizes. The opposite corner is the
///   anchor.
/// * **Left / Right edge** — the WIDTH leads; the height follows and grows
///   symmetrically about the frame's (fixed) vertical centre.
/// * **Top / Bottom edge** — the HEIGHT leads; the width follows and grows
///   symmetrically about the frame's (fixed) horizontal centre.
///
/// The frame stays inside `canvas` and above `min_size` on BOTH sides. When the
/// locked ratio admits no size that satisfies both from the current anchor, the
/// frame is returned UNCHANGED — a refusal, never a distorted frame. Rounding to
/// whole pixels can leave the realised ratio off by less than one pixel per side;
/// that is inherent to an integer crop rect and is not corrected by drifting the
/// anchor.
#[must_use]
pub(super) fn apply_drag_with_ratio(
    frame: CropFrame,
    handle: CropHandle,
    delta: [f64; 2],
    canvas: [u32; 2],
    min_size: u32,
    ratio: AspectRatio,
) -> CropFrame {
    let Some(r) = ratio.value() else {
        return apply_drag(frame, handle, delta, canvas, min_size);
    };
    if handle.is_move() {
        return apply_drag(frame, handle, delta, canvas, min_size);
    }
    let canvas_w = f64::from(canvas[0].max(1));
    let canvas_h = f64::from(canvas[1].max(1));
    let canvas_w_px = i64::from(canvas[0].max(1));
    let canvas_h_px = i64::from(canvas[1].max(1));
    let min = f64::from(min_size.max(1)).min(canvas_w).min(canvas_h);
    let min_px = round_to_px(min).max(1);

    let left = f64::from(frame.x);
    let top = f64::from(frame.y);
    let width = f64::from(frame.w);
    let height = f64::from(frame.h);
    let right = left + width;
    let bottom = top + height;
    let dx = if delta[0].is_finite() { delta[0] } else { 0.0 };
    let dy = if delta[1].is_finite() { delta[1] } else { 0.0 };

    let (moves_left, moves_top, moves_right, moves_bottom) = handle.edges();
    // `h = w / r`, so the height minimum becomes a second width minimum.
    let width_min = min.max(min * r);

    if handle.is_corner() {
        let sign_x = if moves_right { 1.0 } else { -1.0 };
        let sign_y = if moves_bottom { 1.0 } else { -1.0 };
        let available_w = if moves_right { canvas_w - left } else { right };
        let available_h = if moves_bottom { canvas_h - top } else { bottom };
        let width_max = available_w.min(available_h * r);
        if width_min > width_max {
            return frame;
        }
        let target = (width + sign_x * dx).max((height + sign_y * dy) * r);
        let new_width = target.clamp(width_min, width_max);
        let new_height = new_width / r;
        let width_px = round_to_px(new_width).clamp(min_px, canvas_w_px);
        let height_px = round_to_px(new_height).clamp(min_px, canvas_h_px);
        // The anchor is the opposite corner: the moving edges are the only ones
        // that change, which is what keeps a corner drag feeling pinned.
        let new_left = if moves_right {
            round_to_px(left)
        } else {
            round_to_px(right) - width_px
        };
        let new_top = if moves_bottom {
            round_to_px(top)
        } else {
            round_to_px(bottom) - height_px
        };
        return frame_from_edges(
            canvas,
            new_left,
            new_top,
            new_left + width_px,
            new_top + height_px,
            min_px,
        );
    }

    if moves_left || moves_right {
        // Horizontal edge: the width leads, the height is centred on the frame's
        // own vertical centre so the growth is symmetric.
        let sign_x = if moves_right { 1.0 } else { -1.0 };
        let available_w = if moves_right { canvas_w - left } else { right };
        let center_y = top + height * 0.5;
        let available_h = 2.0 * center_y.min(canvas_h - center_y);
        let width_max = available_w.min(available_h * r);
        if width_min > width_max {
            return frame;
        }
        let new_width = (width + sign_x * dx).clamp(width_min, width_max);
        let new_height = new_width / r;
        let width_px = round_to_px(new_width).clamp(min_px, canvas_w_px);
        let height_px = round_to_px(new_height).clamp(min_px, canvas_h_px);
        let new_left = if moves_right {
            round_to_px(left)
        } else {
            round_to_px(right) - width_px
        };
        let new_top = round_to_px(center_y - new_height * 0.5).clamp(0, canvas_h_px - height_px);
        return frame_from_edges(
            canvas,
            new_left,
            new_top,
            new_left + width_px,
            new_top + height_px,
            min_px,
        );
    }

    if moves_top || moves_bottom {
        // Vertical edge: mirror image of the branch above, height leading.
        let sign_y = if moves_bottom { 1.0 } else { -1.0 };
        let available_h = if moves_bottom { canvas_h - top } else { bottom };
        let center_x = left + width * 0.5;
        let available_w = 2.0 * center_x.min(canvas_w - center_x);
        let height_min = min.max(min / r);
        let height_max = available_h.min(available_w / r);
        if height_min > height_max {
            return frame;
        }
        let new_height = (height + sign_y * dy).clamp(height_min, height_max);
        let new_width = new_height * r;
        let width_px = round_to_px(new_width).clamp(min_px, canvas_w_px);
        let height_px = round_to_px(new_height).clamp(min_px, canvas_h_px);
        let new_top = if moves_bottom {
            round_to_px(top)
        } else {
            round_to_px(bottom) - height_px
        };
        let new_left = round_to_px(center_x - new_width * 0.5).clamp(0, canvas_w_px - width_px);
        return frame_from_edges(
            canvas,
            new_left,
            new_top,
            new_left + width_px,
            new_top + height_px,
            min_px,
        );
    }

    // No edge moves and it is not `Move`: unreachable for the current enum, and a
    // no-op is the only answer that cannot corrupt the frame if a variant is added.
    frame
}

/// The largest frame with `ratio`, centred in `canvas`.
///
/// Returns the whole canvas for [`AspectRatio::Free`] and for a degenerate lock.
/// The realised ratio can be off by less than one pixel per side, because the
/// frame is integral; the fit never exceeds the canvas.
///
/// # Errors
/// [`CropLayoutError::CanvasEmpty`] when `canvas` has a zero side.
pub(super) fn largest_centred_frame(
    canvas: [u32; 2],
    ratio: AspectRatio,
) -> Result<CropFrame, CropLayoutError> {
    let Some(r) = ratio.value() else {
        return CropFrame::full(canvas);
    };
    if canvas[0] == 0 || canvas[1] == 0 {
        return Err(CropLayoutError::CanvasEmpty {
            width: canvas[0],
            height: canvas[1],
        });
    }
    let canvas_w = f64::from(canvas[0]);
    let canvas_h = f64::from(canvas[1]);
    // Floor, not round: a rounded-up side would reach outside the canvas.
    let width = canvas_w.min(canvas_h * r).floor();
    let width_px = round_to_px(width).clamp(1, i64::from(canvas[0]));
    let height_px = round_to_px(width / r).clamp(1, i64::from(canvas[1]));
    let x = (i64::from(canvas[0]) - width_px) / 2;
    let y = (i64::from(canvas[1]) - height_px) / 2;
    Ok(frame_from_edges(
        canvas,
        x,
        y,
        x + width_px,
        y + height_px,
        1,
    ))
}

/// Moves `frame` from `old_canvas` into `new_canvas`, keeping it over the same
/// PAGE content, then clamps it into the new canvas.
///
/// A change of the FINE straightening angle resizes the rotated canvas around a
/// page that stays CENTRED in it, so every page pixel moves by exactly half the
/// canvas-size change. Translating the frame by that half-delta is therefore the
/// transform that keeps the user's framing on the content they framed; without
/// it a single small straightening step would slide the frame across the page,
/// and rebuilding the frame instead would discard the framing outright.
///
/// The half-delta is truncated toward zero — at most half a pixel per axis, far
/// below what the board can show — and the result is clamped into `new_canvas`
/// with sides of at least `min_size`: a frame that no longer fits is SHRUNK,
/// never allowed to leave the canvas.
///
/// Total: any canvases and any frame yield a valid frame; it never panics.
#[must_use]
pub(super) fn recentre_frame(
    frame: CropFrame,
    old_canvas: [u32; 2],
    new_canvas: [u32; 2],
    min_size: u32,
) -> CropFrame {
    let dx = (i64::from(new_canvas[0]) - i64::from(old_canvas[0])) / 2;
    let dy = (i64::from(new_canvas[1]) - i64::from(old_canvas[1])) / 2;
    frame_from_edges(
        new_canvas,
        i64::from(frame.x) + dx,
        i64::from(frame.y) + dy,
        i64::from(frame.right()) + dx,
        i64::from(frame.bottom()) + dy,
        i64::from(min_size.max(1)),
    )
}

/// The largest frame with `ratio` that fits INSIDE the rotated page, centred in
/// `canvas`.
///
/// This is the "no empty corners" fit a straightening tool owes the user: a
/// frame spanning the whole rotated canvas always contains the transparent
/// wedges the rotation leaves, and only this rectangle is guaranteed to contain
/// page pixels everywhere.
///
/// `turned_page` is the page AFTER its quarter turns and BEFORE the fine angle
/// (the engine's zero-angle canvas); `angle_deg` is that fine angle. The rotated
/// page is centred in `canvas` by construction, so the answer is centred there
/// too.
///
/// Geometry: a centred axis-aligned rect of half-size `(u, v)` lies inside a
/// page of half-size `(A, B)` rotated by θ exactly when its extreme corner stays
/// inside the page in the PAGE's own frame, i.e. when
/// `u·c + v·s <= A` and `u·s + v·c <= B`, with `c = |cos θ|`, `s = |sin θ|`.
/// A locked ratio makes `v = u / r`, turning both into upper bounds on `u`; a
/// free ratio maximises the area `u·v` over that region (see
/// [`free_inscribed_half_size`]).
///
/// At `angle_deg == 0` the constraints degenerate to `u <= A`, `v <= B`: the
/// whole page, which at a zero angle IS the whole canvas. Sides are FLOORED, so
/// the result never pokes outside the rotated page.
///
/// # Errors
/// [`CropLayoutError::CanvasEmpty`] for a zero-sided `canvas` or a zero-sided
/// `turned_page` (the error carries the offending pair), and
/// [`CropLayoutError::AngleOutOfRange`] for an angle that is not finite or not
/// strictly inside ±45°.
pub(super) fn largest_inscribed_frame(
    canvas: [u32; 2],
    turned_page: [u32; 2],
    angle_deg: f64,
    ratio: AspectRatio,
) -> Result<CropFrame, CropLayoutError> {
    if canvas[0] == 0 || canvas[1] == 0 {
        return Err(CropLayoutError::CanvasEmpty {
            width: canvas[0],
            height: canvas[1],
        });
    }
    if turned_page[0] == 0 || turned_page[1] == 0 {
        return Err(CropLayoutError::CanvasEmpty {
            width: turned_page[0],
            height: turned_page[1],
        });
    }
    if !angle_deg.is_finite() || angle_deg.abs() >= MAX_FINE_ANGLE_DEG {
        return Err(CropLayoutError::AngleOutOfRange { angle_deg });
    }
    let (sin, cos) = angle_deg.to_radians().sin_cos();
    let (s, c) = (sin.abs(), cos.abs());
    let half_w = f64::from(turned_page[0]) / 2.0;
    let half_h = f64::from(turned_page[1]) / 2.0;

    let (half_width, half_height) = match ratio.value() {
        // `v = u / r` collapses both constraints into bounds on `u`; the tighter
        // one wins, which is exact and needs no search.
        Some(r) => {
            let u = (half_w / (c + s / r)).min(half_h / (s + c / r));
            (u, u / r)
        }
        None => free_inscribed_half_size(half_w, half_h, s, c),
    };
    // Floor, not round: a rounded-up side would poke outside the rotated page,
    // which is the one thing this fit exists to prevent.
    let width = round_to_px((2.0 * half_width).floor()).clamp(1, i64::from(canvas[0]));
    let height = round_to_px((2.0 * half_height).floor()).clamp(1, i64::from(canvas[1]));
    let x = (i64::from(canvas[0]) - width) / 2;
    let y = (i64::from(canvas[1]) - height) / 2;
    Ok(frame_from_edges(canvas, x, y, x + width, y + height, 1))
}

/// Half-size of the maximum-AREA axis-aligned rect inscribed in a page of
/// half-size `(half_w, half_h)` rotated by an angle whose `|sin|` and `|cos|`
/// are `s` and `c`.
///
/// Maximises `u·v` under `u·c + v·s <= half_w` and `u·s + v·c <= half_h`. The
/// maximiser of a product over that region is one of three points — where both
/// constraints are tight, or where one alone is tight at its own midpoint — so
/// the search is a comparison of three candidates rather than an optimisation.
/// A fourth candidate, the largest rect SIMILAR to the page, is always feasible
/// and is what keeps the answer non-degenerate when the first three are not.
///
/// `c > s >= 0` holds for every angle strictly inside ±45°, so `c² - s²` is
/// never zero and the midpoints are well defined whenever `s > 0`.
#[must_use]
fn free_inscribed_half_size(half_w: f64, half_h: f64, s: f64, c: f64) -> (f64, f64) {
    if s == 0.0 {
        return (half_w, half_h);
    }
    let det = c * c - s * s;
    // The similar-rect candidate: `t` is the largest scale at which a page-shaped
    // rect still satisfies both constraints, so it is feasible by construction.
    let t = (half_w / (half_w * c + half_h * s)).min(half_h / (half_w * s + half_h * c));
    let candidates = [
        (
            (half_w * c - half_h * s) / det,
            (half_h * c - half_w * s) / det,
        ),
        (half_w / (2.0 * c), half_w / (2.0 * s)),
        (half_h / (2.0 * s), half_h / (2.0 * c)),
        (t * half_w, t * half_h),
    ];
    let mut best = (0.0_f64, 0.0_f64);
    for (u, v) in candidates {
        // The slack absorbs the rounding of the candidate itself; the caller's
        // FLOOR keeps the realised rect inside the page either way.
        let feasible = u > 0.0
            && v > 0.0
            && u.mul_add(c, v * s) <= half_w + INSCRIBED_SLACK_PX
            && u.mul_add(s, v * c) <= half_h + INSCRIBED_SLACK_PX;
        if feasible && u * v > best.0 * best.1 {
            best = (u, v);
        }
    }
    best
}

/// Clamps a fine straightening angle into the open range `(-45, 45)`.
///
/// A non-finite angle becomes `0.0`. Because the range is open, an exact ±45° is
/// moved [`ANGLE_EPSILON_DEG`] toward zero rather than kept; anything past ±45°
/// belongs to another quarter turn, which is [`normalize_rotation`]'s job.
#[must_use]
pub(super) fn clamp_fine_angle(angle_deg: f64) -> f64 {
    if !angle_deg.is_finite() {
        return 0.0;
    }
    let limit = MAX_FINE_ANGLE_DEG - ANGLE_EPSILON_DEG;
    angle_deg.clamp(-limit, limit)
}

/// Normalizes any rotation into the canonical `(quarter_turns, angle_deg)` pair:
/// `quarter_turns` in `0..=3` and `angle_deg` strictly inside ±45°.
///
/// `quarter_turns` may be negative or arbitrarily large and `angle_deg`
/// unbounded: the total rotation is reduced modulo 360° and split at the NEAREST
/// quarter turn, so straightening past 45° rolls into the next 90° step instead
/// of leaving an out-of-range angle. A non-finite angle is read as `0.0`.
///
/// The tie at exactly ±45° goes to the LARGER quarter turn, and the residual is
/// then nudged inside the open range by [`clamp_fine_angle`].
#[must_use]
pub(super) fn normalize_rotation(quarter_turns: i32, angle_deg: f64) -> (u8, f64) {
    let angle = if angle_deg.is_finite() { angle_deg } else { 0.0 };
    let total = f64::from(quarter_turns) * 90.0 + angle;
    // `rem_euclid` keeps the result non-negative, so the floor below is stable
    // for negative inputs too.
    let wrapped = total.rem_euclid(360.0);
    // Splitting at `+45` puts the residual in [-45, 45): the nearest quarter turn.
    let quarter = ((wrapped + MAX_FINE_ANGLE_DEG) / 90.0).floor();
    let residual = wrapped - quarter * 90.0;
    let turns = round_to_px(quarter).rem_euclid(4);
    let turns = u8::try_from(turns).unwrap_or(0);
    (turns, clamp_fine_angle(residual))
}

/// Checks the preconditions of a crop request against the rotated canvas.
///
/// The single validation gate of the window: the confirm button is enabled
/// exactly when this succeeds. It mirrors the engine's contract so the dialog can
/// never emit a request the engine then refuses. `min_size` is deliberately NOT
/// checked here — it is an interaction floor of the drag, not something the
/// engine cares about.
///
/// # Errors
/// [`CropLayoutError::CanvasEmpty`], [`CropLayoutError::FrameEmpty`],
/// [`CropLayoutError::FrameOutsideCanvas`],
/// [`CropLayoutError::QuarterTurnsOutOfRange`] or
/// [`CropLayoutError::AngleOutOfRange`], in that order of checking.
pub(super) fn validate(
    canvas: [u32; 2],
    rect: [u32; 4],
    quarter_turns: u8,
    angle_deg: f64,
) -> Result<(), CropLayoutError> {
    validate_rect(canvas, rect)?;
    if quarter_turns > 3 {
        return Err(CropLayoutError::QuarterTurnsOutOfRange { quarter_turns });
    }
    if !angle_deg.is_finite() || angle_deg.abs() >= MAX_FINE_ANGLE_DEG {
        return Err(CropLayoutError::AngleOutOfRange { angle_deg });
    }
    Ok(())
}

/// The rect-only half of [`validate`], shared with [`CropFrame::new`].
fn validate_rect(canvas: [u32; 2], rect: [u32; 4]) -> Result<(), CropLayoutError> {
    if canvas[0] == 0 || canvas[1] == 0 {
        return Err(CropLayoutError::CanvasEmpty {
            width: canvas[0],
            height: canvas[1],
        });
    }
    if rect[2] == 0 || rect[3] == 0 {
        return Err(CropLayoutError::FrameEmpty {
            width: rect[2],
            height: rect[3],
        });
    }
    // Widened to `u64` because `x + w` overflows `u32` near its maximum, and a
    // wrapped sum would report a rect outside the canvas as inside it.
    let right = u64::from(rect[0]) + u64::from(rect[2]);
    let bottom = u64::from(rect[1]) + u64::from(rect[3]);
    if right > u64::from(canvas[0]) || bottom > u64::from(canvas[1]) {
        return Err(CropLayoutError::FrameOutsideCanvas { rect, canvas });
    }
    Ok(())
}

/// Builds a valid frame from raw edge coordinates, clamping size and position
/// into `canvas` with sides of at least `min` (or of at least 1 px when the
/// canvas itself is smaller than `min`).
///
/// The last safety net of every drag: whatever the edge arithmetic produced, the
/// frame that leaves this function satisfies [`CropFrame`]'s invariants. A
/// zero-sided `canvas` is read as `1x1`, because a frame must exist to be
/// returned; [`validate`] refuses such a canvas at the confirm gate.
fn frame_from_edges(
    canvas: [u32; 2],
    left: i64,
    top: i64,
    right: i64,
    bottom: i64,
    min: i64,
) -> CropFrame {
    let canvas_w = i64::from(canvas[0].max(1));
    let canvas_h = i64::from(canvas[1].max(1));
    let min_w = min.clamp(1, canvas_w);
    let min_h = min.clamp(1, canvas_h);
    let width = right.saturating_sub(left).clamp(min_w, canvas_w);
    let height = bottom.saturating_sub(top).clamp(min_h, canvas_h);
    let x = left.clamp(0, canvas_w - width);
    let y = top.clamp(0, canvas_h - height);
    CropFrame {
        x: to_u32(x),
        y: to_u32(y),
        w: to_u32(width).max(1),
        h: to_u32(height).max(1),
    }
}

/// Narrows a coordinate already clamped into `0..=u32::MAX`; a value outside it
/// would be a bug in the caller and becomes `0` rather than a panic.
fn to_u32(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

/// Rounds a float pixel quantity to the nearest whole pixel.
///
/// NaN becomes `0` (a drag with no movement) and the magnitude is capped at
/// [`PX_LIMIT`], far outside any canvas, so a runaway pointer delta cannot wrap.
fn round_to_px(value: f64) -> i64 {
    if value.is_nan() {
        return 0;
    }
    let rounded = value.round().clamp(-PX_LIMIT, PX_LIMIT);
    // Justified `as`: `rounded` is finite, already integral and bounded by 2^32,
    // so the float->integer conversion is exact and cannot truncate.
    rounded as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame that must be valid; panics with the reason when a test builds a
    /// bad one, which is a defect in the test rather than in the module.
    fn frame(canvas: [u32; 2], rect: [u32; 4]) -> CropFrame {
        match CropFrame::new(canvas, rect) {
            Ok(built) => built,
            Err(error) => panic!("test frame {rect:?} is invalid in canvas {canvas:?}: {error}"),
        }
    }

    /// The canvas every drag test works in, and a frame well inside it.
    const CANVAS: [u32; 2] = [1000, 800];

    fn centred() -> CropFrame {
        frame(CANVAS, [200, 100, 400, 300])
    }

    fn drag(handle: CropHandle, dx: f64, dy: f64) -> [u32; 4] {
        apply_drag(centred(), handle, [dx, dy], CANVAS, 10).rect()
    }

    #[test]
    fn frame_construction_enforces_its_invariants() {
        assert_eq!(frame(CANVAS, [0, 0, 1000, 800]).rect(), [0, 0, 1000, 800]);
        assert_eq!(CropFrame::full(CANVAS).map(CropFrame::rect), Ok([0, 0, 1000, 800]));
        let built = frame(CANVAS, [10, 20, 30, 40]);
        assert_eq!(
            (built.x(), built.y(), built.width(), built.height(), built.right(), built.bottom()),
            (10, 20, 30, 40, 40, 60)
        );
    }

    #[test]
    fn frame_construction_refuses_every_broken_rect() {
        assert_eq!(
            CropFrame::new([0, 800], [0, 0, 1, 1]),
            Err(CropLayoutError::CanvasEmpty { width: 0, height: 800 })
        );
        assert_eq!(
            CropFrame::new(CANVAS, [0, 0, 0, 10]),
            Err(CropLayoutError::FrameEmpty { width: 0, height: 10 })
        );
        assert_eq!(
            CropFrame::new(CANVAS, [0, 0, 10, 0]),
            Err(CropLayoutError::FrameEmpty { width: 10, height: 0 })
        );
        // One pixel past the right edge, and one past the bottom edge.
        assert_eq!(
            CropFrame::new(CANVAS, [991, 0, 10, 10]),
            Err(CropLayoutError::FrameOutsideCanvas { rect: [991, 0, 10, 10], canvas: CANVAS })
        );
        assert_eq!(
            CropFrame::new(CANVAS, [0, 791, 10, 10]),
            Err(CropLayoutError::FrameOutsideCanvas { rect: [0, 791, 10, 10], canvas: CANVAS })
        );
        // `x + w` must not wrap: without the u64 widening this reads as inside.
        assert_eq!(
            CropFrame::new(CANVAS, [u32::MAX, 0, 2, 2]),
            Err(CropLayoutError::FrameOutsideCanvas { rect: [u32::MAX, 0, 2, 2], canvas: CANVAS })
        );
    }

    #[test]
    fn handles_declare_exactly_the_edges_they_move() {
        assert_eq!(CropHandle::TopLeft.edges(), (true, true, false, false));
        assert_eq!(CropHandle::Top.edges(), (false, true, false, false));
        assert_eq!(CropHandle::TopRight.edges(), (false, true, true, false));
        assert_eq!(CropHandle::Right.edges(), (false, false, true, false));
        assert_eq!(CropHandle::BottomRight.edges(), (false, false, true, true));
        assert_eq!(CropHandle::Bottom.edges(), (false, false, false, true));
        assert_eq!(CropHandle::BottomLeft.edges(), (true, false, false, true));
        assert_eq!(CropHandle::Left.edges(), (true, false, false, false));
        assert_eq!(CropHandle::Move.edges(), (false, false, false, false));
        for handle in HANDLE_PRIORITY {
            // Every resize handle is a corner XOR an edge: it moves either one
            // horizontal and one vertical edge, or exactly one edge.
            let (left, top, right, bottom) = handle.edges();
            let is_edge = (left || right) != (top || bottom);
            assert!(handle.is_corner() != is_edge, "{handle:?} is neither or both");
            assert!(!handle.is_move());
        }
        assert!(CropHandle::Move.is_move());
        assert_eq!(CropHandle::Move.edges(), (false, false, false, false));
        assert!(!CropHandle::Move.is_corner());
    }

    #[test]
    fn handle_rects_keep_a_screen_constant_size_at_any_frame_size() {
        let small = ScreenRect::from_min_max(0.0, 0.0, 20.0, 20.0);
        let large = ScreenRect::from_min_max(0.0, 0.0, 2000.0, 900.0);
        for frame_screen in [small, large] {
            let rects = handle_rects(frame_screen, 14.0);
            assert_eq!(rects.len(), 8);
            for (handle, rect) in rects {
                let (w, h) = (rect.width(), rect.height());
                if handle.is_corner() {
                    assert!((w - 14.0).abs() < 1e-3 && (h - 14.0).abs() < 1e-3, "{handle:?}");
                } else {
                    // Edge handles are stretched along their own edge only.
                    let long = 14.0 * EDGE_HANDLE_LENGTH_FACTOR;
                    let expected = match handle {
                        CropHandle::Top | CropHandle::Bottom => (long, 14.0),
                        CropHandle::Left | CropHandle::Right => (14.0, long),
                        CropHandle::TopLeft
                        | CropHandle::TopRight
                        | CropHandle::BottomRight
                        | CropHandle::BottomLeft
                        | CropHandle::Move => unreachable!("not an edge handle"),
                    };
                    assert!((w - expected.0).abs() < 1e-3 && (h - expected.1).abs() < 1e-3, "{handle:?}");
                }
            }
        }
    }

    #[test]
    fn handle_rects_sit_on_their_own_corner_or_edge() {
        let frame_screen = ScreenRect::from_min_max(100.0, 50.0, 300.0, 250.0);
        let rects = handle_rects(frame_screen, 10.0);
        let find = |wanted: CropHandle| {
            rects
                .iter()
                .find(|(handle, _)| *handle == wanted)
                .map(|(_, rect)| rect.center())
        };
        assert_eq!(find(CropHandle::TopLeft), Some((100.0, 50.0)));
        assert_eq!(find(CropHandle::BottomRight), Some((300.0, 250.0)));
        assert_eq!(find(CropHandle::Top), Some((200.0, 50.0)));
        assert_eq!(find(CropHandle::Left), Some((100.0, 150.0)));
    }

    #[test]
    fn hit_test_prefers_corners_then_edges_then_move() {
        let frame_screen = ScreenRect::from_min_max(100.0, 50.0, 300.0, 250.0);
        // Exactly on a corner: the corner wins over both edges that share it.
        assert_eq!(hit_test(frame_screen, 14.0, 100.0, 50.0), Some(CropHandle::TopLeft));
        assert_eq!(hit_test(frame_screen, 14.0, 300.0, 250.0), Some(CropHandle::BottomRight));
        // Mid-edge, away from every corner.
        assert_eq!(hit_test(frame_screen, 14.0, 200.0, 50.0), Some(CropHandle::Top));
        assert_eq!(hit_test(frame_screen, 14.0, 300.0, 150.0), Some(CropHandle::Right));
        // Deep inside: the move region.
        assert_eq!(hit_test(frame_screen, 14.0, 200.0, 150.0), Some(CropHandle::Move));
        // Outside everything.
        assert_eq!(hit_test(frame_screen, 14.0, 20.0, 20.0), None);
        assert_eq!(hit_test(frame_screen, 14.0, 400.0, 150.0), None);
    }

    #[test]
    fn a_frame_too_small_on_screen_has_no_move_region() {
        let tiny = ScreenRect::from_min_max(0.0, 0.0, 8.0, 8.0);
        assert_eq!(move_rect(tiny, 14.0), None);
        // Every point of it still grabs a resize handle, so the frame is not stuck.
        assert!(hit_test(tiny, 14.0, 4.0, 4.0).is_some());
        let roomy = ScreenRect::from_min_max(0.0, 0.0, 200.0, 200.0);
        assert!(move_rect(roomy, 14.0).is_some_and(ScreenRect::is_positive));
    }

    #[test]
    fn every_handle_drags_in_both_directions() {
        // Left edge: negative dx widens, positive dx narrows; the right edge stays.
        assert_eq!(drag(CropHandle::Left, -50.0, 0.0), [150, 100, 450, 300]);
        assert_eq!(drag(CropHandle::Left, 50.0, 0.0), [250, 100, 350, 300]);
        // Right edge: mirror image, the left edge stays.
        assert_eq!(drag(CropHandle::Right, 50.0, 0.0), [200, 100, 450, 300]);
        assert_eq!(drag(CropHandle::Right, -50.0, 0.0), [200, 100, 350, 300]);
        // Top and bottom edges.
        assert_eq!(drag(CropHandle::Top, 0.0, -40.0), [200, 60, 400, 340]);
        assert_eq!(drag(CropHandle::Top, 0.0, 40.0), [200, 140, 400, 260]);
        assert_eq!(drag(CropHandle::Bottom, 0.0, 40.0), [200, 100, 400, 340]);
        assert_eq!(drag(CropHandle::Bottom, 0.0, -40.0), [200, 100, 400, 260]);
        // Corners move one horizontal and one vertical edge, and nothing else.
        assert_eq!(drag(CropHandle::TopLeft, -20.0, -30.0), [180, 70, 420, 330]);
        assert_eq!(drag(CropHandle::TopRight, 20.0, -30.0), [200, 70, 420, 330]);
        assert_eq!(drag(CropHandle::BottomRight, 20.0, 30.0), [200, 100, 420, 330]);
        assert_eq!(drag(CropHandle::BottomLeft, -20.0, 30.0), [180, 100, 420, 330]);
        // A perpendicular delta on an edge handle is ignored.
        assert_eq!(drag(CropHandle::Left, -50.0, 999.0), [150, 100, 450, 300]);
        assert_eq!(drag(CropHandle::Top, 999.0, -40.0), [200, 60, 400, 340]);
    }

    #[test]
    fn a_drag_applies_the_delta_and_not_the_pointer_position() {
        // Two half-drags from the SAME start frame differ, and the second is not
        // the sum: the frame at drag start is always the reference.
        assert_eq!(drag(CropHandle::Right, 10.0, 0.0), [200, 100, 410, 300]);
        assert_eq!(drag(CropHandle::Right, 20.0, 0.0), [200, 100, 420, 300]);
        // Sub-pixel deltas round to the nearest whole pixel.
        assert_eq!(drag(CropHandle::Right, 0.4, 0.0), [200, 100, 400, 300]);
        assert_eq!(drag(CropHandle::Right, 0.6, 0.0), [200, 100, 401, 300]);
        // A non-finite delta is read as no movement instead of panicking.
        assert_eq!(drag(CropHandle::Right, f64::NAN, f64::NAN), [200, 100, 400, 300]);
    }

    #[test]
    fn a_drag_clamps_at_all_four_canvas_edges() {
        assert_eq!(drag(CropHandle::Left, -10_000.0, 0.0), [0, 100, 600, 300]);
        assert_eq!(drag(CropHandle::Right, 10_000.0, 0.0), [200, 100, 800, 300]);
        assert_eq!(drag(CropHandle::Top, 0.0, -10_000.0), [200, 0, 400, 400]);
        assert_eq!(drag(CropHandle::Bottom, 0.0, 10_000.0), [200, 100, 400, 700]);
        // A corner clamps on both of its axes at once.
        assert_eq!(drag(CropHandle::TopLeft, -10_000.0, -10_000.0), [0, 0, 600, 400]);
        assert_eq!(drag(CropHandle::BottomRight, 10_000.0, 10_000.0), [200, 100, 800, 700]);
    }

    #[test]
    fn a_dragged_edge_clamps_at_its_opposite_instead_of_inverting() {
        // Pushing each edge far past its opposite leaves a min_size-wide frame
        // pinned to that opposite edge — never a flipped one.
        assert_eq!(drag(CropHandle::Left, 10_000.0, 0.0), [590, 100, 10, 300]);
        assert_eq!(drag(CropHandle::Right, -10_000.0, 0.0), [200, 100, 10, 300]);
        assert_eq!(drag(CropHandle::Top, 0.0, 10_000.0), [200, 390, 400, 10]);
        assert_eq!(drag(CropHandle::Bottom, 0.0, -10_000.0), [200, 100, 400, 10]);
        assert_eq!(drag(CropHandle::TopLeft, 10_000.0, 10_000.0), [590, 390, 10, 10]);
    }

    #[test]
    fn the_minimum_size_is_honoured_and_capped_by_the_canvas() {
        let start = frame(CANVAS, [200, 100, 400, 300]);
        let squeezed = apply_drag(start, CropHandle::Right, [-10_000.0, 0.0], CANVAS, 120);
        assert_eq!(squeezed.rect(), [200, 100, 120, 300]);
        // A minimum larger than the canvas cannot be met, so the canvas wins.
        let narrow_canvas = [40, 30];
        let tiny = frame(narrow_canvas, [0, 0, 40, 30]);
        let clamped = apply_drag(tiny, CropHandle::Right, [-10_000.0, 0.0], narrow_canvas, 500);
        assert_eq!(clamped.rect(), [0, 0, 30, 30]);
        // `min_size == 0` still leaves a usable frame.
        let zero_min = apply_drag(start, CropHandle::Right, [-10_000.0, 0.0], CANVAS, 0);
        assert_eq!(zero_min.rect(), [200, 100, 1, 300]);
    }

    #[test]
    fn move_translates_the_frame_and_clamps_without_resizing() {
        assert_eq!(drag(CropHandle::Move, 100.0, -50.0), [300, 50, 400, 300]);
        assert_eq!(drag(CropHandle::Move, -10_000.0, -10_000.0), [0, 0, 400, 300]);
        assert_eq!(drag(CropHandle::Move, 10_000.0, 10_000.0), [600, 500, 400, 300]);
        // A frame larger than the canvas is clamped to it rather than moved out.
        let oversized = frame([100, 100], [0, 0, 100, 100]);
        let moved = apply_drag(oversized, CropHandle::Move, [50.0, 50.0], [80, 80], 10);
        assert_eq!(moved.rect(), [0, 0, 80, 80]);
    }

    /// A square lock, the simplest ratio to assert against.
    fn square() -> AspectRatio {
        match AspectRatio::locked(1, 1) {
            Some(ratio) => ratio,
            None => panic!("1:1 is a legal ratio"),
        }
    }

    #[test]
    fn a_degenerate_ratio_is_treated_as_free() {
        assert_eq!(AspectRatio::locked(0, 5), None);
        assert_eq!(AspectRatio::locked(5, 0), None);
        assert_eq!(AspectRatio::Free.value(), None);
        assert_eq!(AspectRatio::Locked { w: 3, h: 0 }.value(), None);
        assert_eq!(AspectRatio::Locked { w: 3, h: 2 }.value(), Some(1.5));
        let free_result = apply_drag_with_ratio(
            centred(),
            CropHandle::Right,
            [50.0, 0.0],
            CANVAS,
            10,
            AspectRatio::Locked { w: 0, h: 0 },
        );
        assert_eq!(free_result.rect(), drag(CropHandle::Right, 50.0, 0.0));
    }

    #[test]
    fn a_locked_corner_drag_covers_the_pointer_from_the_opposite_corner() {
        let start = frame(CANVAS, [200, 100, 200, 200]);
        // Horizontal movement leads when it is the larger of the two candidates.
        let wider = apply_drag_with_ratio(start, CropHandle::BottomRight, [80.0, 10.0], CANVAS, 10, square());
        assert_eq!(wider.rect(), [200, 100, 280, 280]);
        // A purely VERTICAL corner drag still resizes: the vertical candidate wins.
        let taller = apply_drag_with_ratio(start, CropHandle::BottomRight, [0.0, 80.0], CANVAS, 10, square());
        assert_eq!(taller.rect(), [200, 100, 280, 280]);
        // TopLeft keeps the bottom-right corner pinned.
        let from_top_left = apply_drag_with_ratio(start, CropHandle::TopLeft, [-60.0, 0.0], CANVAS, 10, square());
        assert_eq!(from_top_left.rect(), [140, 40, 260, 260]);
        // Shrinking works in the other direction too.
        let smaller = apply_drag_with_ratio(start, CropHandle::BottomRight, [-100.0, -100.0], CANVAS, 10, square());
        assert_eq!(smaller.rect(), [200, 100, 100, 100]);
    }

    #[test]
    fn a_locked_corner_drag_stays_inside_the_canvas_on_both_axes() {
        let start = frame(CANVAS, [200, 100, 200, 200]);
        // The bottom edge (700 px of room) binds before the right edge (800 px).
        let clamped = apply_drag_with_ratio(start, CropHandle::BottomRight, [10_000.0, 10_000.0], CANVAS, 10, square());
        assert_eq!(clamped.rect(), [200, 100, 700, 700]);
        // The top-left corner is limited by the smaller of x (200) and y (100).
        let clamped_up = apply_drag_with_ratio(start, CropHandle::TopLeft, [-10_000.0, -10_000.0], CANVAS, 10, square());
        assert_eq!(clamped_up.rect(), [100, 0, 300, 300]);
        // A non-square ratio keeps its shape while it clamps.
        let three_to_one = match AspectRatio::locked(3, 1) {
            Some(ratio) => ratio,
            None => panic!("3:1 is a legal ratio"),
        };
        let wide = apply_drag_with_ratio(start, CropHandle::BottomRight, [10_000.0, 0.0], CANVAS, 10, three_to_one);
        assert_eq!(wide.rect(), [200, 100, 800, 267]);
    }

    #[test]
    fn a_locked_corner_drag_never_falls_below_the_minimum() {
        let start = frame(CANVAS, [200, 100, 200, 200]);
        let squeezed = apply_drag_with_ratio(start, CropHandle::BottomRight, [-10_000.0, -10_000.0], CANVAS, 40, square());
        assert_eq!(squeezed.rect(), [200, 100, 40, 40]);
        // A ratio that cannot fit between the minimum and the canvas is REFUSED,
        // leaving the frame untouched rather than distorted.
        let tall = match AspectRatio::locked(1, 20) {
            Some(ratio) => ratio,
            None => panic!("1:20 is a legal ratio"),
        };
        let cramped_canvas = [100, 100];
        let cramped = frame(cramped_canvas, [0, 0, 20, 100]);
        let refused = apply_drag_with_ratio(cramped, CropHandle::BottomRight, [50.0, 50.0], cramped_canvas, 90, tall);
        assert_eq!(refused.rect(), cramped.rect());
    }

    #[test]
    fn a_locked_edge_drag_grows_symmetrically_about_the_fixed_centre() {
        let start = frame(CANVAS, [200, 100, 200, 200]);
        // Right edge: the WIDTH leads, the height follows about y-centre 200.
        let wider = apply_drag_with_ratio(start, CropHandle::Right, [100.0, 0.0], CANVAS, 10, square());
        assert_eq!(wider.rect(), [200, 50, 300, 300]);
        // Left edge: same, anchored on the right edge (400).
        let from_left = apply_drag_with_ratio(start, CropHandle::Left, [-100.0, 0.0], CANVAS, 10, square());
        assert_eq!(from_left.rect(), [100, 50, 300, 300]);
        // Bottom edge: the HEIGHT leads, the width follows about x-centre 300.
        let taller = apply_drag_with_ratio(start, CropHandle::Bottom, [0.0, 100.0], CANVAS, 10, square());
        assert_eq!(taller.rect(), [150, 100, 300, 300]);
        // Top edge: anchored on the bottom edge (300).
        let from_top = apply_drag_with_ratio(start, CropHandle::Top, [0.0, -100.0], CANVAS, 10, square());
        assert_eq!(from_top.rect(), [150, 0, 300, 300]);
        // The perpendicular component of an edge drag is ignored under a lock too.
        let ignored = apply_drag_with_ratio(start, CropHandle::Right, [100.0, 999.0], CANVAS, 10, square());
        assert_eq!(ignored.rect(), wider.rect());
    }

    #[test]
    fn a_locked_edge_drag_is_limited_by_the_centred_axis() {
        // Centre y is 150, so the height can reach 2*min(150, 650) = 300 and a
        // square frame therefore stops at 300 px wide, well before the canvas.
        let start = frame(CANVAS, [0, 100, 100, 100]);
        let clamped = apply_drag_with_ratio(start, CropHandle::Right, [10_000.0, 0.0], CANVAS, 10, square());
        assert_eq!(clamped.rect(), [0, 0, 300, 300]);
        // Under the minimum the drag stops at min_size on both sides.
        let squeezed = apply_drag_with_ratio(start, CropHandle::Right, [-10_000.0, 0.0], CANVAS, 30, square());
        assert_eq!(squeezed.rect(), [0, 135, 30, 30]);
    }

    #[test]
    fn a_locked_move_is_a_plain_translation() {
        let start = frame(CANVAS, [200, 100, 200, 200]);
        let moved = apply_drag_with_ratio(start, CropHandle::Move, [50.0, 25.0], CANVAS, 10, square());
        assert_eq!(moved.rect(), [250, 125, 200, 200]);
    }

    #[test]
    fn the_fit_helpers_centre_the_largest_frame_that_fits() {
        // Free: the whole canvas.
        assert_eq!(
            largest_centred_frame(CANVAS, AspectRatio::Free).map(CropFrame::rect),
            Ok([0, 0, 1000, 800])
        );
        // Square in a landscape canvas: the height binds.
        assert_eq!(
            largest_centred_frame(CANVAS, square()).map(CropFrame::rect),
            Ok([100, 0, 800, 800])
        );
        // A ratio taller than the canvas: the width binds.
        let one_to_two = match AspectRatio::locked(1, 2) {
            Some(ratio) => ratio,
            None => panic!("1:2 is a legal ratio"),
        };
        assert_eq!(
            largest_centred_frame([1000, 800], one_to_two).map(CropFrame::rect),
            Ok([300, 0, 400, 800])
        );
        // The result is always inside the canvas, whatever the rounding.
        let odd = match AspectRatio::locked(7, 3) {
            Some(ratio) => ratio,
            None => panic!("7:3 is a legal ratio"),
        };
        let fitted = largest_centred_frame([999, 501], odd);
        assert!(fitted.is_ok_and(|f| f.right() <= 999 && f.bottom() <= 501));
        assert_eq!(
            largest_centred_frame([0, 10], square()),
            Err(CropLayoutError::CanvasEmpty { width: 0, height: 10 })
        );
        assert_eq!(
            CropFrame::full([10, 0]),
            Err(CropLayoutError::CanvasEmpty { width: 10, height: 0 })
        );
    }

    /// Compares a normalized rotation, tolerating the epsilon nudge.
    fn assert_rotation(actual: (u8, f64), turns: u8, angle: f64) {
        assert_eq!(actual.0, turns, "quarter turns of {actual:?}");
        assert!(
            (actual.1 - angle).abs() < 1e-5,
            "angle of {actual:?} is not {angle}"
        );
    }

    #[test]
    fn rotation_normalizes_into_the_canonical_pair() {
        assert_rotation(normalize_rotation(0, 0.0), 0, 0.0);
        assert_rotation(normalize_rotation(2, 10.0), 2, 10.0);
        assert_rotation(normalize_rotation(3, -12.5), 3, -12.5);
        // Full turns fall away, in both directions.
        assert_rotation(normalize_rotation(4, 5.0), 0, 5.0);
        assert_rotation(normalize_rotation(-4, 5.0), 0, 5.0);
        assert_rotation(normalize_rotation(9, 0.0), 1, 0.0);
        assert_rotation(normalize_rotation(-1, 0.0), 3, 0.0);
        assert_rotation(normalize_rotation(-1, -90.0), 2, 0.0);
        // A non-finite angle is read as no fine rotation.
        assert_rotation(normalize_rotation(1, f64::NAN), 1, 0.0);
        assert_rotation(normalize_rotation(1, f64::INFINITY), 1, 0.0);
    }

    #[test]
    fn rotation_past_the_boundary_rolls_into_the_next_quarter_turn() {
        assert_rotation(normalize_rotation(0, 46.0), 1, -44.0);
        assert_rotation(normalize_rotation(0, -46.0), 3, 44.0);
        assert_rotation(normalize_rotation(0, 44.0), 0, 44.0);
        assert_rotation(normalize_rotation(3, 50.0), 0, -40.0);
        assert_rotation(normalize_rotation(0, 200.0), 2, 20.0);
        // Exactly ±45° is not representable in the OPEN range. The tie always
        // goes to the LARGER quarter turn, so the residual is the -45 side of the
        // pair, nudged just inside the range: +45° becomes turn 1 at -45°, and
        // -45° becomes turn 0 (a full turn back) at -45° rather than turn 3.
        let (turns, angle) = normalize_rotation(0, 45.0);
        assert_eq!(turns, 1);
        assert!(angle > -MAX_FINE_ANGLE_DEG && (angle + 45.0).abs() < 1e-5);
        let (turns, angle) = normalize_rotation(0, -45.0);
        assert_eq!(turns, 0);
        assert!(angle > -MAX_FINE_ANGLE_DEG && (angle + 45.0).abs() < 1e-5);
        // Whatever it returns is accepted by `validate`.
        for degrees in [-720.0, -45.0, -0.5, 0.0, 44.999, 45.0, 133.7, 1000.0] {
            let (turns, angle) = normalize_rotation(0, degrees);
            assert_eq!(validate([10, 10], [0, 0, 10, 10], turns, angle), Ok(()), "{degrees}");
        }
    }

    #[test]
    fn the_fine_angle_clamps_strictly_inside_the_open_range() {
        assert!((clamp_fine_angle(10.0) - 10.0).abs() < f64::EPSILON);
        assert!(clamp_fine_angle(90.0) < MAX_FINE_ANGLE_DEG);
        assert!(clamp_fine_angle(-90.0) > -MAX_FINE_ANGLE_DEG);
        assert!((clamp_fine_angle(45.0) - MAX_FINE_ANGLE_DEG).abs() < 1e-5);
        assert!((clamp_fine_angle(f64::NAN)).abs() < f64::EPSILON);
        assert!((clamp_fine_angle(f64::NEG_INFINITY)).abs() < f64::EPSILON);
    }

    /// The engine's canvas rounding, for building a rotated canvas in a test.
    ///
    /// Justified `as`: the argument is derived from the test's own page sizes
    /// (at most a few thousand px), so the value is a non-negative integer far
    /// inside `u32` and the conversion is exact.
    fn ceil_px(value: f64) -> u32 {
        value.ceil() as u32
    }

    /// Whether a CENTRED frame of this size fits inside the page rotated by
    /// `angle_deg`, using the constraint the fit itself is derived from. The
    /// tolerance covers the half-pixel the integer centring can introduce.
    fn fits_inside_rotated_page(
        frame: CropFrame,
        canvas: [u32; 2],
        turned: [u32; 2],
        angle_deg: f64,
    ) -> bool {
        let (sin, cos) = angle_deg.to_radians().sin_cos();
        let (s, c) = (sin.abs(), cos.abs());
        // Measure the frame's extreme corner from the CANVAS centre, which is
        // where the rotated page is centred.
        let u = (f64::from(frame.x()) - f64::from(canvas[0]) / 2.0)
            .abs()
            .max((f64::from(frame.right()) - f64::from(canvas[0]) / 2.0).abs());
        let v = (f64::from(frame.y()) - f64::from(canvas[1]) / 2.0)
            .abs()
            .max((f64::from(frame.bottom()) - f64::from(canvas[1]) / 2.0).abs());
        u * c + v * s <= f64::from(turned[0]) / 2.0 + 1.0
            && u * s + v * c <= f64::from(turned[1]) / 2.0 + 1.0
    }

    #[test]
    fn recentring_follows_a_canvas_that_grows_around_a_centred_page() {
        // The canvas grew by 200 px on each axis, so every page pixel moved by
        // 100; the frame must move with it and keep its size.
        let moved = recentre_frame(frame([800, 1200], [100, 100, 400, 500]), [800, 1200], [1000, 1400], 10);
        assert_eq!(moved.rect(), [200, 200, 400, 500]);
        // And back again when the canvas shrinks.
        let back = recentre_frame(moved, [1000, 1400], [800, 1200], 10);
        assert_eq!(back.rect(), [100, 100, 400, 500]);
    }

    #[test]
    fn recentring_clamps_instead_of_leaving_the_new_canvas() {
        // A frame filling the old canvas cannot fit a much smaller one: it is
        // shrunk into it rather than allowed to hang outside.
        let shrunk = recentre_frame(frame([1000, 1400], [0, 0, 1000, 1400]), [1000, 1400], [400, 600], 10);
        assert_eq!(shrunk.rect(), [0, 0, 400, 600]);
        assert!(validate([400, 600], shrunk.rect(), 0, 0.0).is_ok());
    }

    #[test]
    fn recentring_never_leaves_an_invalid_frame() {
        // An odd size change truncates the half-delta; whatever it produces is
        // still a frame valid for the new canvas.
        for new_canvas in [[801, 1201], [799, 1199], [1, 1], [4000, 40]] {
            let moved = recentre_frame(frame([800, 1200], [100, 100, 400, 500]), [800, 1200], new_canvas, 10);
            assert!(
                validate(new_canvas, moved.rect(), 0, 0.0).is_ok(),
                "{moved:?} is not valid in {new_canvas:?}"
            );
        }
    }

    #[test]
    fn the_inscribed_fit_of_an_unrotated_page_is_the_whole_page() {
        // A zero angle leaves no empty corner, so the fit is the whole canvas —
        // and at a zero angle the canvas IS the quarter-turned page.
        assert_eq!(
            largest_inscribed_frame([800, 1200], [800, 1200], 0.0, AspectRatio::Free),
            Ok(frame([800, 1200], [0, 0, 800, 1200]))
        );
        // A quarter turn is still a zero fine angle, on the transposed page.
        assert_eq!(
            largest_inscribed_frame([1200, 800], [1200, 800], 0.0, AspectRatio::Free),
            Ok(frame([1200, 800], [0, 0, 1200, 800]))
        );
    }

    #[test]
    fn the_inscribed_fit_matches_the_hand_derived_square_case() {
        // A square page of side L rotated by θ inscribes a square of side
        // L / (cos θ + sin θ): for L = 100 and θ = 30° that is 73.205 px, and the
        // canvas is ceil(100·cos30 + 100·sin30) = 137 px on both axes.
        let canvas = [137, 137];
        let turned = [100, 100];
        let fitted = largest_inscribed_frame(canvas, turned, 30.0, AspectRatio::Free);
        assert_eq!(fitted, Ok(frame(canvas, [32, 32, 73, 73])));
        // A locked 1:1 must reach exactly the same square by the other branch.
        assert_eq!(
            largest_inscribed_frame(canvas, turned, 30.0, AspectRatio::Locked { w: 1, h: 1 }),
            fitted
        );
    }

    #[test]
    fn the_inscribed_fit_honours_an_extreme_aspect_ratio() {
        // Still the 100x100 page at 30° (canvas 137x137, half-page 50).
        // 10:1 puts `v = u/10`, so the WIDTH constraint binds first:
        // u·cos30 + (u/10)·sin30 = u·0.916025 <= 50  ->  u = 54.5836,
        // i.e. 109 x 10 px, centred at ((137-109)/2, (137-10)/2) = (14, 63).
        let canvas = [137, 137];
        assert_eq!(
            largest_inscribed_frame(canvas, [100, 100], 30.0, AspectRatio::Locked { w: 10, h: 1 }),
            Ok(frame(canvas, [14, 63, 109, 10]))
        );
        // 1:10 is the mirror image and binds on the HEIGHT constraint instead:
        // u·sin30 + 10u·cos30 = u·9.16025 <= 50  ->  u = 5.4584, i.e. 10 x 109.
        assert_eq!(
            largest_inscribed_frame(canvas, [100, 100], 30.0, AspectRatio::Locked { w: 1, h: 10 }),
            Ok(frame(canvas, [63, 14, 10, 109]))
        );
    }

    #[test]
    fn every_inscribed_fit_really_fits_inside_the_rotated_page() {
        let ratios = [
            AspectRatio::Free,
            AspectRatio::Locked { w: 1, h: 1 },
            AspectRatio::Locked { w: 16, h: 9 },
            AspectRatio::Locked { w: 1, h: 12 },
        ];
        for turned in [[800_u32, 1200_u32], [1200, 800], [4000, 200], [37, 41]] {
            for angle in [-44.0_f64, -12.5, -0.75, 0.0, 0.75, 12.5, 44.0] {
                // The canvas of the same rotation, computed the way the engine
                // does: the bounding box of the rotated page.
                let (sin, cos) = angle.to_radians().sin_cos();
                let (s, c) = (sin.abs(), cos.abs());
                let w = f64::from(turned[0]);
                let h = f64::from(turned[1]);
                let canvas = [ceil_px(w * c + h * s), ceil_px(w * s + h * c)];
                for ratio in ratios {
                    let fitted = match largest_inscribed_frame(canvas, turned, angle, ratio) {
                        Ok(fitted) => fitted,
                        Err(error) => panic!("{turned:?} at {angle}° with {ratio:?}: {error}"),
                    };
                    assert!(
                        validate(canvas, fitted.rect(), 0, angle).is_ok(),
                        "{fitted:?} is not a legal crop of {canvas:?}"
                    );
                    assert!(
                        fits_inside_rotated_page(fitted, canvas, turned, angle),
                        "{fitted:?} pokes outside {turned:?} rotated by {angle}° ({ratio:?})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_inscribed_fit_refuses_what_it_cannot_measure() {
        assert_eq!(
            largest_inscribed_frame([0, 100], [10, 10], 0.0, AspectRatio::Free),
            Err(CropLayoutError::CanvasEmpty { width: 0, height: 100 })
        );
        assert_eq!(
            largest_inscribed_frame([100, 100], [10, 0], 0.0, AspectRatio::Free),
            Err(CropLayoutError::CanvasEmpty { width: 10, height: 0 })
        );
        assert_eq!(
            largest_inscribed_frame([100, 100], [10, 10], 45.0, AspectRatio::Free),
            Err(CropLayoutError::AngleOutOfRange { angle_deg: 45.0 })
        );
        assert!(matches!(
            largest_inscribed_frame([100, 100], [10, 10], f64::NAN, AspectRatio::Free),
            Err(CropLayoutError::AngleOutOfRange { .. })
        ));
    }

    #[test]
    fn validate_accepts_a_legal_request() {
        assert_eq!(validate(CANVAS, [0, 0, 1000, 800], 0, 0.0), Ok(()));
        assert_eq!(validate(CANVAS, [999, 799, 1, 1], 3, -44.999), Ok(()));
        assert_eq!(validate([1, 1], [0, 0, 1, 1], 1, 0.0), Ok(()));
    }

    #[test]
    fn validate_refuses_every_engine_precondition() {
        assert_eq!(
            validate([0, 0], [0, 0, 1, 1], 0, 0.0),
            Err(CropLayoutError::CanvasEmpty { width: 0, height: 0 })
        );
        assert_eq!(
            validate(CANVAS, [0, 0, 0, 0], 0, 0.0),
            Err(CropLayoutError::FrameEmpty { width: 0, height: 0 })
        );
        assert_eq!(
            validate(CANVAS, [500, 400, 501, 10], 0, 0.0),
            Err(CropLayoutError::FrameOutsideCanvas { rect: [500, 400, 501, 10], canvas: CANVAS })
        );
        assert_eq!(
            validate(CANVAS, [0, 0, 10, 10], 4, 0.0),
            Err(CropLayoutError::QuarterTurnsOutOfRange { quarter_turns: 4 })
        );
        assert_eq!(
            validate(CANVAS, [0, 0, 10, 10], 0, 45.0),
            Err(CropLayoutError::AngleOutOfRange { angle_deg: 45.0 })
        );
        assert_eq!(
            validate(CANVAS, [0, 0, 10, 10], 0, -45.0),
            Err(CropLayoutError::AngleOutOfRange { angle_deg: -45.0 })
        );
        assert!(matches!(
            validate(CANVAS, [0, 0, 10, 10], 0, f64::NAN),
            Err(CropLayoutError::AngleOutOfRange { .. })
        ));
        assert!(matches!(
            validate(CANVAS, [0, 0, 10, 10], 0, f64::INFINITY),
            Err(CropLayoutError::AngleOutOfRange { .. })
        ));
    }

    #[test]
    fn a_dragged_frame_always_satisfies_validate() {
        // Every handle, every direction, an oversized delta and a degenerate
        // minimum: the result is still a request the engine would accept.
        let handles = [
            CropHandle::TopLeft,
            CropHandle::Top,
            CropHandle::TopRight,
            CropHandle::Right,
            CropHandle::BottomRight,
            CropHandle::Bottom,
            CropHandle::BottomLeft,
            CropHandle::Left,
            CropHandle::Move,
        ];
        for handle in handles {
            for delta in [[-1e9, -1e9], [1e9, 1e9], [0.0, 0.0], [f64::NAN, 3.0]] {
                for min_size in [0, 1, 10, 5000] {
                    let free = apply_drag(centred(), handle, delta, CANVAS, min_size);
                    assert_eq!(validate(CANVAS, free.rect(), 0, 0.0), Ok(()), "{handle:?} {delta:?} {min_size}");
                    let locked = apply_drag_with_ratio(centred(), handle, delta, CANVAS, min_size, square());
                    assert_eq!(validate(CANVAS, locked.rect(), 0, 0.0), Ok(()), "{handle:?} {delta:?} {min_size}");
                }
            }
        }
    }
}
