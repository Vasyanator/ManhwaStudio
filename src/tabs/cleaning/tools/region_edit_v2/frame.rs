/*
File: region_edit_v2/frame.rs

Purpose:
The on-canvas region frame itself: the state a host tool hands around, the per-frame pass
that anchors it to a page, keeps it in view, senses its handles and chrome, paints it, and
the intent it reports back. This is the file a tool talks to; `geometry.rs` holds the maths,
`layers.rs` the pixels, `render.rs` the painting and `input.rs` the hit geometry.

Key structures:
- `RegionFrame`: page anchor, rectangle in source page pixels, masks, result, brush, drag
- `FrameLock`, `FrameVisual`: the derived protection state and the colour it selects
- `FrameHost`, `FrameOutcome`, `FrameButtons`: the per-pass input, the reported intent and
  the enablement table the chrome and the dock panel share
- `PendingRequests`: what a dock panel queued for the next pass, re-checked before it acts

Key functions:
- `RegionFrame::update()`: the whole per-frame pass, run from `CleaningTool::draw_overlay_ui`
- `RegionFrame::set_constraints()`, `set_mask_layers()`, `set_allows_empty_mask()`: what a host
  pushes into the frame when the active consumer changes, or when its parameters do (D16)
- `page_source_size()`: the source-pixel size of a page, i.e. the space `rect_px` lives in
- `RegionFrame::captures_pointer()`, `drag_active()`: what the tool answers the canvas with
- `handle_brush_gestures()`, `paint_brush_cursor()`: the brush gestures and ring the frame has
  to own itself, because `tab.rs` delivers no key, wheel or cursor hook over an occluded pointer
- `derive_lock()`, `derive_visual()`, `derive_buttons()`, `stroke_erases()`,
  `result_hidden()`, `keep_in_view_px()`: the pure rules
- `fold_pending()`: queued panel requests, dropped when the frame no longer allows them

Notes:
The pass runs inside ONE `egui::Area` sized to the HITBOX, never to the viewport: a
viewport-sized area makes egui report the pointer as "over an area" everywhere and kills
canvas wheel scrolling. The area sits on `Order::Middle`, below the dock panels
(`Order::Foreground`), which is also what makes `tab.rs`'s z-order occlusion test treat the
frame as canvas-blocking. Every hover and drag decision goes through a `Response` from
`Ui::interact`, never through a raw pointer position (`egui-docs/06-overlays.md` §5).
The chrome row holds four buttons — «Применить», «Сравнить», «Отменить», «Стереть маску» —
and «Сравнить» is a MOMENTARY HOLD, not a toggle: while its pointer button is down the pending
result layer is not painted, so the original pixels show through. That is why the row is sensed
BEFORE the frame's contents are painted.
Design and the decisions behind it: `dev-docs/region_edit_v2_plan.md` (§1, §2 D1-D6/D10, §10).
*/

use super::geometry::{
    self, FrameChrome, FrameConstraints, PageChoice, PageView, SizeViolation, hitbox_rect,
    keep_in_view_delta, nearest_valid_size,
};
use super::input::{DragKind, DragState, HANDLE_RADIUS, HandleKind, correction_delta_to_px, handle_hit_rects, handle_points, moved_rect_px, resized_rect_px, screen_delta_to_px};
use super::layers::{MaskLayerSpec, MaskStack, ResultLayer};
use super::render;
use crate::canvas::{CanvasView, OverlayRectPx};
use crate::tools::MaskBrush;
use egui::{Color32, Id, Pos2, Rect, Sense, Vec2, pos2, vec2};

/// Id source of the frame's `egui::Area` and the stem of every widget id inside it.
///
/// A literal, never a localized caption: a widget id that changed with the interface
/// language would drop the frame's drag state on a language switch.
const FRAME_AREA_ID: &str = "cleaning_region_frame_v2";

/// Height of the drag strip above the frame, in screen points.
const TOP_STRIP_H: f32 = 20.0;
/// Height of the action button row below the frame, in screen points.
const BUTTONS_H: f32 = 24.0;
/// Height of the status line below the button row, in screen points.
const STATUS_H: f32 = 18.0;
/// Spacing at each seam between the frame and its chrome rows, in screen points.
const CHROME_GAP: f32 = 4.0;
/// Gap between two action buttons, in screen points.
const BUTTON_GAP: f32 = 4.0;
/// Smallest width the chrome rows may have, in screen points.
///
/// The rows carry a status sentence and FOUR captioned buttons, and they must NOT inherit
/// the frame's screen width: the canvas zooms down to 0.2, so a minimum-side frame is barely
/// a dozen points wide there and rows that narrow would spill their text over the artwork and
/// floor each button at a point.
///
/// Sized from the captions themselves, not guessed: at `egui::TextStyle::Button` (13 pt
/// proportional) the widest Russian caption, «Стереть маску», measures 85.1 pt and a button
/// adds `2 × button_padding.x` = 8 pt, so a slot must be at least 93.1 pt wide; four slots
/// separated by `BUTTON_GAP` need `4 × 93.1 + 3 × 4 = 384.4`. 400.0 gives a 97 pt slot, which
/// clears every Russian and English caption with a little headroom for a different UI font.
/// The longer Romance translations («Effacer le masque» at 103.4 pt) still do not fit and are
/// ELIDED by `chrome_button`'s `truncate()`; the tooltip always carries the full text.
/// `chrome_caption_slots_hold_the_russian_and_english_captions` keeps this claim honest.
const CHROME_MIN_W: f32 = 400.0;

/// Side of a freshly spawned frame, in source page pixels, before the consumer's constraints
/// snap it. Large enough to be a useful edit region on a manhwa strip and small enough to
/// fit a phone-sized page.
const DEFAULT_FRAME_SIDE_PX: usize = 512;

/// How much of the frame protects itself against being moved, resized or scrolled away.
///
/// DERIVED every frame by `derive_lock`, never assigned: the frame's protection must follow
/// its contents, and a stored flag would eventually disagree with them (D4 of the design).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLock {
    /// Nothing is held: the frame may be moved, resized and kept in view.
    Free,
    /// The mask stack holds painted pixels.
    MaskPainted,
    /// A processed result is waiting to be applied or discarded.
    ResultPending,
    /// A consumer is working on this frame right now.
    Processing,
}

impl FrameLock {
    /// Whether the frame may be moved, resized, or auto-moved to stay in view.
    #[must_use]
    pub fn is_free(self) -> bool {
        matches!(self, Self::Free)
    }
}

/// Which colour the frame's inner stroke and status line take.
///
/// `Invalid` wins over `Occupied` (D6): a locked frame whose size stopped satisfying the
/// active consumer needs the user to act, and the colour reports the actionable fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameVisual {
    /// Light grey: free, movable, resizable.
    Free,
    /// Red: the size violates the active consumer's requirements.
    Invalid,
    /// Green: a mask, a pending result or running work is held.
    Occupied,
}

/// What the host tool hands the frame for one pass.
#[derive(Debug, Clone, Copy)]
pub struct FrameHost<'a> {
    /// Rects of the dock panels drawn over the canvas this frame, in screen points. They are
    /// cut out of the viewport, so the frame never hides behind one.
    pub panel_rects: &'a [Rect],
    /// Number of pages in the project, i.e. the range `choose_page` may re-anchor into.
    pub page_count: usize,
}

/// What the frame asks the host tool to do, decided during one pass.
///
/// The frame never touches the clean overlay itself: `update` borrows the canvas shared, and
/// the tool performs the requested action with its own `&mut CanvasView`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameOutcome {
    /// The consumer should start processing the current region and mask.
    pub process_requested: bool,
    /// The pending result should be merged into the clean overlay.
    pub apply_requested: bool,
    /// The pending result should be dropped, or running work cancelled.
    pub cancel_requested: bool,
    /// Every mask layer should be cleared.
    pub clear_mask_requested: bool,
}

impl FrameOutcome {
    /// Merges another source of intent into this one.
    ///
    /// Every field is a REQUEST, so the union is the right combination: a dock panel and the
    /// frame's own button row can ask for the same action in one frame, and asking twice must
    /// mean the same as asking once.
    fn merge(&mut self, other: Self) {
        self.process_requested |= other.process_requested;
        self.apply_requested |= other.apply_requested;
        self.cancel_requested |= other.cancel_requested;
        self.clear_mask_requested |= other.clear_mask_requested;
    }
}

/// Which of the five actions are currently available.
///
/// Decided once here so the frame's own button row and the dock panel that repeats these
/// actions can never disagree about what is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameButtons {
    /// «Обработать»: the size is valid, nothing is running and the mask is not empty.
    pub process: bool,
    /// «Применить»: a result is pending.
    pub apply: bool,
    /// «Сравнить»: a result is pending, i.e. there is something to hide. Exactly the same
    /// condition as `apply` — the button exists to show what is UNDER the pending result, and
    /// there is nothing to compare while the frame is free or a run has not answered yet.
    pub compare: bool,
    /// «Отменить»: a result is pending, or work is running and can be cancelled.
    pub cancel: bool,
    /// «Стереть маску»: the mask stack holds something to erase.
    pub clear_mask: bool,
}

/// The lock a frame is under, from the four facts that can hold it.
///
/// Precedence is `Processing` > `ResultPending` > `MaskPainted` > `Free`: the strongest
/// reason present is the one reported, so the status line names the state the user must
/// resolve first.
///
/// `stroke_in_flight` — a paint or erase stroke whose button is still down — locks the frame
/// as `MaskPainted` even while the mask is empty. Without it, erasing the last painted pixel
/// mid-stroke unlocks the frame BEFORE the button is released, and the page transition, the
/// keep-in-view clamp and the mask resize they can trigger then run under the live stroke:
/// the frame moves out from under the pointer and the stroke's own undo snapshot is thrown
/// away. A gesture in flight is a reason to hold the frame, exactly like its contents are.
#[must_use]
fn derive_lock(processing: bool, has_result: bool, mask_empty: bool, stroke_in_flight: bool) -> FrameLock {
    if processing {
        FrameLock::Processing
    } else if has_result {
        FrameLock::ResultPending
    } else if mask_empty && !stroke_in_flight {
        FrameLock::Free
    } else {
        FrameLock::MaskPainted
    }
}

/// Whether a stroke ERASES, from the buttons held, the Shift key and the panel's mode.
///
/// This is the region editor's rule verbatim (`base.rs::draw_mask_editor_image`): the right
/// button erases unless the left is held too, and Shift+left erases whatever mode the panel
/// offers. Shift is here rather than in `CleaningTool::set_temporary_erase` — the tab calls
/// that hook only for a pointer it does NOT consider occluded, and the frame occludes its own
/// hitbox, so the convention has to be read inside the pass to reach this tool at all.
#[must_use]
fn stroke_erases(primary: bool, secondary: bool, shift: bool, mode_erase: bool) -> bool {
    (secondary && !primary) || shift || mode_erase
}

/// The colour state for a lock and an optional size violation (D6: red wins over green).
#[must_use]
fn derive_visual(lock: FrameLock, violation: Option<SizeViolation>) -> FrameVisual {
    if violation.is_some() {
        FrameVisual::Invalid
    } else if lock.is_free() {
        FrameVisual::Free
    } else {
        FrameVisual::Occupied
    }
}

/// The enablement of the four actions, from the table of the design (§10.3).
///
/// «Обработать» needs a lock of `Free` or `MaskPainted`, not merely "not processing": a
/// second run started while a result is still pending would replace that result without the
/// user ever seeing it, and the frame offers no way to get the discarded one back.
///
/// `allows_empty_mask` relaxes the non-empty-mask requirement, and nothing else (D16): a
/// consumer whose run regenerates the WHOLE region has nothing for the user to paint, so
/// demanding a stroke first would make its own mode unreachable. The size check and the lock
/// still apply unchanged.
#[must_use]
fn derive_buttons(lock: FrameLock, violation: Option<SizeViolation>, mask_empty: bool, allows_empty_mask: bool) -> FrameButtons {
    let processing = matches!(lock, FrameLock::Processing);
    let has_result = matches!(lock, FrameLock::ResultPending);
    FrameButtons {
        process: violation.is_none()
            && matches!(lock, FrameLock::Free | FrameLock::MaskPainted)
            && (!mask_empty || allows_empty_mask),
        apply: has_result,
        compare: has_result,
        cancel: has_result || processing,
        clear_mask: !mask_empty,
    }
}

/// Whether the pending result layer must stay HIDDEN this frame, so the user sees the
/// original pixels under it.
///
/// `compare_held` is the momentary «Сравнить» button's state, settled when the button row is
/// sensed; this is read when the contents are painted. Re-checking `buttons.compare` here is
/// therefore not redundant: it is what guarantees that a result which disappeared between the
/// two — applied or cancelled from the dock panel, or dropped by the host tool — can never
/// leave the frame hiding something. A held flag alone is never allowed to hide anything.
#[must_use]
fn result_hidden(buttons: FrameButtons, compare_held: bool) -> bool {
    buttons.compare && compare_held
}

/// The actions a dock panel queued for the next pass.
///
/// A panel body runs inside `CanvasView::draw` and may mutate only the tool, so it cannot act
/// on the canvas itself: it raises a flag here and `update` folds it into the outcome. Only
/// the actions with a `request_*` setter exist as fields — «Стереть маску» is not one,
/// because clearing the mask needs no canvas and a panel does it directly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PendingRequests {
    process: bool,
    apply: bool,
    cancel: bool,
}

/// Folds queued dock-panel requests into this pass's outcome, dropping every one the frame no
/// longer allows.
///
/// Re-checking against `buttons` is what lets a panel repeat the frame's own actions without
/// forking the enablement table: a request raised while an action was legal and consumed one
/// frame later, when it no longer is, is simply dropped.
fn fold_pending(pending: PendingRequests, buttons: FrameButtons, outcome: &mut FrameOutcome) {
    outcome.process_requested |= pending.process && buttons.process;
    outcome.apply_requested |= pending.apply && buttons.apply;
    outcome.cancel_requested |= pending.cancel && buttons.cancel;
}

/// The frame's screen rect: its page-pixel rectangle placed inside the page's screen rect.
///
/// `zoom` is `CanvasView::zoom()`, the same factor the canvas scales a page's source pixels
/// by, so this is the inverse of `screen_pos_to_page_px`. A non-finite or non-positive zoom
/// yields an empty rect at the page origin rather than a rect full of NaN.
#[must_use]
fn frame_screen_rect(page_screen: Rect, zoom: f32, rect_px: OverlayRectPx) -> Rect {
    if !zoom.is_finite() || zoom <= 0.0 {
        return Rect::from_min_size(page_screen.min, Vec2::ZERO);
    }
    let min = page_screen.min + vec2(px_f32(rect_px.x) * zoom, px_f32(rect_px.y) * zoom);
    Rect::from_min_size(min, vec2(px_f32(rect_px.w) * zoom, px_f32(rect_px.h) * zoom))
}

/// A screen position as source page pixels of `page_screen`, clamped to the page.
#[must_use]
fn screen_pos_to_page_px(page_screen: Rect, zoom: f32, pos: Pos2, page_w: usize, page_h: usize) -> (usize, usize) {
    if !zoom.is_finite() || zoom <= 0.0 {
        return (0, 0);
    }
    let x = ((pos.x - page_screen.left()) / zoom).round();
    let y = ((pos.y - page_screen.top()) / zoom).round();
    (clamp_px(x, page_w), clamp_px(y, page_h))
}

/// Rounds a page-pixel coordinate into `0..=limit`. A float-to-integer cast saturates in
/// Rust, and the clamp below runs first, so no value can wrap.
#[must_use]
fn clamp_px(v: f32, limit: usize) -> usize {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    let limit_f = px_f32(limit);
    if v >= limit_f { limit } else { v as usize }
}

/// A page-pixel count as `f32`.
///
/// Page pixel counts are at most a few tens of thousands, far inside the range where `f32`
/// represents every integer exactly, so this conversion is lossless for any input the canvas
/// can produce.
#[inline]
#[must_use]
fn px_f32(v: usize) -> f32 {
    // A count that does not fit `u32` cannot describe a page; saturating there keeps the
    // comparison monotonic instead of wrapping. The `as` cast is exact for every value a
    // page can produce (tens of thousands, far below 2^24).
    u32::try_from(v).unwrap_or(u32::MAX) as f32
}

/// The preview tints of `layers`, in declaration order — what `MaskStack` is built from.
#[must_use]
fn layer_tints(layers: &[MaskLayerSpec]) -> Vec<Color32> {
    layers.iter().map(|layer| layer.tint).collect()
}

/// The SOURCE pixel size of page `page_idx`, i.e. the coordinate space of every
/// `OverlayRectPx` on that page.
///
/// The clean overlay is authoritative when one is allocated. Otherwise the size is re-derived
/// from the page's screen rect and the zoom, because the canvas lays pages out as
/// `screen = source_px * zoom + translation`. `None` when the canvas has not laid the page out
/// yet or the zoom is unusable.
///
/// Public because a host tool needs exactly this number to describe the frame's rectangle to a
/// worker that crops the page file (`RegionLoadRequest::source_size`), and re-deriving it there
/// would be a second copy of a rule that has to agree with the frame's own placement.
#[must_use]
pub fn page_source_size(canvas: &CanvasView, page_idx: usize, zoom: f32) -> Option<[usize; 2]> {
    RegionFrame::page_placement(canvas, page_idx, zoom).map(|page| [page.w, page.h])
}

/// The chrome heights the frame lays its rows out with. One place, so the hitbox the
/// geometry clamps and the rows the pass paints can never disagree.
#[must_use]
fn chrome() -> FrameChrome {
    FrameChrome {
        top_strip_h: TOP_STRIP_H,
        buttons_h: BUTTONS_H,
        status_h: STATUS_H,
        gap: CHROME_GAP,
        min_row_w: CHROME_MIN_W,
        // The handles are painted and sensed OUTSIDE the frame, so the hitbox has to reach
        // that far too — and the rows are then pushed clear of them instead of overlapping.
        handle_margin: HANDLE_RADIUS,
    }
}

/// Where one page sits on screen this frame, and how large it is in source pixels.
///
/// Bundled because every conversion between the frame's page-pixel rectangle and its screen
/// rectangle needs all four values together, and passing them separately made a helper
/// exceed the argument budget without saying anything more.
#[derive(Debug, Clone, Copy)]
struct PagePlacement {
    /// The whole page on screen, in screen points.
    screen: Rect,
    /// The page's width in source pixels.
    w: usize,
    /// The page's height in source pixels.
    h: usize,
    /// `CanvasView::zoom()`: screen points per source pixel.
    zoom: f32,
}

impl PagePlacement {
    /// The screen rectangle of a page-pixel rectangle on this page.
    #[must_use]
    fn screen_rect(&self, rect_px: OverlayRectPx) -> Rect {
        frame_screen_rect(self.screen, self.zoom, rect_px)
    }
}

/// The frame rectangle after this frame's keep-in-view clamp (D3).
///
/// A LOCKED frame is returned unchanged: it is allowed to scroll out of view, and an arrow
/// then points at it. A free frame is translated by `keep_in_view_delta`, which also
/// implements "manual dragging stops at the viewport border" — there is deliberately no
/// second clamp for dragging anywhere in this module.
///
/// The correction is converted to page pixels TOWARD ZERO, so a sub-pixel overhang is left
/// uncorrected rather than overshot; that is what makes this per-frame clamp reach a fixed
/// point instead of alternating between two origins forever (`correction_delta_to_px`).
#[must_use]
fn keep_in_view_px(lock: FrameLock, rect_px: OverlayRectPx, page: &PagePlacement, usable: Rect) -> OverlayRectPx {
    if !lock.is_free() {
        return rect_px;
    }
    let delta = keep_in_view_delta(page.screen_rect(rect_px), &chrome(), page.screen, usable);
    let (dx, dy) = correction_delta_to_px(delta, page.zoom);
    if dx == 0 && dy == 0 {
        return rect_px;
    }
    moved_rect_px(rect_px, (dx, dy), page.w, page.h)
}

/// The on-canvas selection frame: page anchor, contents, and the per-frame pass around them.
///
/// The authoritative state is `(page_idx, rect_px)` in SOURCE PAGE PIXELS (D2); the screen
/// rectangle is re-derived every frame and is never stored as truth. The frame owns no
/// `&mut CanvasView` and applies nothing: it reports intent through `FrameOutcome`.
pub struct RegionFrame {
    constraints: FrameConstraints,
    page_idx: Option<usize>,
    rect_px: Option<OverlayRectPx>,
    masks: MaskStack,
    /// What the active consumer declared about each mask layer: the tint the chips of the top
    /// strip are painted with, and the catalog key their tooltip resolves at draw time.
    /// Always exactly as long as the mask stack has layers.
    layers: Vec<MaskLayerSpec>,
    /// Whether the active consumer accepts a run with every mask layer empty (D16). It only
    /// relaxes `derive_buttons`' non-empty-mask requirement; the lock rules are untouched.
    allows_empty_mask: bool,
    result: Option<ResultLayer>,
    processing: bool,
    drag: Option<DragState>,
    brush: MaskBrush,
    /// Whether a PRIMARY-button stroke erases instead of painting. The secondary button
    /// erases regardless of it, so this is the panel's mode, not the whole erase policy.
    erase: bool,
    /// Last mask pixel a stroke touched, so a fast drag paints a segment rather than dots.
    /// `Some` for exactly as long as the stroke's button is held, which is also what makes it
    /// the frame's "a stroke is in flight" flag (`derive_lock`, `drag_active`).
    last_paint_px: Option<(i32, i32)>,
    /// Whether the chrome's «Сравнить» button is being HELD right now, which hides the pending
    /// result so the original pixels show through.
    ///
    /// Recomputed from scratch in every drawn pass (`draw_rows`) as "the button is enabled AND
    /// its pointer button is down on it" — never toggled and never accumulated, so it cannot
    /// outlive the press. Private on purpose: it is a momentary gesture, not frame state a
    /// host may set. `result_hidden` re-checks the enablement at read time as well.
    compare_held: bool,
    /// Actions pressed in a dock panel, waiting to be folded into the next outcome.
    /// A panel body cannot act directly (it runs inside `CanvasView::draw`), so it raises
    /// these flags and the next pass consumes them — the `CleaningDockOut` rule at tool scope.
    pending: PendingRequests,
    /// Hitbox of the last drawn pass, clipped to the usable viewport, or `None` when the
    /// frame was not drawn. This is what `captures_pointer` answers from.
    hitbox: Option<Rect>,
}

impl std::fmt::Debug for RegionFrame {
    /// Hand-written because `MaskBrush` and the layer types carry buffers that would drown a
    /// derived `Debug` in pixel data.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegionFrame")
            .field("page_idx", &self.page_idx)
            .field("rect_px", &self.rect_px)
            .field("lock", &self.lock())
            .field("layers", &self.masks.layer_count())
            .field("allows_empty_mask", &self.allows_empty_mask)
            .field("has_result", &self.result.is_some())
            .finish()
    }
}

impl RegionFrame {
    /// Creates an UNPLACED frame with `layers.len()` mask layers.
    ///
    /// The frame places itself on the current page the first time `update` runs with a laid
    /// out canvas (D1: there is no rubber band, the frame simply appears). A run with an empty
    /// mask is refused until a consumer says otherwise through `set_allows_empty_mask`.
    #[must_use]
    pub fn new(constraints: FrameConstraints, layers: &[MaskLayerSpec]) -> Self {
        Self {
            constraints,
            page_idx: None,
            rect_px: None,
            // Zero geometry until the frame is placed; `resize` allocates the real buffers.
            masks: MaskStack::new(0, 0, &layer_tints(layers)),
            layers: layers.to_vec(),
            allows_empty_mask: false,
            result: None,
            processing: false,
            drag: None,
            brush: MaskBrush::default(),
            erase: false,
            last_paint_px: None,
            compare_held: false,
            pending: PendingRequests::default(),
            hitbox: None,
        }
    }

    #[must_use]
    pub fn constraints(&self) -> &FrameConstraints {
        &self.constraints
    }

    /// Publishes the size requirements of the consumer that is active now (D16).
    ///
    /// The rectangle is deliberately NOT snapped to them: a frame whose current size the new
    /// consumer refuses turns RED and says so, which is exactly the state `FrameVisual::Invalid`
    /// and `status.invalid_size` exist for. Snapping instead would silently move a rectangle the
    /// user placed, and would do it at the moment their attention is on the consumer list.
    pub fn set_constraints(&mut self, constraints: FrameConstraints) {
        self.constraints = constraints;
    }

    /// Publishes the mask layers of the consumer that is active now (D16).
    ///
    /// Re-creates the mask stack over the CURRENT geometry whenever the declaration differs, so
    /// every layer ends up empty and the previews take the new tints. That destroys painted work,
    /// which is why the call is REFUSED (logged, and otherwise a no-op) unless the frame is free:
    /// a non-empty stack locks the frame, and D15 forbids a consumer switch under that lock for
    /// this very reason. An identical declaration is a no-op and never disturbs the stack.
    pub fn set_mask_layers(&mut self, layers: &[MaskLayerSpec]) {
        if self.layers == layers {
            return;
        }
        if !self.lock().is_free() {
            crate::runtime_log::log_warn(format!(
                "[region-edit-v2] refused to replace the {} mask layer(s) with {}: the frame is locked ({:?}), so the switch would discard painted work",
                self.layers.len(),
                layers.len(),
                self.lock()
            ));
            return;
        }
        let (w, h) = self.masks.size();
        self.masks = MaskStack::new(w, h, &layer_tints(layers));
        self.layers = layers.to_vec();
    }

    /// Publishes whether the active consumer accepts an empty mask (D16).
    ///
    /// Re-pushed every frame by the host, because a consumer may derive it from one of its own
    /// parameters: a copy taken once at the consumer switch would keep blocking «Обработать»
    /// after that parameter changed, or keep allowing it after it changed back.
    pub fn set_allows_empty_mask(&mut self, allow: bool) {
        self.allows_empty_mask = allow;
    }

    /// Localized name of mask layer `idx`, or its 1-based number when the layer was declared
    /// without a resolvable key. Resolved on every call so it follows a language switch.
    #[must_use]
    pub fn layer_label(&self, idx: usize) -> String {
        self.layers.get(idx).map_or_else(
            || idx.saturating_add(1).to_string(),
            |layer| ms_i18n::lookup(layer.label_key).unwrap_or(layer.label_key).to_string(),
        )
    }

    /// Whether the frame has an anchor and a rectangle, i.e. whether it can be drawn.
    #[must_use]
    pub fn is_placed(&self) -> bool {
        self.page_idx.is_some() && self.rect_px.is_some()
    }

    #[must_use]
    pub fn page_idx(&self) -> Option<usize> {
        self.page_idx
    }

    /// The frame rectangle in SOURCE PAGE PIXELS — the units
    /// `CanvasView::replace_overlay_region_px` consumes.
    #[must_use]
    pub fn rect_px(&self) -> Option<OverlayRectPx> {
        self.rect_px
    }

    /// The lock the frame is under, derived from its contents (D4).
    #[must_use]
    pub fn lock(&self) -> FrameLock {
        derive_lock(self.processing, self.result.is_some(), self.masks.is_empty(), self.last_paint_px.is_some())
    }

    /// The colour state, i.e. the lock combined with the size check (D6).
    #[must_use]
    pub fn visual(&self) -> FrameVisual {
        derive_visual(self.lock(), self.size_violation())
    }

    /// How the current size fails the active constraints, or `None` while it satisfies them.
    /// An unplaced frame has no size and therefore no violation.
    #[must_use]
    pub fn size_violation(&self) -> Option<SizeViolation> {
        let rect = self.rect_px?;
        geometry::check_size(rect.w, rect.h, &self.constraints)
    }

    /// Which of the four actions are available right now.
    #[must_use]
    pub fn buttons(&self) -> FrameButtons {
        derive_buttons(self.lock(), self.size_violation(), self.masks.is_empty(), self.allows_empty_mask)
    }

    #[must_use]
    pub fn masks(&self) -> &MaskStack {
        &self.masks
    }

    pub fn masks_mut(&mut self) -> &mut MaskStack {
        &mut self.masks
    }

    #[must_use]
    pub fn result(&self) -> Option<&ResultLayer> {
        self.result.as_ref()
    }

    /// Stores or drops the pending processed result.
    ///
    /// The frame does NOT validate the result's size against `rect_px`: applying it is the
    /// tool's job and so is that check (D7) — `replace_overlay_region_px` would silently
    /// rescale a mismatched image.
    pub fn set_result(&mut self, result: Option<ResultLayer>) {
        self.result = result;
    }

    /// Marks the frame as being processed by a consumer, which locks it (D4).
    pub fn set_processing(&mut self, on: bool) {
        self.processing = on;
        if on {
            // A run starts from the mask as it is; a stroke may not continue into it.
            self.last_paint_px = None;
        }
    }

    /// The brush the frame paints its mask with. The host tool routes wheel events into
    /// `MaskBrush::handle_wheel` through this, so the radius policy lives in ONE place.
    pub fn brush_mut(&mut self) -> &mut MaskBrush {
        &mut self.brush
    }

    /// Whether a primary-button stroke currently ERASES instead of painting.
    #[must_use]
    pub fn erase(&self) -> bool {
        self.erase
    }

    /// Sets the primary-button stroke mode. The secondary button erases whatever this says,
    /// so clearing it never takes the "undo a stray stroke" gesture away.
    pub fn set_erase(&mut self, erase: bool) {
        self.erase = erase;
    }

    /// Queues a «Обработать» press made in a dock panel.
    ///
    /// The request is honoured by the next `update` only if `buttons().process` still allows
    /// it, so a panel cannot start a run the frame refuses.
    pub fn request_process(&mut self) {
        self.pending.process = true;
    }

    /// Queues a «Применить» press made in a dock panel.
    ///
    /// The frame's own button row is not the only way to resolve a pending result: at a low
    /// zoom that row is a few points wide, and a result-pending frame is LOCKED, so a user who
    /// could not reach it would have no way out at all. Both surfaces route through the same
    /// `FrameOutcome` and the same `buttons()` table.
    pub fn request_apply(&mut self) {
        self.pending.apply = true;
    }

    /// Queues a «Отменить» press made in a dock panel. Same reason and same re-check as
    /// `request_apply`.
    pub fn request_cancel(&mut self) {
        self.pending.cancel = true;
    }

    /// Releases the frame: clears every mask layer, drops the pending result and the
    /// processing flag. The PLACEMENT is kept — after applying a result the user usually
    /// wants to move the same frame on, not to hunt for it again.
    pub fn reset(&mut self) {
        self.masks.clear_all();
        self.result = None;
        self.processing = false;
        self.cancel_gestures();
        self.pending = PendingRequests::default();
    }

    /// Ends every gesture in flight, so no drag or stroke survives into a pass that cannot
    /// continue it (a canvas without a layout, or a frame whose page vanished).
    fn cancel_gestures(&mut self) {
        self.drag = None;
        self.last_paint_px = None;
        // A hold is a gesture too: nothing may keep hiding a result the frame no longer draws.
        self.compare_held = false;
    }

    /// Whether a gesture of the frame is in flight: a move or resize drag, or a paint/erase
    /// stroke whose button is still down.
    ///
    /// The host tool reports this through `block_canvas_drag_scroll_on_primary`, which is the
    /// precise alternative to `block_canvas_zoom` (D5): the canvas keeps its zoom and its
    /// undo shortcuts for the whole editing session. A stroke counts because it is exactly as
    /// much a live gesture as a drag — scrolling the page out from under it would break it in
    /// the same way.
    #[must_use]
    pub fn drag_active(&self) -> bool {
        self.drag.is_some() || self.last_paint_px.is_some()
    }

    /// Whether `pos` (screen points) lies on the frame's hitbox — the frame plus its top
    /// strip and its two rows below.
    ///
    /// Answers from the LAST drawn pass, which is one frame behind when the tab asks before
    /// `draw_overlay_ui`; the frame's `Order::Middle` area covers the same point through
    /// egui's own z-order test, so a first frame is not left unguarded.
    #[must_use]
    pub fn captures_pointer(&self, pos: Pos2) -> bool {
        self.hitbox.is_some_and(|rect| rect.contains(pos))
    }

    /// The localized status line: what the frame is doing, or what stops it.
    #[must_use]
    pub fn status_text(&self) -> String {
        if !self.is_placed() {
            return t!("cleaning.region_frame.status.unplaced").to_string();
        }
        let lock = self.lock();
        if self.size_violation().is_some() {
            // D6: a locked frame that became invalid must say that it has to be released
            // first, because resizing it is exactly what the lock forbids.
            return if lock.is_free() {
                t!("cleaning.region_frame.status.invalid_size").to_string()
            } else {
                t!("cleaning.region_frame.status.invalid_size_locked").to_string()
            };
        }
        match lock {
            FrameLock::Free => t!("cleaning.region_frame.status.free").to_string(),
            FrameLock::MaskPainted => t!("cleaning.region_frame.status.mask_painted").to_string(),
            FrameLock::ResultPending => t!("cleaning.region_frame.status.result_pending").to_string(),
            FrameLock::Processing => t!("cleaning.region_frame.status.processing").to_string(),
        }
    }

    /// Whether a stroke may reach the mask right now.
    ///
    /// Painting is refused while a consumer is running and while a result waits to be applied
    /// or dropped: in both states the mask describes work already handed over, and editing it
    /// would make the pending result describe a mask that no longer exists.
    #[must_use]
    fn mask_paintable(&self) -> bool {
        matches!(self.lock(), FrameLock::Free | FrameLock::MaskPainted)
    }

    /// Where page `page_idx` sits this frame and how large it is in SOURCE PIXELS.
    ///
    /// The overlay image is authoritative for the size when one is allocated for the page.
    /// Otherwise it is re-derived from the page's screen rect and the zoom, because the
    /// canvas lays pages out as `screen = source_px * zoom + translation` — the same
    /// derivation `CanvasView::page_source_size_from_scene` makes from its private world
    /// rects. `None` when the canvas has not laid the page out yet.
    #[must_use]
    fn page_placement(canvas: &CanvasView, page_idx: usize, zoom: f32) -> Option<PagePlacement> {
        let screen = canvas.page_scene_rect(page_idx)?;
        if let Some([w, h]) = canvas.overlay_size(page_idx) {
            return Some(PagePlacement { screen, w, h, zoom });
        }
        if !zoom.is_finite() || zoom <= 0.0 {
            return None;
        }
        let w = clamp_px((screen.width() / zoom).round(), usize::MAX).max(1);
        let h = clamp_px((screen.height() / zoom).round(), usize::MAX).max(1);
        Some(PagePlacement { screen, w, h, zoom })
    }

    /// Places an unplaced frame on the canvas' current page, centred in the usable viewport.
    ///
    /// Returns `false` when the canvas cannot say where that page is yet, in which case the
    /// caller skips the whole pass and tries again next frame.
    fn ensure_placed(&mut self, canvas: &CanvasView, usable: Rect, zoom: f32) -> bool {
        if self.is_placed() {
            return true;
        }
        let page_idx = canvas.current_page_idx();
        let Some(page) = Self::page_placement(canvas, page_idx, zoom) else {
            return false;
        };
        let (w, h) = nearest_valid_size(
            DEFAULT_FRAME_SIDE_PX.min(page.w),
            DEFAULT_FRAME_SIDE_PX.min(page.h),
            &self.constraints,
        );
        let (w, h) = (w.min(page.w).max(1), h.min(page.h).max(1));

        // Centre on the visible part of the page when there is one, so a frame spawned while
        // the page is half scrolled away still appears where the user is looking.
        let visible = page.screen.intersect(usable);
        let anchor = if visible.is_positive() { visible.center() } else { page.screen.center() };
        let (cx, cy) = screen_pos_to_page_px(page.screen, zoom, anchor, page.w, page.h);
        let rect = OverlayRectPx {
            x: cx.saturating_sub(w / 2).min(page.w.saturating_sub(w)),
            y: cy.saturating_sub(h / 2).min(page.h.saturating_sub(h)),
            w,
            h,
        };
        self.page_idx = Some(page_idx);
        self.rect_px = Some(rect);
        self.masks.resize(w, h);
        true
    }

    /// Re-anchors the frame to `next_page`, keeping its size and its screen position as far
    /// as the new page allows. Called only for a free frame, so clearing the mask stack on a
    /// geometry change cannot lose work (D4).
    fn move_to_page(&mut self, canvas: &CanvasView, next_page: usize, frame_screen: Rect, zoom: f32) {
        let Some(rect_px) = self.rect_px else {
            return;
        };
        let Some(page) = Self::page_placement(canvas, next_page, zoom) else {
            return;
        };
        let (x, y) = screen_pos_to_page_px(page.screen, zoom, frame_screen.min, page.w, page.h);
        let w = rect_px.w.min(page.w).max(1);
        let h = rect_px.h.min(page.h).max(1);
        self.page_idx = Some(next_page);
        self.rect_px = Some(OverlayRectPx {
            x: x.min(page.w.saturating_sub(w)),
            y: y.min(page.h.saturating_sub(h)),
            w,
            h,
        });
        if (w, h) != (rect_px.w, rect_px.h) {
            self.masks.resize(w, h);
        }
    }

    /// The page transition rule of the design, fed with every page the project has.
    ///
    /// Enumerating all pages is cheap — `page_scene_rect` is a vector lookup — and it is what
    /// makes the rule order-independent: `choose_page` picks the largest visible candidate
    /// itself.
    #[must_use]
    fn page_choice(canvas: &CanvasView, page_idx: usize, page_screen: Rect, frame_size: Vec2, usable: Rect, page_count: usize) -> PageChoice {
        let current = PageView { page_idx, visible: page_screen.intersect(usable) };
        let candidates: Vec<PageView> = (0..page_count)
            .filter(|idx| *idx != page_idx)
            .filter_map(|idx| {
                let rect = canvas.page_scene_rect(idx)?;
                Some(PageView { page_idx: idx, visible: rect.intersect(usable) })
            })
            .collect();
        geometry::choose_page(&current, frame_size, &candidates)
    }

    /// The whole per-frame pass: anchor, keep in view, sense, paint, and report intent.
    ///
    /// `canvas` is borrowed SHARED on purpose — the frame decides, the tool acts. Run this
    /// from `CleaningTool::draw_overlay_ui`, which is the only hook that owns the context,
    /// the canvas and the project at once.
    #[must_use]
    pub fn update(&mut self, ctx: &egui::Context, canvas: &CanvasView, host: FrameHost<'_>) -> FrameOutcome {
        let mut outcome = FrameOutcome::default();
        self.hitbox = None;

        // 0. Dock-panel requests are folded FIRST, so they survive every early return below.
        //    A locked frame may be scrolled off-screen, and its panel is then the only
        //    surface that can resolve it — dropping the request there would strand the frame.
        let pending = std::mem::take(&mut self.pending);
        fold_pending(pending, self.buttons(), &mut outcome);

        let Some(viewport) = canvas.visible_scene_rect() else {
            // The canvas has not laid out yet; a gesture cannot be continued against geometry
            // that does not exist.
            self.cancel_gestures();
            return outcome;
        };
        let zoom = canvas.zoom();
        // A frame that has not been placed yet has no hitbox to measure the panels against,
        // so the viewport itself stands in for one: every overlapping panel then lands in the
        // both-axes branch of the cut rule and is charged by least area, which is the
        // conservative reading and still order-independent.
        let placement_usable = geometry::usable_viewport_for(viewport, viewport, host.panel_rects);
        if !self.ensure_placed(canvas, placement_usable, zoom) {
            self.cancel_gestures();
            return outcome;
        }
        let Some(anchor) = self.resolve_anchor(canvas, zoom) else {
            self.cancel_gestures();
            return outcome;
        };
        let lock = self.lock();
        let frame_screen = anchor.page.screen_rect(anchor.rect_px);
        // The dock panels are cut RELATIVE to the frame's own hitbox, so a right-docked panel
        // that happens to start near the top of the viewport costs the frame the right edge
        // and never a full-width band across the top. Derived from the hitbox the frame has
        // right now and deliberately not recomputed after the page transition below: a
        // transition preserves the frame's screen position, and the next pass re-derives this
        // anyway — the same per-frame convergence the keep-in-view clamp already relies on.
        let usable = geometry::usable_viewport_for(hitbox_rect(frame_screen, &chrome()), viewport, host.panel_rects);

        // 1. Page transition — only for a free frame, and only while nothing is being
        //    dragged: re-anchoring under a live drag would move the frame out from under the
        //    pointer. Re-resolving afterwards is what keeps the rest of the pass working on
        //    the NEW page's geometry rather than one frame behind it.
        let anchor = if lock.is_free()
            && self.drag.is_none()
            && let PageChoice::MoveTo(next) =
                Self::page_choice(canvas, anchor.page_idx, anchor.page.screen, frame_screen.size(), usable, host.page_count)
        {
            self.move_to_page(canvas, next, frame_screen, zoom);
            let Some(moved) = self.resolve_anchor(canvas, zoom) else {
                self.cancel_gestures();
                return outcome;
            };
            moved
        } else {
            anchor
        };

        // 2. Keep-in-view (D3). The same call also implements "manual dragging stops at the
        //    viewport border"; there is no second clamp for dragging anywhere.
        let rect_px = keep_in_view_px(lock, anchor.rect_px, &anchor.page, usable);
        self.rect_px = Some(rect_px);
        let frame_screen = anchor.page.screen_rect(rect_px);

        let hitbox = hitbox_rect(frame_screen, &chrome());
        // 3. Off-screen: only a locked frame can get here (a free one was just pulled back),
        //    and it gets an arrow instead of a chrome nobody could reach.
        if !hitbox.intersect(usable).is_positive() {
            if let Some(arrow) = geometry::offscreen_arrow(frame_screen, usable) {
                let painter = ctx
                    .layer_painter(egui::LayerId::new(egui::Order::Middle, Id::new((FRAME_AREA_ID, "arrow"))))
                    .with_clip_rect(usable);
                render::paint_offscreen_arrow(&painter, &arrow, self.visual());
            }
            // Nothing can be sensed this frame, so no gesture can continue either.
            self.cancel_gestures();
            return outcome;
        }

        // 4. The interactive pass. ONE area, sized to the hitbox (never to the viewport) and
        //    on `Order::Middle` so the dock panels on `Order::Foreground` stay above it.
        let area = egui::Area::new(Id::new(FRAME_AREA_ID))
            .order(egui::Order::Middle)
            .fixed_pos(hitbox.min)
            .constrain(false)
            .movable(false)
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_clip_rect(usable);
                self.pass(ui, PassCtx { page: anchor.page, usable, hitbox, frame_screen, lock })
            });
        // 5. The frame's own chrome asks for the same actions as the dock panel, so the two
        //    are merged rather than one replacing the other.
        outcome.merge(area.inner);
        outcome
    }

    /// Everything the pass needs about the page the frame is anchored to, this frame.
    ///
    /// `None` when the frame is not placed, when the canvas cannot say where its page is, or
    /// when the page has no usable size — in each case the caller skips the pass entirely
    /// rather than deriving a screen rect from missing geometry.
    #[must_use]
    fn resolve_anchor(&self, canvas: &CanvasView, zoom: f32) -> Option<PageAnchor> {
        let page_idx = self.page_idx?;
        let rect_px = self.rect_px?;
        let page = Self::page_placement(canvas, page_idx, zoom)?;
        Some(PageAnchor { page_idx, rect_px, page })
    }

    /// Senses the frame's widgets, applies what they did and paints the result.
    fn pass(&mut self, ui: &mut egui::Ui, cx: PassCtx) -> FrameOutcome {
        let mut outcome = FrameOutcome::default();
        let strip = Rect::from_min_max(cx.hitbox.min, pos2(cx.hitbox.max.x, cx.hitbox.min.y + TOP_STRIP_H));

        // A hover sensor over the whole hitbox gives the area its size (and therefore its
        // entry in egui's layer hit-test) and swallows hover that would otherwise reach the
        // canvas between the widgets. Allocated FIRST: a later allocation wins the pointer,
        // so every real control below stays reachable (`egui-docs/06-overlays.md` §6). Its
        // `Response` is also the frame's own hover test for the brush gestures — a widget is
        // only hidden from `contains_pointer` by a covering widget on ANOTHER layer, so this
        // sensor still reports the pointer that the body or a handle went on to claim.
        let over_frame = ui.allocate_rect(cx.hitbox, Sense::hover()).contains_pointer();
        self.handle_brush_gestures(ui, over_frame);

        self.sense_strip_drag(ui, strip);
        if cx.lock.is_free() {
            self.sense_handles(ui, cx.frame_screen);
        }
        let body = ui.interact(cx.frame_screen, Id::new((FRAME_AREA_ID, "body")), Sense::click_and_drag());
        self.sense_mask_painting(ui, &body, cx.frame_screen);
        // The pointer of a drag already claimed through a `Response`: the occlusion test ran
        // when the drag started, so following it raw afterwards is what egui itself does.
        let pointer = ui.ctx().input(|i| i.pointer.interact_pos());
        self.apply_drag(pointer, cx);

        // The drag may have moved or resized the frame; everything below is painted at the
        // rectangle it settled on, so the frame follows the pointer without a frame of lag.
        let Some(rect_px) = self.rect_px else {
            return outcome;
        };
        let frame_screen = cx.page.screen_rect(rect_px);
        let hitbox = hitbox_rect(frame_screen, &chrome());
        self.hitbox = Some(hitbox.intersect(cx.usable));

        let lock = self.lock();
        let visual = derive_visual(lock, self.size_violation());
        // The button row is sensed BEFORE the contents are painted, because «Сравнить» is a
        // MOMENTARY hold: the frame must react in the very frame the press arrives, or the
        // result would stay visible until the next unrelated input event repaints the canvas.
        // The rows lie strictly below the frame body and below the bottom handles, so drawing
        // them first changes neither the picture nor any widget's pointer claim.
        self.draw_rows(ui, hitbox, frame_screen.bottom(), visual, &mut outcome);
        self.paint_contents(ui, frame_screen, lock);
        render::paint_frame_border(ui.painter(), frame_screen, visual);
        render::paint_handles(ui.painter(), frame_screen, visual, lock.is_free());

        let strip = Rect::from_min_max(hitbox.min, pos2(hitbox.max.x, hitbox.min.y + TOP_STRIP_H));
        self.draw_strip(ui, strip);
        // Last, so the ring sits above the mask previews, the border and the chrome.
        self.paint_brush_cursor(ui, &body, frame_screen, rect_px);
        outcome
    }

    /// Paints the mask layers and, on top of them, the pending result (§6 of the design:
    /// index order, the result last).
    ///
    /// The result is SKIPPED while «Сравнить» is held (`result_hidden`), which is the whole
    /// point of that button: the user sees the original pixels under the pending result. The
    /// mask layers keep drawing in both states — they are the user's own marking, equally
    /// present before and after the run, and blinking them would disturb the comparison.
    fn paint_contents(&mut self, ui: &mut egui::Ui, frame_screen: Rect, lock: FrameLock) {
        let ctx = ui.ctx().clone();
        self.masks.ensure_textures(&ctx);
        self.masks.draw(ui.painter(), frame_screen);
        let hidden = result_hidden(self.buttons(), self.compare_held);
        if let Some(result) = self.result.as_mut()
            && !hidden
        {
            result.ensure_texture(&ctx);
            result.draw(ui.painter(), frame_screen);
        }
        if matches!(lock, FrameLock::Processing) {
            render::paint_processing_scrim(ui.painter(), frame_screen);
        }
    }

    /// Senses the drag grip of the top strip and starts / ends a move drag.
    fn sense_strip_drag(&mut self, ui: &mut egui::Ui, strip: Rect) {
        let grip = self.grip_rect(strip);
        let response = ui.interact(grip, Id::new((FRAME_AREA_ID, "grip")), Sense::click_and_drag());
        if response.drag_started()
            && self.lock().is_free()
            && let (Some(origin), Some(start)) = (response.interact_pointer_pos(), self.rect_px)
        {
            self.drag = Some(DragState { kind: DragKind::Move, origin, start });
        }
        if response.drag_stopped() && matches!(self.drag.map(|d| d.kind), Some(DragKind::Move)) {
            self.drag = None;
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Move);
        }
    }

    /// Senses the eight resize handles and starts / ends a resize drag.
    ///
    /// A corner handle occupies an L-shaped area — the three quadrants of its box that lie
    /// outside the frame — which no single rectangle describes, so it is sensed through TWO
    /// `ui.interact` calls with distinct ids. Both drive the SAME handle: the drag state is
    /// keyed by `HandleKind`, never by the rectangle the pointer happened to grab, so which
    /// of the two `Response`s reports the gesture makes no difference to what it does.
    fn sense_handles(&mut self, ui: &mut egui::Ui, frame_screen: Rect) {
        let points = handle_points(frame_screen);
        for (handle, point) in HandleKind::ALL.into_iter().zip(points) {
            let (primary, secondary) = handle_hit_rects(point, handle);
            let response = ui.interact(primary, Id::new((FRAME_AREA_ID, "handle", handle.id_suffix())), Sense::click_and_drag());
            // The id must differ from the first one: two widgets sharing an id in the same
            // frame collide in egui's widget map and only one of them keeps its interaction.
            let corner = secondary
                .map(|rect| ui.interact(rect, Id::new((FRAME_AREA_ID, "handle_corner", handle.id_suffix())), Sense::click_and_drag()));

            let started = response.drag_started() || corner.as_ref().is_some_and(egui::Response::drag_started);
            let origin = response
                .interact_pointer_pos()
                .or_else(|| corner.as_ref().and_then(egui::Response::interact_pointer_pos));
            if started
                && let (Some(origin), Some(start)) = (origin, self.rect_px)
            {
                self.drag = Some(DragState { kind: DragKind::Resize(handle), origin, start });
            }
            let stopped = response.drag_stopped() || corner.as_ref().is_some_and(egui::Response::drag_stopped);
            if stopped && matches!(self.drag.map(|d| d.kind), Some(DragKind::Resize(_))) {
                self.drag = None;
            }
        }
    }

    /// Applies the live drag to `rect_px`, measured from the anchor captured when it started.
    ///
    /// A resize re-sizes the mask stack, which CLEARS it — legal only because a non-empty
    /// stack locks the frame and no drag can start on a locked one (D4).
    fn apply_drag(&mut self, pointer: Option<Pos2>, cx: PassCtx) {
        let Some(drag) = self.drag else {
            return;
        };
        let Some(pointer) = pointer else {
            return;
        };
        if !self.lock().is_free() {
            self.drag = None;
            return;
        }
        let delta = screen_delta_to_px(pointer - drag.origin, cx.page.zoom);
        let next = match drag.kind {
            DragKind::Move => moved_rect_px(drag.start, delta, cx.page.w, cx.page.h),
            DragKind::Resize(handle) => resized_rect_px(drag.start, handle, delta, cx.page.w, cx.page.h, &self.constraints),
        };
        // The same per-frame clamp as everywhere else: this is what makes a manual drag stop
        // at the viewport border, and there is deliberately no second clamp for it (D3).
        let next = keep_in_view_px(FrameLock::Free, next, &cx.page, cx.usable);
        if self.masks.size() != (next.w, next.h) {
            self.masks.resize(next.w, next.h);
        }
        self.rect_px = Some(next);
    }

    /// Paints a stroke into the active mask layer while the body is being dragged.
    ///
    /// The right mouse button always erases, whatever mode a panel offers — the one gesture
    /// users expect for undoing a stray stroke (the same rule the FLUX.2 klein engine follows,
    /// `tools/ai_editor/engines/flux2_klein/`).
    fn sense_mask_painting(&mut self, ui: &mut egui::Ui, body: &egui::Response, frame_screen: Rect) {
        let Some(rect_px) = self.rect_px else {
            return;
        };
        if !self.mask_paintable() || self.drag.is_some() {
            self.last_paint_px = None;
            return;
        }
        let (primary, secondary, mods, z_down) = ui.ctx().input(|i| {
            (
                i.pointer.primary_down(),
                i.pointer.secondary_down(),
                i.modifiers,
                i.key_down(egui::Key::Z),
            )
        });
        // Ctrl / Cmd / Z are the canvas' zoom modifiers, and Ctrl+drag over the frame zooms the
        // page (`canvas/mod.rs::handle_shortcuts` tests only `canvas_rect`, not occlusion). A
        // zoom gesture must not leave a stroke behind on the mask, so painting is suppressed
        // exactly as the region editor suppresses it.
        if mods.ctrl || mods.command || z_down {
            self.last_paint_px = None;
            return;
        }
        // `interact_pointer_pos` is `Some` only while EGUI decided this widget owns the
        // pointer, so the occlusion test has already run — this is not a raw pointer read.
        let Some(pointer) = body.interact_pointer_pos() else {
            self.last_paint_px = None;
            return;
        };
        if !(primary || secondary) {
            self.last_paint_px = None;
            return;
        }
        let to = super::super::base::scene_pointer_to_image_px(pointer, frame_screen, [rect_px.w, rect_px.h]);
        let from = self.last_paint_px.unwrap_or(to);
        if self.last_paint_px.is_none() {
            // One snapshot per stroke, taken before the first segment reaches the buffer.
            self.masks.push_undo();
        }
        let erase = stroke_erases(primary, secondary, mods.shift, self.erase);
        self.masks.paint_segment(from, to, self.brush.radius_px(), erase);
        self.last_paint_px = Some(to);
        ui.ctx().request_repaint();
    }

    /// Applies the two brush-size gestures of the region editor while the pointer is over the
    /// frame: the `-` / `=` / `+` shortcuts and `Shift`+wheel.
    ///
    /// Both bindings are `MaskBrush`'s own, reused rather than reimplemented, and both are
    /// handled HERE rather than only through `CleaningTool::on_key_event` / `on_wheel_event`:
    /// `tab.rs` refuses to deliver either while the canvas pointer is occluded, and the frame
    /// occludes it over its own hitbox (`captures_pointer`). The host tool keeps those two
    /// hooks for the pointer OUTSIDE the frame, so between them the whole canvas is covered.
    ///
    /// `over_frame` must come from a `Response`, never from a raw pointer test. A change asks
    /// for a repaint: the brush ring is drawn from the radius, so without one the new size
    /// would stay invisible until something unrelated redrew the frame.
    fn handle_brush_gestures(&mut self, ui: &mut egui::Ui, over_frame: bool) {
        if !over_frame {
            return;
        }
        let (mods, z_down, scroll) = ui
            .ctx()
            .input(|i| (i.modifiers, i.key_down(egui::Key::Z), i.smooth_scroll_delta));
        // The canvas' zoom modifiers win over the brush: `canvas/mod.rs` consumes Ctrl + `-` /
        // `=` for zooming and egui diverts Ctrl+wheel into a zoom delta, so answering either
        // here would resize the brush behind a zoom the user asked for.
        if mods.ctrl || mods.command || z_down {
            return;
        }
        let mut changed = self.brush.handle_size_shortcuts(ui.ctx());
        // With Shift some backends remap the wheel into horizontal scrolling, so fall back to
        // the X component. The delta is tested before the call because `handle_wheel` answers
        // "handled" for every Shift-held frame, and repainting on that alone would spin the
        // canvas at full frame rate for as long as the key is merely held.
        let mut wheel = scroll.y;
        if wheel.abs() <= f32::EPSILON {
            wheel = scroll.x;
        }
        if wheel.abs() > f32::EPSILON && self.brush.handle_wheel(wheel, mods) {
            changed = true;
        }
        if changed {
            ui.ctx().request_repaint();
        }
    }

    /// Paints the brush ring over the frame body, above everything else the pass drew.
    ///
    /// It lives here and not in `CleaningTool::draw_cursor` because
    /// `tab.rs::draw_active_tool_cursor` refuses to draw a tool cursor while the canvas
    /// pointer is occluded, and the frame occludes it over its own hitbox — a ring requested
    /// from that hook could therefore never appear over the one rectangle it belongs to.
    /// Nothing is drawn while a stroke could not reach the mask, so the ring never advertises
    /// an edit the frame would refuse.
    fn paint_brush_cursor(&self, ui: &mut egui::Ui, body: &egui::Response, frame_screen: Rect, rect_px: OverlayRectPx) {
        if !self.mask_paintable() || !body.contains_pointer() {
            return;
        }
        // The hover DECISION came from the `Response` above; this reads only the position to
        // draw at, and `interact_pos` is what keeps the ring under the pointer during a stroke.
        let Some(pointer) = ui
            .ctx()
            .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()))
        else {
            return;
        };
        self.brush.draw_circle_cursor_on_image(ui, frame_screen, [rect_px.w, rect_px.h], pointer);
    }

    /// Rect of the drag grip: the strip minus the mask-layer chips on its right.
    #[must_use]
    fn grip_rect(&self, strip: Rect) -> Rect {
        let count = self.masks.layer_count();
        if count == 0 {
            return strip;
        }
        let leftmost = render::layer_chip_rect(strip, 0, count);
        Rect::from_min_max(strip.min, pos2(leftmost.left().max(strip.left()) - 2.0, strip.max.y)).intersect(strip)
    }

    /// Paints the top strip and senses its mask-layer chips.
    fn draw_strip(&mut self, ui: &mut egui::Ui, strip: Rect) {
        render::paint_row_background(ui.painter(), strip);
        render::paint_grip(ui.painter(), self.grip_rect(strip), matches!(self.drag.map(|d| d.kind), Some(DragKind::Move)));

        let count = self.masks.layer_count();
        let active = self.masks.active();
        for idx in 0..count {
            let rect = render::layer_chip_rect(strip, idx, count);
            // The tooltip names the layer the ACTIVE consumer declared, so a chip does not stay
            // an anonymous coloured square when the consumer changes what its layers mean.
            let response = ui
                .interact(rect, Id::new((FRAME_AREA_ID, "layer", idx)), Sense::click())
                .on_hover_text(tf!("cleaning.region_frame.layer_tooltip", name = self.layer_label(idx)));
            let tint = self.layers.get(idx).map_or(Color32::GRAY, |layer| layer.tint);
            render::paint_layer_chip(ui.painter(), rect, idx.saturating_add(1), idx == active, tint);
            if response.clicked() {
                self.masks.set_active(idx);
            }
        }
    }

    /// Paints the button row and the status line below the frame, records what was pressed,
    /// and settles `compare_held` for this frame.
    ///
    /// The rows take their horizontal extent from the HITBOX, not from the frame's screen
    /// rect: the hitbox is the frame widened to `CHROME_MIN_W` when the zoom made the frame
    /// narrower than its own chrome, and it is also what the keep-in-view clamp holds inside
    /// the viewport, so laying the rows out anywhere else would put them where nothing
    /// guarantees they are reachable.
    ///
    /// Runs BEFORE `paint_contents` in the pass: «Сравнить» is a momentary hold and its effect
    /// on the result layer must land in the same frame as the press.
    fn draw_rows(&mut self, ui: &mut egui::Ui, hitbox: Rect, frame_bottom: f32, visual: FrameVisual, outcome: &mut FrameOutcome) {
        // Measured from the outer edge of the bottom handles, not from the frame itself, or
        // the button row would be painted over the two corner discs and the bottom midpoint one.
        let buttons_top = frame_bottom + HANDLE_RADIUS + CHROME_GAP;
        let buttons = Rect::from_min_max(pos2(hitbox.left(), buttons_top), pos2(hitbox.right(), buttons_top + BUTTONS_H));
        let status_top = buttons.bottom() + CHROME_GAP;
        let status = Rect::from_min_max(pos2(hitbox.left(), status_top), pos2(hitbox.right(), status_top + STATUS_H));

        render::paint_row_background(ui.painter(), buttons);
        render::paint_row_background(ui.painter(), status);
        render::paint_status_text(ui.painter(), status, &self.status_text(), visual);

        let enabled = self.buttons();
        // «Сравнить» sits next to «Применить»: both act on the pending result, and the two are
        // enabled by exactly the same condition.
        let slots = split_row(buttons, 4);
        if chrome_button(ui, slots[0], "apply", t!("cleaning.region_frame.button.apply"), enabled.apply).clicked() {
            outcome.apply_requested = true;
        }
        let compare = chrome_button(ui, slots[1], "compare", t!("cleaning.region_frame.button.compare"), enabled.compare)
            .on_hover_text(t!("cleaning.region_frame.button.compare_tooltip"));
        // Recomputed from scratch, never toggled: the hold ends by itself when the pointer
        // button is released, and ANDing it with the enablement means applying or cancelling
        // the result cannot strand the frame with an invisible one.
        self.compare_held = enabled.compare && compare.is_pointer_button_down_on();
        if chrome_button(ui, slots[2], "cancel", t!("cleaning.region_frame.button.cancel"), enabled.cancel).clicked() {
            outcome.cancel_requested = true;
        }
        if chrome_button(ui, slots[3], "clear_mask", t!("cleaning.region_frame.button.clear_mask"), enabled.clear_mask).clicked() {
            outcome.clear_mask_requested = true;
        }
    }
}

/// The page the frame is anchored to, resolved for one frame.
#[derive(Debug, Clone, Copy)]
struct PageAnchor {
    page_idx: usize,
    rect_px: OverlayRectPx,
    page: PagePlacement,
}

/// The per-pass geometry `RegionFrame::pass` and its helpers share.
///
/// A plain bundle rather than a growing argument list; every field is settled before the
/// pass starts and none of them changes inside it.
#[derive(Debug, Clone, Copy)]
struct PassCtx {
    page: PagePlacement,
    usable: Rect,
    hitbox: Rect,
    frame_screen: Rect,
    lock: FrameLock,
}

/// Splits a row into `count` equally wide slots separated by `BUTTON_GAP`.
///
/// `count` is a small literal at every call site; a zero would divide by zero, so it is
/// floored at one and yields the whole row.
#[must_use]
fn split_row(row: Rect, count: usize) -> Vec<Rect> {
    let count = count.max(1);
    let slots = f32::from(u16::try_from(count).unwrap_or(u16::MAX));
    let width = ((row.width() - BUTTON_GAP * (slots - 1.0)) / slots).max(1.0);
    (0..count)
        .map(|idx| {
            let step = f32::from(u16::try_from(idx).unwrap_or(u16::MAX));
            let left = row.left() + step * (width + BUTTON_GAP);
            Rect::from_min_size(pos2(left, row.top()), vec2(width, row.height()))
        })
        .collect()
}

/// One button of the chrome row. Returns the button's own `Response`.
///
/// The `Response` rather than a `bool`, because the row holds both CLICK buttons (`.clicked()`)
/// and the momentary «Сравнить» hold (`.is_pointer_button_down_on()`): a second, subtly
/// different button construction living next to this one would eventually drift from it in the
/// disabled scope, the id salt, the truncation or the minimum size.
///
/// `id_suffix` is a non-localized literal so the widget id survives a language switch; the
/// caption is the localized text and is never an id source. The caption is TRUNCATED to the
/// slot: `CHROME_MIN_W` is sized to hold the four Russian and English chrome captions, but a
/// longer translation must be elided rather than painted over the neighbouring button.
#[must_use]
fn chrome_button(ui: &mut egui::Ui, rect: Rect, id_suffix: &'static str, label: &str, enabled: bool) -> egui::Response {
    let mut builder = egui::UiBuilder::new().max_rect(rect).id_salt((FRAME_AREA_ID, id_suffix));
    if !enabled {
        builder = builder.disabled();
    }
    ui.scope_builder(builder, |ui| ui.put(rect, egui::Button::new(label).truncate().min_size(rect.size())))
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_constraints() -> FrameConstraints {
        FrameConstraints { multiple: 1, min_side: 1, max_area: None, max_aspect: None }
    }

    /// One layer spec per tint, all naming the same catalog key: the tests care about the
    /// layer COUNT and the tints, never about the labels.
    fn test_layers(tints: &[Color32]) -> Vec<MaskLayerSpec> {
        tints
            .iter()
            .map(|tint| MaskLayerSpec { tint: *tint, label_key: "cleaning.region_frame.status.free" })
            .collect()
    }

    fn rect_px(x: usize, y: usize, w: usize, h: usize) -> OverlayRectPx {
        OverlayRectPx { x, y, w, h }
    }

    fn screen(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::from_min_max(pos2(x0, y0), pos2(x1, y1))
    }

    // -----------------------------------------------------------------------------------
    // Lock derivation
    // -----------------------------------------------------------------------------------

    #[test]
    fn lock_is_free_only_when_nothing_is_held() {
        assert_eq!(derive_lock(false, false, true, false), FrameLock::Free);
        assert_eq!(derive_lock(false, false, false, false), FrameLock::MaskPainted);
        assert_eq!(derive_lock(false, true, true, false), FrameLock::ResultPending);
        assert_eq!(derive_lock(true, false, true, false), FrameLock::Processing);
    }

    #[test]
    fn lock_precedence_is_processing_result_mask() {
        // Every combination that could hold two reasons at once must report the strongest.
        assert_eq!(derive_lock(true, true, false, false), FrameLock::Processing);
        assert_eq!(derive_lock(true, false, false, false), FrameLock::Processing);
        assert_eq!(derive_lock(true, true, true, false), FrameLock::Processing);
        assert_eq!(derive_lock(false, true, false, false), FrameLock::ResultPending);
    }

    #[test]
    fn a_stroke_in_flight_locks_an_empty_mask() {
        // The button is still down, so the frame must stay held even with nothing painted.
        assert_eq!(derive_lock(false, false, true, true), FrameLock::MaskPainted);
        // The stronger reasons still win over it.
        assert_eq!(derive_lock(true, false, true, true), FrameLock::Processing);
        assert_eq!(derive_lock(false, true, true, true), FrameLock::ResultPending);
    }

    // -----------------------------------------------------------------------------------
    // Visual selection (D6)
    // -----------------------------------------------------------------------------------

    #[test]
    fn visual_follows_the_lock_when_the_size_is_valid() {
        assert_eq!(derive_visual(FrameLock::Free, None), FrameVisual::Free);
        assert_eq!(derive_visual(FrameLock::MaskPainted, None), FrameVisual::Occupied);
        assert_eq!(derive_visual(FrameLock::ResultPending, None), FrameVisual::Occupied);
        assert_eq!(derive_visual(FrameLock::Processing, None), FrameVisual::Occupied);
    }

    #[test]
    fn invalid_size_wins_over_occupied() {
        // D6: red over green, in every locked state, not just the free one.
        for lock in [FrameLock::Free, FrameLock::MaskPainted, FrameLock::ResultPending, FrameLock::Processing] {
            assert_eq!(derive_visual(lock, Some(SizeViolation::NotMultiple)), FrameVisual::Invalid, "lock {lock:?}");
        }
    }

    // -----------------------------------------------------------------------------------
    // Button enablement (§10.3)
    // -----------------------------------------------------------------------------------

    #[test]
    fn buttons_of_a_free_frame_with_an_empty_mask() {
        let b = derive_buttons(FrameLock::Free, None, true, false);
        assert_eq!(b, FrameButtons { process: false, apply: false, compare: false, cancel: false, clear_mask: false });
    }

    #[test]
    fn buttons_of_a_painted_frame() {
        let b = derive_buttons(FrameLock::MaskPainted, None, false, false);
        assert_eq!(b, FrameButtons { process: true, apply: false, compare: false, cancel: false, clear_mask: true });
    }

    #[test]
    fn buttons_of_a_painted_frame_with_an_invalid_size() {
        let b = derive_buttons(FrameLock::MaskPainted, Some(SizeViolation::TooSmall), false, false);
        assert!(!b.process, "an invalid size must block processing");
        assert!(!b.compare, "there is no result to compare against yet");
        assert!(b.clear_mask, "erasing the mask is how the user gets back to a free frame");
    }

    /// A pending result must not be replaceable by a second run: the discarded one cannot be
    /// recovered, so «Обработать» stays disabled until the user applies or cancels.
    #[test]
    fn buttons_of_a_pending_result() {
        let b = derive_buttons(FrameLock::ResultPending, None, false, false);
        assert_eq!(b, FrameButtons { process: false, apply: true, compare: true, cancel: true, clear_mask: true });
    }

    #[test]
    fn buttons_while_processing() {
        let b = derive_buttons(FrameLock::Processing, None, false, false);
        // No result exists yet, so there is nothing for «Сравнить» to hide.
        assert_eq!(b, FrameButtons { process: false, apply: false, compare: false, cancel: true, clear_mask: true });
    }

    // -----------------------------------------------------------------------------------
    // Page pixel <-> screen conversions
    // -----------------------------------------------------------------------------------

    #[test]
    fn screen_rect_at_zoom_one_is_the_page_rect_offset() {
        let page = screen(100.0, 200.0, 900.0, 1400.0);
        let r = frame_screen_rect(page, 1.0, rect_px(50, 60, 200, 300));
        assert_eq!(r, screen(150.0, 260.0, 350.0, 560.0));
    }

    #[test]
    fn screen_rect_scales_by_a_non_integer_zoom() {
        let page = screen(100.0, 200.0, 900.0, 1400.0);
        let r = frame_screen_rect(page, 0.75, rect_px(40, 80, 200, 400));
        assert!((r.min.x - 130.0).abs() < 1e-3, "{r:?}");
        assert!((r.min.y - 260.0).abs() < 1e-3, "{r:?}");
        assert!((r.width() - 150.0).abs() < 1e-3, "{r:?}");
        assert!((r.height() - 300.0).abs() < 1e-3, "{r:?}");
    }

    #[test]
    fn screen_pos_round_trips_back_to_page_pixels() {
        let page = screen(100.0, 200.0, 900.0, 1400.0);
        for zoom in [1.0_f32, 0.75, 1.6] {
            let start = rect_px(64, 128, 32, 32);
            let r = frame_screen_rect(page, zoom, start);
            let (x, y) = screen_pos_to_page_px(page, zoom, r.min, 800, 1200);
            assert_eq!((x, y), (start.x, start.y), "zoom {zoom}");
        }
    }

    #[test]
    fn a_degenerate_zoom_produces_no_geometry_instead_of_nan() {
        let page = screen(0.0, 0.0, 100.0, 100.0);
        for zoom in [0.0_f32, -1.0, f32::NAN] {
            let r = frame_screen_rect(page, zoom, rect_px(10, 10, 20, 20));
            assert!(r.width() == 0.0 && r.height() == 0.0, "zoom {zoom}: {r:?}");
            assert_eq!(screen_pos_to_page_px(page, zoom, pos2(50.0, 50.0), 100, 100), (0, 0));
        }
    }

    #[test]
    fn screen_pos_is_clamped_to_the_page() {
        let page = screen(0.0, 0.0, 100.0, 100.0);
        assert_eq!(screen_pos_to_page_px(page, 1.0, pos2(-40.0, -40.0), 100, 100), (0, 0));
        assert_eq!(screen_pos_to_page_px(page, 1.0, pos2(400.0, 400.0), 100, 100), (100, 100));
    }

    // -----------------------------------------------------------------------------------
    // Keep-in-view (D3 / D4)
    // -----------------------------------------------------------------------------------

    #[test]
    fn keep_in_view_pulls_a_free_frame_back_and_leaves_a_locked_one_alone() {
        // A tall page, a short viewport, and a frame far below the visible part of it.
        let page = screen(0.0, 0.0, 800.0, 4000.0);
        let usable = screen(0.0, 0.0, 800.0, 600.0);
        // x is chosen so the 400 pt-wide chrome (the frame is only 200 pt) sits comfortably
        // inside the usable rect: this case is about the Y correction, and an x that needed
        // one too would stop it proving "only the y axis needed a correction".
        let start = rect_px(300, 2000, 200, 200);

        let placement = PagePlacement { screen: page, w: 800, h: 4000, zoom: 1.0 };
        let free = keep_in_view_px(FrameLock::Free, start, &placement, usable);
        assert_ne!(free.y, start.y, "a free frame must be pulled into the viewport");
        let pulled = frame_screen_rect(page, 1.0, free);
        let hitbox = hitbox_rect(pulled, &chrome());
        assert!(usable.contains_rect(hitbox), "hitbox {hitbox:?} outside {usable:?}");
        assert_eq!((free.x, free.w, free.h), (start.x, start.w, start.h), "only the y axis needed a correction");

        for lock in [FrameLock::MaskPainted, FrameLock::ResultPending, FrameLock::Processing] {
            let locked = keep_in_view_px(lock, start, &placement, usable);
            assert_eq!((locked.x, locked.y), (start.x, start.y), "lock {lock:?} must not move the frame");
        }
    }

    /// The half-pixel oscillation: at zoom 1.25 a 0.625 pt overhang is exactly half a source
    /// pixel, so rounding the correction to the nearest pixel overshoots, the opposite edge
    /// then hangs out by the same 0.625 pt, and `rect_px.x` alternates between two values for
    /// as long as the frame is drawn. Truncating toward zero tolerates the sub-pixel residual
    /// and the very first frame is already a fixed point.
    #[test]
    fn keep_in_view_reaches_a_fixed_point_at_a_half_pixel_overhang() {
        let page = screen(0.0, 0.0, 2000.0, 4000.0);
        // 516 pt wide, exactly the HITBOX width (the 500 pt frame plus the 8 pt the side
        // handles stick out on each side), placed so the hitbox hangs over its left edge by
        // half a source pixel and would hang over the right edge by the same if corrected.
        let usable = screen(0.125, 0.0, 516.125, 3000.0);
        let placement = PagePlacement { screen: page, w: 1600, h: 3200, zoom: 1.25 };
        // y is chosen so only the x axis has anything to correct.
        let start = rect_px(6, 100, 400, 400);

        let n1 = keep_in_view_px(FrameLock::Free, start, &placement, usable);
        let n2 = keep_in_view_px(FrameLock::Free, n1, &placement, usable);
        assert_eq!((n1.x, n1.y), (start.x, start.y), "a half-pixel overhang must not be overshot");
        assert_eq!((n2.x, n2.y), (n1.x, n1.y), "frame N+1 must be a fixed point");
    }

    #[test]
    fn keep_in_view_is_a_no_op_for_a_frame_that_already_fits() {
        let page = screen(0.0, 0.0, 800.0, 4000.0);
        let usable = screen(0.0, 0.0, 800.0, 600.0);
        let start = rect_px(100, 100, 200, 200);
        let placement = PagePlacement { screen: page, w: 800, h: 4000, zoom: 1.0 };
        let kept = keep_in_view_px(FrameLock::Free, start, &placement, usable);
        assert_eq!((kept.x, kept.y), (start.x, start.y));
    }

    // -----------------------------------------------------------------------------------
    // Pointer capture over the hitbox
    // -----------------------------------------------------------------------------------

    fn placed_frame(body: Rect) -> RegionFrame {
        let mut frame = RegionFrame::new(free_constraints(), &test_layers(&[Color32::RED, Color32::GREEN]));
        frame.page_idx = Some(0);
        frame.rect_px = Some(rect_px(0, 0, 100, 100));
        frame.hitbox = Some(hitbox_rect(body, &chrome()));
        frame
    }

    #[test]
    fn captures_pointer_covers_the_body_the_strip_and_both_rows() {
        // Wider than `CHROME_MIN_W`, so the hitbox is the frame's own width plus the margin
        // the handles stick out into — that is the case this test is written for, and it is
        // why the body must stay wider than the minimum chrome width (500 > 400).
        let body = screen(200.0, 300.0, 700.0, 500.0);
        let frame = placed_frame(body);

        assert!(frame.captures_pointer(body.center()), "the frame body");
        // The top strip sits one gap above the handles, which stick out of the body.
        let strip_y = body.top() - HANDLE_RADIUS - CHROME_GAP - TOP_STRIP_H / 2.0;
        assert!(frame.captures_pointer(pos2(body.center().x, strip_y)), "the top strip");
        // The button row sits one gap below them, the status line one gap below that.
        let buttons_y = body.bottom() + HANDLE_RADIUS + CHROME_GAP + BUTTONS_H / 2.0;
        assert!(frame.captures_pointer(pos2(body.center().x, buttons_y)), "the button row");
        let status_y = body.bottom() + HANDLE_RADIUS + CHROME_GAP + BUTTONS_H + CHROME_GAP + STATUS_H / 2.0;
        assert!(frame.captures_pointer(pos2(body.center().x, status_y)), "the status line");
    }

    #[test]
    fn captures_pointer_is_false_just_outside_the_hitbox() {
        // Again wider than `CHROME_MIN_W`: the horizontal assertions below are about the
        // handle margin, and a frame narrower than the chrome would put the hitbox edge at the
        // widened row instead, so the test would stop proving what it was written to prove.
        let body = screen(200.0, 300.0, 700.0, 500.0);
        let frame = placed_frame(body);
        let above = body.top() - chrome().above() - 1.0;
        let below = body.bottom() + chrome().below() + 1.0;
        assert!(!frame.captures_pointer(pos2(body.center().x, above)), "above the strip");
        assert!(!frame.captures_pointer(pos2(body.center().x, below)), "below the status line");
        // The side handles stick out by `HANDLE_RADIUS`, so the hitbox does too.
        assert!(frame.captures_pointer(pos2(body.left() - 1.0, body.center().y)), "the left handle");
        assert!(!frame.captures_pointer(pos2(body.left() - HANDLE_RADIUS - 1.0, body.center().y)), "left of the handles");
        assert!(!frame.captures_pointer(pos2(body.right() + HANDLE_RADIUS + 1.0, body.center().y)), "right of the handles");
    }

    // -----------------------------------------------------------------------------------
    // Resize handles
    // -----------------------------------------------------------------------------------

    /// Drives `sense_handles` through a real `egui::Context` and reports which gesture a press
    /// at `grab` followed by a drag started.
    ///
    /// Three frames, because egui hit-tests against the widget rects registered in the
    /// PREVIOUS frame: one to register the handles, one to press, one to move far enough that
    /// a `click_and_drag` widget resolves the press as a drag rather than a click.
    fn drag_started_at(body: Rect, grab: Pos2) -> Option<DragKind> {
        let ctx = egui::Context::default();
        let mut frame = placed_frame(body);
        let hitbox = hitbox_rect(body, &chrome());
        let screen_rect = hitbox.expand(100.0);
        for events in [
            Vec::new(),
            vec![
                egui::Event::PointerMoved(grab),
                egui::Event::PointerButton { pos: grab, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE },
            ],
            vec![egui::Event::PointerMoved(grab + vec2(40.0, 40.0))],
        ] {
            let input = egui::RawInput { screen_rect: Some(screen_rect), events, ..Default::default() };
            // `Context::run_ui` hands back a `FullOutput` this test has no renderer for; the
            // state it asserts on lives in `frame` instead.
            let _output = ctx.run_ui(input, |ui| {
                // Same order as the real pass: the occluding hover sensor first, so the
                // handles allocated after it keep the pointer. Its `Response` exists to
                // OCCLUDE and is discarded exactly as the real pass discards it.
                let _ = ui.allocate_rect(hitbox, Sense::hover());
                frame.sense_handles(ui, body);
            });
        }
        frame.drag.map(|drag| drag.kind)
    }

    /// A corner handle's area is L-shaped and therefore sensed through TWO rectangles. Both
    /// must start a drag of the SAME handle, or grabbing the short arm of the L would resize
    /// the wrong edge — or nothing at all.
    #[test]
    fn either_rect_of_a_corner_handle_drags_that_same_handle() {
        let body = screen(200.0, 300.0, 500.0, 500.0);
        for (handle, point) in HandleKind::ALL.into_iter().zip(handle_points(body)) {
            if !handle.is_corner() {
                continue;
            }
            let (band, stub) = handle_hit_rects(point, handle);
            let stub = stub.expect("a corner handle has two hit rectangles");
            for (name, rect) in [("band", band), ("stub", stub)] {
                assert_eq!(
                    drag_started_at(body, rect.center()),
                    Some(DragKind::Resize(handle)),
                    "{handle:?}: a press on its {name} rect did not start that handle's resize"
                );
            }
        }
    }

    /// A side midpoint is sensed through one rectangle, and pressing it must start that
    /// handle's resize rather than fall through to the frame body.
    #[test]
    fn a_side_handle_drags_from_its_single_rect() {
        let body = screen(200.0, 300.0, 500.0, 500.0);
        for (handle, point) in HandleKind::ALL.into_iter().zip(handle_points(body)) {
            if handle.is_corner() {
                continue;
            }
            let (rect, second) = handle_hit_rects(point, handle);
            assert!(second.is_none(), "{handle:?} is not a corner and needs no second rect");
            assert_eq!(drag_started_at(body, rect.center()), Some(DragKind::Resize(handle)), "{handle:?}");
        }
    }

    #[test]
    fn an_undrawn_frame_captures_nothing() {
        let frame = RegionFrame::new(free_constraints(), &test_layers(&[Color32::RED]));
        assert!(!frame.captures_pointer(pos2(0.0, 0.0)));
        assert!(!frame.is_placed());
    }

    // -----------------------------------------------------------------------------------
    // Chrome layout
    // -----------------------------------------------------------------------------------

    #[test]
    fn split_row_fills_the_row_without_overlapping() {
        let row = screen(0.0, 0.0, 300.0, 24.0);
        for count in [3, 4] {
            let slots = split_row(row, count);
            assert_eq!(slots.len(), count);
            assert!((slots[0].left() - row.left()).abs() < 1e-3, "count {count}: {:?}", slots[0]);
            let last = slots[count - 1];
            assert!((last.right() - row.right()).abs() < 1e-3, "count {count}: {last:?}");
            for pair in slots.windows(2) {
                assert!(pair[0].right() <= pair[1].left(), "count {count}: slots must not overlap: {pair:?}");
                assert!((pair[1].left() - pair[0].right() - BUTTON_GAP).abs() < 1e-3, "count {count}: one gap between slots");
            }
        }
    }

    // -----------------------------------------------------------------------------------
    // «Сравнить»: a momentary hold, never a toggle
    // -----------------------------------------------------------------------------------

    /// A frame holding a 4x4 pending result, so `buttons().compare` is enabled.
    fn frame_with_a_result() -> RegionFrame {
        let mut frame = RegionFrame::new(free_constraints(), &test_layers(&[Color32::RED]));
        frame.page_idx = Some(0);
        frame.rect_px = Some(rect_px(0, 0, 4, 4));
        frame.set_result(Some(ResultLayer::new(egui::ColorImage::filled([4, 4], Color32::WHITE))));
        frame
    }

    /// The whole point of the button: while it is held the result layer is not painted, so the
    /// original pixels under it show through.
    #[test]
    fn holding_compare_hides_the_pending_result() {
        let mut frame = frame_with_a_result();
        assert!(frame.buttons().compare, "a pending result is exactly when comparing is offered");
        assert!(!result_hidden(frame.buttons(), frame.compare_held), "nothing is hidden until the button is held");

        frame.compare_held = true;
        assert!(result_hidden(frame.buttons(), frame.compare_held), "a held «Сравнить» must hide the result");
    }

    /// The hold may not outlive the thing it hides. `compare_held` is settled when the button
    /// row is sensed and read when the contents are painted; if the result is applied or
    /// cancelled in between — from the dock panel, or by the host tool — a stale flag would
    /// leave the frame hiding a layer that no longer exists, with no way to get the picture
    /// back until the next press.
    #[test]
    fn a_held_compare_cannot_survive_the_result_disappearing() {
        let mut frame = frame_with_a_result();
        frame.compare_held = true;
        assert!(result_hidden(frame.buttons(), frame.compare_held));

        // The host applied or cancelled the result.
        frame.set_result(None);
        assert!(!frame.buttons().compare, "with no result there is nothing to compare");
        assert!(!result_hidden(frame.buttons(), frame.compare_held), "a stale hold must hide nothing");

        // And releasing the frame drops the flag outright, like every other gesture in flight.
        let mut frame = frame_with_a_result();
        frame.compare_held = true;
        frame.reset();
        assert!(!frame.compare_held, "`reset` ends the hold with the other gestures");
    }

    /// «Сравнить» is enabled by exactly the condition «Применить» is: both act on the pending
    /// result, and a user who can apply one can always look under it first.
    #[test]
    fn compare_is_offered_exactly_when_apply_is() {
        for lock in [FrameLock::Free, FrameLock::MaskPainted, FrameLock::ResultPending, FrameLock::Processing] {
            for mask_empty in [false, true] {
                let b = derive_buttons(lock, None, mask_empty, false);
                assert_eq!(b.compare, b.apply, "lock {lock:?}, mask_empty {mask_empty}");
            }
        }
    }

    /// `CHROME_MIN_W` claims to hold the FOUR chrome captions in Russian and English; this is
    /// what keeps that claim honest. The captions are read out of the shipped catalogs, so
    /// adding a button or lengthening a caption fails here instead of silently eliding in the
    /// running app. Measured with egui's own default proportional font, which is a proxy for
    /// the bundled UI stack — hence the headroom `CHROME_MIN_W` carries.
    #[test]
    fn chrome_caption_slots_hold_the_russian_and_english_captions() {
        let keys = [
            "cleaning.region_frame.button.apply",
            "cleaning.region_frame.button.compare",
            "cleaning.region_frame.button.cancel",
            "cleaning.region_frame.button.clear_mask",
        ];
        let row = Rect::from_min_max(pos2(0.0, 0.0), pos2(CHROME_MIN_W, BUTTONS_H));
        let slot_w = split_row(row, keys.len())[0].width();

        let ctx = egui::Context::default();
        let _output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let font_id = egui::TextStyle::Button.resolve(ui.style());
            // A `Button` lays its caption out inside the slot minus its horizontal padding on
            // both sides; anything wider is what `truncate()` elides.
            let text_w = slot_w - 2.0 * ui.spacing().button_padding.x;
            for (tag, source) in ms_i18n::embedded_locales() {
                if *tag != "ru" && *tag != "en" {
                    continue;
                }
                let catalog: serde_json::Value = serde_json::from_str(source).expect("a shipped catalog is valid JSON");
                for key in keys {
                    let caption = catalog.get(key).and_then(serde_json::Value::as_str).unwrap_or_else(|| panic!("{tag}: {key} is missing"));
                    let galley = ui.painter().layout_no_wrap(caption.to_owned(), font_id.clone(), Color32::WHITE);
                    assert!(
                        galley.size().x <= text_w,
                        "{tag}: «{caption}» needs {:.1} pt but a chrome slot offers {text_w:.1} pt — raise CHROME_MIN_W",
                        galley.size().x
                    );
                }
            }
        });
    }

    /// The chrome must not inherit the frame's SCREEN width. The canvas zooms down to 0.2, so
    /// a 64 px minimum-side frame is 12.8 pt wide there — a plate that could hold neither the
    /// status sentence nor four captioned buttons, and text that would spill over the page.
    #[test]
    fn a_frame_narrower_than_the_chrome_still_gets_readable_rows() {
        let narrow = Rect::from_min_size(pos2(400.0, 300.0), vec2(12.8, 12.8));
        let hitbox = hitbox_rect(narrow, &chrome());
        assert!((hitbox.width() - CHROME_MIN_W).abs() < 1e-3, "{hitbox:?}");
        assert!((hitbox.center().x - narrow.center().x).abs() < 1e-3, "the chrome stays centred on the frame");

        // The button row is laid out on that width, so every one of the FOUR slots stays
        // clickable — and wide enough for a caption, which is the sizing rule of `CHROME_MIN_W`.
        let row = Rect::from_min_max(pos2(hitbox.left(), 0.0), pos2(hitbox.right(), BUTTONS_H));
        for slot in split_row(row, 4) {
            assert!(slot.width() > 20.0, "a chrome button slot must stay usable: {slot:?}");
        }

        // And the widened hitbox is what `captures_pointer` answers from, so the pointer over
        // those buttons belongs to the frame rather than to the canvas.
        let frame = placed_frame(narrow);
        assert!(frame.captures_pointer(pos2(hitbox.left() + 1.0, narrow.center().y)));
    }

    // -----------------------------------------------------------------------------------
    // A stroke is a gesture (the erase-the-last-pixel-while-held regression)
    // -----------------------------------------------------------------------------------

    /// Erasing the last painted pixel while the button is STILL DOWN used to unlock the frame
    /// mid-gesture: the mask counter reached zero, `derive_lock` answered `Free`, and the page
    /// transition, the keep-in-view clamp and the `MaskStack::resize` a page change performs
    /// all ran under the live stroke — destroying the stroke's own undo snapshot.
    #[test]
    fn erasing_the_last_pixel_of_a_held_stroke_does_not_unlock_the_frame() {
        let page = screen(0.0, 0.0, 800.0, 4000.0);
        let usable = screen(0.0, 0.0, 800.0, 600.0);
        let placement = PagePlacement { screen: page, w: 800, h: 4000, zoom: 1.0 };
        let start = rect_px(100, 2000, 200, 200);

        let mut frame = RegionFrame::new(free_constraints(), &test_layers(&[Color32::RED]));
        frame.page_idx = Some(0);
        frame.rect_px = Some(start);
        frame.masks.resize(start.w, start.h);

        // The stroke paints a dot and then erases it, all without releasing the button.
        frame.masks.push_undo();
        frame.last_paint_px = Some((40, 40));
        frame.masks.paint_segment((40, 40), (40, 40), 3, false);
        assert!(!frame.masks.is_empty(), "the dot must have reached the mask");
        frame.masks.paint_segment((40, 40), (40, 40), 8, true);
        assert!(frame.masks.is_empty(), "the stroke erased everything it had painted");

        assert_eq!(frame.lock(), FrameLock::MaskPainted, "a stroke in flight must hold the frame");
        assert!(frame.drag_active(), "a stroke is a gesture: canvas drag-scroll stays blocked");
        let kept = keep_in_view_px(frame.lock(), start, &placement, usable);
        assert_eq!((kept.x, kept.y), (start.x, start.y), "the frame must not be re-anchored under the stroke");

        // Releasing the button ends the gesture and the empty mask frees the frame again.
        frame.last_paint_px = None;
        assert_eq!(frame.lock(), FrameLock::Free);
        assert!(!frame.drag_active());
        let released = keep_in_view_px(frame.lock(), start, &placement, usable);
        assert_ne!(released.y, start.y, "once free, the frame is pulled back into view again");
    }

    // -----------------------------------------------------------------------------------
    // Dock-panel requests (the frame is never the only surface)
    // -----------------------------------------------------------------------------------

    #[test]
    fn a_queued_panel_request_is_dropped_when_the_action_is_no_longer_allowed() {
        let pending = PendingRequests { process: true, apply: true, cancel: true };

        // A pending result: apply and cancel are allowed, a second run is not (F4).
        let mut outcome = FrameOutcome::default();
        fold_pending(pending, derive_buttons(FrameLock::ResultPending, None, false, false), &mut outcome);
        assert_eq!(
            outcome,
            FrameOutcome { process_requested: false, apply_requested: true, cancel_requested: true, clear_mask_requested: false }
        );

        // A free frame with an empty mask allows none of the three.
        let mut outcome = FrameOutcome::default();
        fold_pending(pending, derive_buttons(FrameLock::Free, None, true, false), &mut outcome);
        assert_eq!(outcome, FrameOutcome::default());
    }

    // -----------------------------------------------------------------------------------
    // The consumer-switch setters (D16)
    // -----------------------------------------------------------------------------------

    /// A consumer that refuses the frame's current size must leave it RED rather than snap it:
    /// the switch is the first moment at which a placed, valid frame can become invalid, and
    /// silently resizing it would move a rectangle the user placed by hand.
    #[test]
    fn new_constraints_turn_an_unsatisfying_frame_red_instead_of_resizing_it() {
        let mut frame = RegionFrame::new(free_constraints(), &test_layers(&[Color32::RED]));
        frame.page_idx = Some(0);
        frame.rect_px = Some(rect_px(0, 0, 100, 100));
        assert_eq!(frame.visual(), FrameVisual::Free);

        frame.set_constraints(FrameConstraints { multiple: 64, min_side: 64, max_area: None, max_aspect: None });
        assert_eq!(frame.size_violation(), Some(SizeViolation::NotMultiple));
        assert_eq!(frame.visual(), FrameVisual::Invalid);
        let kept = frame.rect_px().expect("the frame was given a rectangle above");
        assert_eq!((kept.x, kept.y, kept.w, kept.h), (0, 0, 100, 100), "the rectangle must not be snapped");
        assert!(!frame.buttons().process, "an invalid size blocks the run");
    }

    #[test]
    fn a_new_layer_declaration_rebuilds_the_stack_and_an_identical_one_does_not() {
        let mut frame = RegionFrame::new(free_constraints(), &test_layers(&[Color32::RED, Color32::GREEN]));
        frame.masks.resize(8, 8);
        assert_eq!(frame.masks().layer_count(), 2);

        // A different declaration: the stack is re-created over the same geometry.
        frame.set_mask_layers(&test_layers(&[Color32::BLUE]));
        assert_eq!(frame.masks().layer_count(), 1);
        assert_eq!(frame.masks().size(), (8, 8));
        assert_eq!(frame.layer_label(0), t!("cleaning.region_frame.status.free"));
        assert_eq!(frame.layer_label(9), "10", "a layer nobody declared falls back to its number");

        // An identical declaration must not disturb anything, so a per-frame push is free.
        frame.masks.paint_segment((1, 1), (2, 2), 1, false);
        assert!(!frame.masks().is_empty());
        frame.set_mask_layers(&test_layers(&[Color32::BLUE]));
        assert!(!frame.masks().is_empty(), "an unchanged declaration must keep the painted mask");
    }

    /// D15's rule, enforced by the frame itself: a locked frame protects painted work even if a
    /// caller asks for a layer switch anyway.
    #[test]
    fn a_locked_frame_refuses_a_layer_switch() {
        let mut frame = RegionFrame::new(free_constraints(), &test_layers(&[Color32::RED]));
        frame.masks.resize(8, 8);
        frame.masks.paint_segment((1, 1), (2, 2), 1, false);
        assert_eq!(frame.lock(), FrameLock::MaskPainted);

        frame.set_mask_layers(&test_layers(&[Color32::BLUE, Color32::GREEN]));
        assert_eq!(frame.masks().layer_count(), 1, "the stack must survive the refused switch");
        assert!(!frame.masks().is_empty(), "and so must the painted mask");
    }

    /// «Работа без маски»: the run button must come alive with nothing painted, and only
    /// because of this flag — the size check and the lock stay in force.
    #[test]
    fn allowing_an_empty_mask_unblocks_the_run_and_nothing_else() {
        let mut frame = RegionFrame::new(free_constraints(), &test_layers(&[Color32::RED]));
        frame.page_idx = Some(0);
        frame.rect_px = Some(rect_px(0, 0, 64, 64));
        assert!(!frame.buttons().process, "an empty mask blocks the run by default");

        frame.set_allows_empty_mask(true);
        assert!(frame.buttons().process);
        assert!(!frame.buttons().clear_mask, "there is still nothing to erase");

        // An invalid size still wins over the relaxation.
        frame.set_constraints(FrameConstraints { multiple: 64, min_side: 4096, max_area: None, max_aspect: None });
        assert!(!frame.buttons().process);
    }

    #[test]
    fn a_request_queued_by_a_panel_is_kept_until_a_pass_consumes_it() {
        let mut frame = RegionFrame::new(free_constraints(), &test_layers(&[Color32::RED]));
        assert_eq!(frame.pending, PendingRequests::default());
        frame.request_apply();
        frame.request_cancel();
        frame.request_process();
        assert_eq!(frame.pending, PendingRequests { process: true, apply: true, cancel: true });
        // `reset` releases the frame, so nothing queued against the released state survives.
        frame.reset();
        assert_eq!(frame.pending, PendingRequests::default());
    }

    /// The frame's own chrome and the dock panel ask for the same actions; one must not
    /// replace the other, or a panel press made in the same frame as a chrome press is lost.
    #[test]
    fn outcomes_from_the_two_surfaces_are_merged_not_replaced() {
        let mut outcome = FrameOutcome { apply_requested: true, ..FrameOutcome::default() };
        outcome.merge(FrameOutcome { clear_mask_requested: true, ..FrameOutcome::default() });
        assert!(outcome.apply_requested && outcome.clear_mask_requested);
    }

    // -----------------------------------------------------------------------------------
    // The brush: the region editor's gestures, on the canvas
    // -----------------------------------------------------------------------------------

    /// Drives `handle_brush_gestures` through a real `egui::Context` with the pointer parked at
    /// `pointer` and `events` / `modifiers` delivered on the second pass, and reports the
    /// radius it left behind.
    ///
    /// Two passes, because egui hit-tests against the widget rects registered in the PREVIOUS
    /// one: the first registers the hitbox sensor and moves the pointer, the second is the one
    /// whose `Response` can answer `contains_pointer` at all.
    fn brush_radius_after(body: Rect, pointer: Pos2, modifiers: egui::Modifiers, events: Vec<egui::Event>) -> usize {
        let ctx = egui::Context::default();
        let mut frame = placed_frame(body);
        let hitbox = hitbox_rect(body, &chrome());
        let screen_rect = hitbox.expand(200.0);
        for events in [vec![egui::Event::PointerMoved(pointer)], events] {
            let input = egui::RawInput { screen_rect: Some(screen_rect), modifiers, events, ..Default::default() };
            // `run_ui` hands back a `FullOutput` this test has no renderer for; what it asserts
            // on is the brush radius inside `frame`.
            let _output = ctx.run_ui(input, |ui| {
                // The same two lines the real pass runs, in the same order.
                let over_frame = ui.allocate_rect(hitbox, Sense::hover()).contains_pointer();
                frame.handle_brush_gestures(ui, over_frame);
            });
        }
        frame.brush.radius_px()
    }

    fn key_press(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    /// A wheel notch delivered as six sub-notch events rather than one big one: egui hands any
    /// wheel delta of 8 points or more to its own smoothing filter, which spreads it over
    /// several frames, while anything smaller lands in `smooth_scroll_delta` whole
    /// (`egui-0.35.0/src/input_state/wheel_state.rs`, `is_smooth`).
    fn shift_wheel(points: f32) -> Vec<egui::Event> {
        let modifiers = egui::Modifiers { shift: true, ..egui::Modifiers::NONE };
        (0..6)
            .map(|_| egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: vec2(0.0, points),
                phase: egui::TouchPhase::Move,
                modifiers,
            })
            .collect()
    }

    /// The region editor's `-` / `=` / `+` must resize the brush with the pointer over the
    /// frame. They cannot arrive through `CleaningTool::on_key_event` there: `tab.rs` drops
    /// tool hotkeys while the canvas pointer is occluded and the frame occludes its own
    /// hitbox, so the frame has to answer them inside its own pass.
    #[test]
    fn the_size_shortcuts_resize_the_brush_while_the_pointer_is_over_the_frame() {
        let body = screen(200.0, 300.0, 500.0, 500.0);
        let default_radius = MaskBrush::default().radius_px();
        let none = egui::Modifiers::NONE;

        let grown = brush_radius_after(body, body.center(), none, vec![key_press(egui::Key::Equals, none)]);
        assert!(grown > default_radius, "`=` must grow the brush: {default_radius} -> {grown}");
        let plus = brush_radius_after(body, body.center(), none, vec![key_press(egui::Key::Plus, none)]);
        assert_eq!(plus, grown, "`+` is the same binding as `=`");
        let shrunk = brush_radius_after(body, body.center(), none, vec![key_press(egui::Key::Minus, none)]);
        assert!(shrunk < default_radius, "`-` must shrink the brush: {default_radius} -> {shrunk}");
    }

    /// The whole hitbox counts, not just the body: the strip, the handles and the two chrome
    /// rows are the frame too, and the tab delivers nothing there either.
    #[test]
    fn the_size_shortcuts_reach_the_frame_over_its_chrome_but_not_off_it() {
        let body = screen(200.0, 300.0, 500.0, 500.0);
        let default_radius = MaskBrush::default().radius_px();
        let none = egui::Modifiers::NONE;

        let status_y = body.bottom() + HANDLE_RADIUS + CHROME_GAP + BUTTONS_H + CHROME_GAP + STATUS_H / 2.0;
        let over_status = pos2(body.center().x, status_y);
        assert!(
            brush_radius_after(body, over_status, none, vec![key_press(egui::Key::Equals, none)]) > default_radius,
            "the status line belongs to the frame"
        );

        // Off the hitbox the canvas owns the pointer, and `on_key_event` — which the tab does
        // deliver there — is the path that answers instead.
        let off_frame = pos2(body.center().x, body.top() - chrome().above() - 20.0);
        assert_eq!(
            brush_radius_after(body, off_frame, none, vec![key_press(egui::Key::Equals, none)]),
            default_radius,
            "the frame must not answer a shortcut aimed at the canvas"
        );
    }

    /// Ctrl + `-` / `=` is the canvas zoom (`canvas/mod.rs`), so the brush must keep its hands
    /// off it even with the pointer over the frame.
    #[test]
    fn a_zoom_modifier_takes_the_size_shortcuts_away_from_the_brush() {
        let body = screen(200.0, 300.0, 500.0, 500.0);
        let ctrl = egui::Modifiers { ctrl: true, ..egui::Modifiers::NONE };
        assert_eq!(
            brush_radius_after(body, body.center(), ctrl, vec![key_press(egui::Key::Equals, ctrl)]),
            MaskBrush::default().radius_px()
        );
    }

    /// Shift+wheel over the frame resizes the brush, exactly as it does over the region
    /// editor's preview. With Shift held egui reports the wheel as HORIZONTAL scrolling, which
    /// is why the gesture is read from the X component when Y is flat.
    #[test]
    fn shift_wheel_over_the_frame_resizes_the_brush() {
        let body = screen(200.0, 300.0, 500.0, 500.0);
        let default_radius = MaskBrush::default().radius_px();
        let shift = egui::Modifiers { shift: true, ..egui::Modifiers::NONE };

        assert!(brush_radius_after(body, body.center(), shift, shift_wheel(7.0)) > default_radius, "one notch up");
        assert!(brush_radius_after(body, body.center(), shift, shift_wheel(-7.0)) < default_radius, "one notch down");
        // Without Shift the wheel belongs to the canvas, which scrolls the page with it.
        assert_eq!(
            brush_radius_after(body, body.center(), egui::Modifiers::NONE, shift_wheel(7.0)),
            default_radius,
            "an unmodified wheel must not touch the brush"
        );
    }

    /// The erase rule is the region editor's: the right button erases unless the left is held
    /// too, and Shift+left erases whatever mode the panel offers. Shift is read inside the
    /// pass because `CleaningTool::set_temporary_erase` never reaches a tool whose frame
    /// occludes the canvas pointer.
    #[test]
    fn the_erase_rule_matches_the_region_editors() {
        // (primary, secondary, shift, mode) -> erases
        assert!(!stroke_erases(true, false, false, false), "a plain left drag paints");
        assert!(stroke_erases(false, true, false, false), "the right button erases");
        assert!(stroke_erases(true, false, true, false), "Shift + left erases");
        assert!(stroke_erases(true, false, false, true), "the panel's erase mode erases");
        assert!(!stroke_erases(true, true, false, false), "with both buttons down the left one wins");
        assert!(stroke_erases(false, false, true, false), "Shift alone would erase once a button goes down");
    }

    #[test]
    fn frame_lock_is_derived_from_the_frames_own_contents() {
        let mut frame = RegionFrame::new(free_constraints(), &test_layers(&[Color32::RED]));
        assert_eq!(frame.lock(), FrameLock::Free);
        frame.set_processing(true);
        assert_eq!(frame.lock(), FrameLock::Processing);
        frame.set_processing(false);
        assert_eq!(frame.lock(), FrameLock::Free);
    }
}

