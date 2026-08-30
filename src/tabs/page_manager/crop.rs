/*
File: tabs/page_manager/crop.rs

Purpose:
The "crop page" window: an `egui::Window` that shows ONE selected page on a
zoomable, pannable board, rotated by the user's quarter turns plus a fine
straightening angle, with a draggable crop frame over it and a confirm that
emits `PageOpKind::Crop`.

Key structures:
- CropDialogState: the dialog's own state (rotation, frame, ratio, camera, drag).
- RatioChoice: the aspect-ratio presets the picker offers.
- CropDrag: the frame drag in progress (handle + start frame + total delta).

Key functions:
- PageManagerTabState::draw_crop_dialog(): the per-frame window.
- build_crop_op() / validate_state(): state -> the engine request (pure).
- screen_delta_to_canvas(): a pointer delta in points -> canvas pixels (pure).
- page_quad_screen(): the rotated page's four screen corners (pure).
- paint_textured_quad(): the rotated preview, as an `epaint::Mesh`.
- layout_error_message(): CropLayoutError -> localized user text.

Notes:
The board's WORLD space is the ROTATED CANVAS' pixel space, not the source
page's. Two consequences the whole file rests on:

* The crop frame is AXIS-ALIGNED on screen at every rotation, which is exactly
  what `crop_layout`'s `ScreenRect`-based handle geometry and hit test require.
  What is drawn rotated is the PAGE, as a textured quad whose four corners come
  from the ENGINE's own `RotatedPage::map_point` — so the preview and the rect
  this window emits can never disagree about where the page sits.
* Converting a pointer movement into canvas pixels is a division by the zoom
  (`screen_delta_to_canvas`); the rotation is already carried by the world
  basis and must not be applied a second time.

Frame coordinates are ROTATED-CANVAS pixels, never preview pixels: the preview
from the bounded cache of `thumbs.rs` is only ever a VIEW, and the engine crops
the untouched original. All frame math lives in `crop_layout.rs` and the canvas
geometry in `page_ops::crop_geometry`; this file only draws and routes input.
No decode ever happens on the GUI thread.
*/

use std::collections::HashMap;

use eframe::egui::{self, epaint};

use crate::app::PageImageInfo;
use crate::page_ops::PageOpKind;
use crate::page_ops::crop_geometry::{PageRotation, RotatedPage};
use crate::project::ProjectData;
use crate::tabs::ps_editor::viewport::{PsViewport, ViewTransform};
use crate::widgets::{WheelSlider, WheelSpinBox, combo_popup_open};

use super::crop_layout::{self, AspectRatio, CropFrame, CropHandle, CropLayoutError, ScreenRect};
use super::thumbs::{PreviewState, SPLIT_PREVIEW_LONG_SIDE_PX};
use super::{PageManagerAction, PageManagerTabState};

/// Zoom step handed to [`PsViewport::handle_input`] per wheel notch.
///
/// `raw_wheel_delta` is unit-dependent (`Point` / `Line` / `Page`), so its
/// magnitude must never be used as a distance (`egui-docs/03-input.md`). Only its
/// sign is read here and this fixed step is applied instead, which makes one
/// notch mean the same zoom change on every platform. Same rationale and value
/// as the split and stitch boards' constant.
const WHEEL_ZOOM_STEP: f32 = 100.0;

/// Side of a corner grab handle, in SCREEN points.
///
/// Screen-constant on purpose, exactly like the split window's cut handles: the
/// handles must stay equally grabbable at any zoom, so their footprint in canvas
/// pixels is not a constant and cannot be baked into the frame.
const HANDLE_SIZE_POINTS: f32 = 14.0;

/// The ONE precision of the fine straightening angle, in degrees.
///
/// The slider steps by it, [`quantize_angle`] rounds every stored angle to it,
/// and [`FINE_ANGLE_DECIMALS`] displays it exactly. Two controls share this
/// value, so neither can silently override the other's precision: a slider that
/// stepped more coarsely than the spin box displays would re-snap the shared
/// angle every frame and quantize away what the user typed.
const FINE_ANGLE_STEP_DEG: f64 = 0.01;

/// Degrees moved by one wheel notch over either angle control: five steps, so a
/// notch is a usable nudge while the step stays the precision.
const FINE_ANGLE_WHEEL_STEP_DEG: f64 = 0.05;

/// Degrees per point of pointer travel while the angle spin box is dragged.
const FINE_ANGLE_DRAG_SPEED: f64 = 0.02;

/// Decimals shown by the angle widgets, matching [`FINE_ANGLE_STEP_DEG`] exactly.
const FINE_ANGLE_DECIMALS: usize = 2;

/// Alpha of the veil painted over everything the crop discards.
const DIM_ALPHA: u8 = 140;

/// Colors of the crop frame and its handles. Fixed rather than taken from the
/// theme: the frame must stay readable over an arbitrary page image, which the
/// theme's widget colors are not chosen for.
const FRAME_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 190, 60);
const HANDLE_FILL: egui::Color32 = egui::Color32::from_rgb(255, 190, 60);
const HANDLE_STROKE: egui::Color32 = egui::Color32::from_rgb(40, 30, 10);
/// Color of the rule-of-thirds guides inside the frame.
const GUIDE_COLOR: egui::Color32 = egui::Color32::from_rgba_premultiplied(200, 200, 200, 90);

/// The aspect-ratio presets the window offers.
///
/// Exhaustive on purpose: a new preset must force the label table and the
/// [`AspectRatio`] mapping to be reconsidered. A free `w:h` entry is
/// deliberately NOT offered — the three presets cover the framing decisions this
/// window is for, and a numeric pair would need its own validation surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RatioChoice {
    /// Width and height move independently.
    Free,
    /// The quarter-turned page's own `w:h`.
    Original,
    /// A square crop.
    Square,
}

/// A crop-frame drag in progress.
///
/// `start_frame` is the frame as it was when the drag began and `delta` is the
/// pointer's TOTAL movement since then, in canvas pixels: `crop_layout` resolves
/// every drag from the start state, which preserves the grab offset and keeps a
/// long drag free of accumulation drift. The delta is accumulated in CANVAS
/// units (converted per frame at the current zoom), so zooming mid-drag stays
/// consistent.
#[derive(Debug, Clone, Copy)]
struct CropDrag {
    handle: CropHandle,
    start_frame: CropFrame,
    delta: [f64; 2],
}

/// State of the "crop page" window.
///
/// `page_idx` is the selection snapshot taken when the window opened; it is
/// re-validated against the current page count on every frame, because
/// `clamp_selection` may silently drop it after a reload.
///
/// Invariant maintained by [`CropDialogState::ensure_frame`]: `frame` is `Some`
/// and valid for the CURRENT rotated canvas whenever that canvas is known, and
/// `None` otherwise. Rotation changes resize the canvas underneath the frame, so
/// the frame is re-fitted rather than clamped.
pub(super) struct CropDialogState {
    /// Index of the page being cropped, in the CURRENT page order.
    page_idx: usize,
    /// The page's pixel size, once the thumbnail/preview probe has reported it.
    page_size: Option<[u32; 2]>,
    /// Clockwise 90-degree steps applied before the fine angle; always `0..=3`.
    quarter_turns: u8,
    /// Fine straightening angle in degrees; always strictly inside ±45.
    angle_deg: f64,
    /// The crop rect, in rotated-canvas pixels.
    frame: Option<CropFrame>,
    /// The ratio the frame is locked to while it is resized.
    ratio: RatioChoice,
    /// Board camera.
    viewport: PsViewport,
    /// Whether the camera has already been fit to the current canvas.
    camera_fitted: bool,
    /// The frame drag in progress, if any. `None` means a board drag pans.
    drag: Option<CropDrag>,
}

impl CropDialogState {
    /// Fresh dialog for the given current page index: no rotation, a free ratio,
    /// and a frame that is seeded to the whole canvas on the first frame that
    /// knows the page size.
    pub(super) fn new(page_idx: usize) -> Self {
        Self {
            page_idx,
            page_size: None,
            quarter_turns: 0,
            angle_deg: 0.0,
            frame: None,
            ratio: RatioChoice::Free,
            viewport: PsViewport::default(),
            camera_fitted: false,
            drag: None,
        }
    }

    /// Size of the ROTATED CANVAS for the current rotation, or `None` while the
    /// page size is unknown or the engine refuses the rotation.
    ///
    /// Always the engine's own bounding box (`RotatedPage::canvas_size`), never a
    /// second copy of the formula, so the dialog can never offer a crop the
    /// engine measures differently.
    #[must_use]
    fn canvas(&self) -> Option<[u32; 2]> {
        Some(rotated_page(self.page_size?, self.quarter_turns, self.angle_deg)?.canvas_size())
    }

    /// The page after its quarter turns and BEFORE the fine angle, or `None`
    /// while the page size is unknown.
    ///
    /// This is the engine's own canvas at a zero angle, which `RotatedPage`
    /// guarantees to be exactly the quarter-turned page — never a second copy of
    /// the "sides swap on an odd turn" rule.
    #[must_use]
    fn turned_page(&self) -> Option<[u32; 2]> {
        turned_page_size(self.page_size, self.quarter_turns)
    }

    /// The ratio a resize drag must preserve, for the current preset.
    #[must_use]
    fn aspect(&self) -> AspectRatio {
        aspect_ratio(self.ratio, self.page_size, self.quarter_turns)
    }

    /// Fits the frame to the largest rectangle INSCRIBED in the rotated page,
    /// honouring the current ratio preset.
    ///
    /// The affordance a straightening tool owes the user: a frame spanning the
    /// whole rotated canvas always contains the transparent wedges the rotation
    /// leaves, and this is the largest frame guaranteed to contain page pixels
    /// everywhere.
    ///
    /// A no-op while the fit cannot be computed. Every reachable refusal is a
    /// state in which the board shows no frame at all (no page size yet, or a
    /// rotation the engine rejects) and the confirm strip already says so, so
    /// there is nothing to report here and nothing worth replacing a good frame
    /// with.
    fn fit_inscribed(&mut self) {
        let (Some(canvas), Some(turned)) = (self.canvas(), self.turned_page()) else {
            return;
        };
        if let Ok(fitted) =
            crop_layout::largest_inscribed_frame(canvas, turned, self.angle_deg, self.aspect())
        {
            self.frame = Some(fitted);
            self.drag = None;
        }
    }

    /// Whether the current rotation is a lossless pixel permutation.
    ///
    /// True only for an exactly zero fine angle — the same rule the engine's
    /// `PageRotation::is_identity` uses for its own exactness guarantee. A
    /// quarter turn alone transposes pixels; any other angle resamples.
    #[must_use]
    fn is_lossless(&self) -> bool {
        self.angle_deg == 0.0
    }

    /// Sets the ABSOLUTE rotation, normalized into the canonical
    /// `(quarter_turns, angle_deg)` pair.
    ///
    /// `quarter_turns` may be negative or out of range and `angle_deg`
    /// unbounded: `crop_layout::normalize_rotation` reduces both, so straightening
    /// past ±45° rolls into the next 90° step instead of leaving an angle the
    /// engine would refuse. A rotation that changes nothing is a no-op, so
    /// calling this from a widget that reports no change costs nothing.
    ///
    /// Only a QUARTER TURN rebuilds the frame and re-fits the camera; a change of
    /// the fine angle preserves both.
    ///
    /// A quarter turn transposes the canvas, so the frame's coordinates mean
    /// something else afterwards and the board's aspect flips: starting over is
    /// the only honest answer. A fine angle only grows or shrinks the canvas
    /// AROUND a page that stays centred in it, so the frame and the camera are
    /// translated by half the size change (`crop_layout::recentre_frame`) and
    /// stay over the same page content. Refitting there would make straightening
    /// by eye at working zoom — the entire purpose of the control — impossible,
    /// because every step would snap the board back to whole-canvas fit and
    /// discard the placed frame.
    ///
    /// The frame invariant is restored before this returns, so no caller ever
    /// observes the gap.
    fn set_rotation(&mut self, quarter_turns: i32, angle_deg: f64) {
        let (turns, angle) =
            crop_layout::normalize_rotation(quarter_turns, quantize_angle(angle_deg));
        // Exact comparison on purpose: this is change DETECTION on a value both
        // controls write at one quantized precision, not a tolerance question.
        if turns == self.quarter_turns && angle == self.angle_deg {
            return;
        }
        let quarter_changed = turns != self.quarter_turns;
        let old_canvas = self.canvas();
        self.quarter_turns = turns;
        self.angle_deg = angle;
        self.drag = None;
        if quarter_changed {
            self.frame = None;
            self.camera_fitted = false;
        } else if let (Some(old), Some(new)) = (old_canvas, self.canvas()) {
            self.frame = self.frame.map(|frame| {
                crop_layout::recentre_frame(frame, old, new, crop_layout::MIN_FRAME_SIDE_PX)
            });
            // The camera follows the same half-delta, so the page does not slide
            // under the viewport while the angle is nudged. Skipped before the
            // first fit, which has no meaningful centre yet.
            if self.camera_fitted {
                let shift = canvas_shift(old, new);
                self.viewport
                    .set_camera(self.viewport.zoom(), self.viewport.center_world() + shift);
            }
        }
        self.ensure_frame();
    }

    /// Switches the aspect-ratio preset, re-fitting the frame to it.
    ///
    /// A no-op when the preset is unchanged, so the user's framing survives a
    /// redundant click on the radio row. The frame invariant is restored before
    /// this returns.
    fn set_ratio(&mut self, ratio: RatioChoice) {
        if self.ratio == ratio {
            return;
        }
        self.ratio = ratio;
        self.frame = None;
        self.drag = None;
        self.ensure_frame();
    }

    /// Restores the frame invariant: a frame valid for the current canvas, or
    /// `None` while there is no canvas.
    ///
    /// Called once per frame BEFORE anything reads the frame (the page size may
    /// have just arrived from the worker), and at the tail of every mutator that
    /// invalidates the frame, so no caller ever observes the gap.
    fn ensure_frame(&mut self) {
        let Some(canvas) = self.canvas() else {
            self.frame = None;
            return;
        };
        let intact = self.frame.is_some_and(|frame| {
            crop_layout::validate(canvas, frame.rect(), self.quarter_turns, self.angle_deg).is_ok()
        });
        if !intact {
            // `largest_centred_frame` only fails on an empty canvas, which
            // `canvas()` cannot produce (the engine refuses a zero-sized page).
            self.frame = crop_layout::largest_centred_frame(canvas, self.aspect()).ok();
        }
    }
}

/// The engine's validated (page, rotation) pair, or `None` when the engine
/// refuses the request.
///
/// The engine's error text is deliberately not surfaced: every refusal reachable
/// from this window's widgets (quarter turns out of range, angle outside ±45) is
/// restated by `crop_layout::validate` as a localized message in the confirm
/// strip, and the remaining ones (a zero-sized page, a canvas past `u32`) are
/// reported by the board's own "rotation unsupported" caption.
#[must_use]
fn rotated_page(page_size: [u32; 2], quarter_turns: u8, angle_deg: f64) -> Option<RotatedPage> {
    let rotation = PageRotation::new(quarter_turns, angle_deg).ok()?;
    RotatedPage::new(page_size, rotation).ok()
}

/// Maps a preset to the ratio a resize drag must preserve.
///
/// [`RatioChoice::Original`] is the QUARTER-TURNED page's own `w:h`, taken from
/// the engine's canvas at a zero fine angle — which `RotatedPage` guarantees to
/// be exactly the quarter-turned page. It deliberately ignores the fine angle:
/// "the page's own proportions" is what the user picked, and the bounding box of
/// a straightened page is not that.
///
/// Falls back to [`AspectRatio::Free`] while the page size is unknown, so the
/// frame is never locked to a ratio derived from nothing.
#[must_use]
fn aspect_ratio(
    choice: RatioChoice,
    page_size: Option<[u32; 2]>,
    quarter_turns: u8,
) -> AspectRatio {
    match choice {
        RatioChoice::Free => AspectRatio::Free,
        RatioChoice::Original => turned_page_size(page_size, quarter_turns)
            .and_then(|size| AspectRatio::locked(size[0], size[1]))
            .unwrap_or(AspectRatio::Free),
        RatioChoice::Square => AspectRatio::locked(1, 1).unwrap_or(AspectRatio::Free),
    }
}

/// The page after `quarter_turns` and before any fine angle, taken from the
/// ENGINE's zero-angle canvas rather than from a restated axis-swap rule.
#[must_use]
fn turned_page_size(page_size: Option<[u32; 2]>, quarter_turns: u8) -> Option<[u32; 2]> {
    Some(rotated_page(page_size?, quarter_turns, 0.0)?.canvas_size())
}

/// Rounds a fine angle to [`FINE_ANGLE_STEP_DEG`], the single precision both
/// angle controls work at. A non-finite angle becomes `0.0`.
///
/// Applied BEFORE `normalize_rotation`, never after: rounding afterwards could
/// push a residual produced by a ±45° roll back onto the excluded boundary, and
/// the engine refuses exactly ±45°.
#[must_use]
fn quantize_angle(angle_deg: f64) -> f64 {
    if !angle_deg.is_finite() {
        return 0.0;
    }
    (angle_deg / FINE_ANGLE_STEP_DEG).round() * FINE_ANGLE_STEP_DEG
}

/// How far the content of a rotated page moves on screen when the canvas is
/// resized around it: half the size change, because the page stays centred.
#[must_use]
fn canvas_shift(old_canvas: [u32; 2], new_canvas: [u32; 2]) -> egui::Vec2 {
    egui::vec2(
        f64_to_f32((f64::from(new_canvas[0]) - f64::from(old_canvas[0])) / 2.0),
        f64_to_f32((f64::from(new_canvas[1]) - f64::from(old_canvas[1])) / 2.0),
    )
}

/// Converts a pointer movement in SCREEN points into ROTATED-CANVAS pixels.
///
/// The board's world space IS the canvas' pixel space (see the file header), so
/// this is a pure division by the zoom — the rotation is carried by the world
/// basis and must not be applied a second time. The result feeds
/// `crop_layout::apply_drag_with_ratio`, which resolves the drag from the frame
/// the drag STARTED on: at a ribbon's fit zoom one screen point is tens of canvas
/// pixels, so snapping the frame to the pointer would throw the grab offset away
/// as a jump of hundreds of pixels.
#[must_use]
fn screen_delta_to_canvas(view: &ViewTransform, delta: egui::Vec2) -> [f64; 2] {
    // `PsViewport` clamps the zoom to [0.02, 32.0], so this floor is unreachable;
    // it exists so that a future camera change can never divide by zero here.
    let zoom = f64::from(view.zoom.max(f32::MIN_POSITIVE));
    [f64::from(delta.x) / zoom, f64::from(delta.y) / zoom]
}

/// Screen rect of the crop frame, in the board's own space.
#[must_use]
fn frame_screen_rect(view: &ViewTransform, frame: CropFrame) -> ScreenRect {
    let world = egui::Rect::from_min_size(
        egui::pos2(u32_to_f32(frame.x()), u32_to_f32(frame.y())),
        egui::vec2(u32_to_f32(frame.width()), u32_to_f32(frame.height())),
    );
    let screen = view.world_rect_to_screen(world);
    ScreenRect::from_min_max(screen.min.x, screen.min.y, screen.max.x, screen.max.y)
}

/// `crop_layout`'s egui-free rect as an `egui::Rect`.
#[must_use]
fn to_egui_rect(rect: ScreenRect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(rect.min_x, rect.min_y),
        egui::pos2(rect.max_x, rect.max_y),
    )
}

/// The four screen corners of the ROTATED page, in the order
/// top-left, top-right, bottom-right, bottom-left OF THE SOURCE PAGE.
///
/// That order is what lets the quad be textured with the fixed UV corners
/// `(0,0) (1,0) (1,1) (0,1)`: corner `k` of the quad is always corner `k` of the
/// preview texture, whatever the rotation does to it on screen. The mapping is
/// the ENGINE's (`RotatedPage::map_point`), so the drawn page and the emitted
/// crop rect share one definition of the canvas.
#[must_use]
fn page_quad_screen(rotated: &RotatedPage, view: &ViewTransform) -> [egui::Pos2; 4] {
    let [width, height] = rotated.page_size();
    let (w, h) = (f64::from(width), f64::from(height));
    [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)].map(|(x, y)| {
        let (canvas_x, canvas_y) = rotated.map_point(x, y);
        view.world_to_screen(egui::pos2(f64_to_f32(canvas_x), f64_to_f32(canvas_y)))
    })
}

/// Widening conversion of a pixel count into the board's world/screen space.
/// Exact below 2^24 px, far above any page the engine accepts.
#[must_use]
fn u32_to_f32(value: u32) -> f32 {
    value as f32
}

/// Narrowing conversion of an engine-space coordinate into board space.
///
/// The inputs are canvas pixel coordinates produced by `RotatedPage` from
/// integer page sizes, so they are finite and bounded by the `u32` canvas; the
/// conversion only drops sub-pixel precision the board cannot display.
#[must_use]
fn f64_to_f32(value: f64) -> f32 {
    value as f32
}

/// Cursor shown over each grab region.
///
/// Exhaustive: a new [`CropHandle`] must not compile until it has a cursor.
#[must_use]
fn cursor_for(handle: CropHandle) -> egui::CursorIcon {
    match handle {
        CropHandle::TopLeft | CropHandle::BottomRight => egui::CursorIcon::ResizeNwSe,
        CropHandle::TopRight | CropHandle::BottomLeft => egui::CursorIcon::ResizeNeSw,
        CropHandle::Left | CropHandle::Right => egui::CursorIcon::ResizeHorizontal,
        CropHandle::Top | CropHandle::Bottom => egui::CursorIcon::ResizeVertical,
        CropHandle::Move => egui::CursorIcon::Move,
    }
}

/// Localized caption of an aspect-ratio preset.
///
/// The `t!` macro only accepts a string literal, so the mapping is an exhaustive
/// match instead of a key table: adding a preset must not compile until it has a
/// caption.
#[must_use]
fn ratio_label(ratio: RatioChoice) -> &'static str {
    match ratio {
        RatioChoice::Free => t!("page_manager.crop_dialog.ratio_free_radio"),
        RatioChoice::Original => t!("page_manager.crop_dialog.ratio_original_radio"),
        RatioChoice::Square => t!("page_manager.crop_dialog.ratio_square_radio"),
    }
}

/// The single validation gate of the window: the confirm button is enabled
/// exactly when this succeeds.
///
/// An unknown canvas is reported as `CanvasEmpty` and a missing frame as
/// `FrameEmpty`, which is what keeps the confirm disabled while the page size is
/// still being probed.
///
/// # Errors
/// Any [`CropLayoutError`] raised by `crop_layout::validate`.
fn validate_state(state: &CropDialogState) -> Result<(), CropLayoutError> {
    crop_layout::validate(
        state.canvas().unwrap_or([0, 0]),
        state.frame.map_or([0, 0, 0, 0], CropFrame::rect),
        state.quarter_turns,
        state.angle_deg,
    )
}

/// Builds the engine request from the current state.
///
/// # Errors
/// Whatever [`validate_state`] refuses; the request is never built from a state
/// the engine would then reject.
fn build_crop_op(state: &CropDialogState) -> Result<PageOpKind, CropLayoutError> {
    validate_state(state)?;
    Ok(PageOpKind::Crop {
        page_idx: state.page_idx,
        quarter_turns: state.quarter_turns,
        angle_deg: state.angle_deg,
        rect: state.frame.map_or([0, 0, 0, 0], CropFrame::rect),
    })
}

/// Whether the confirm button is offered this frame.
///
/// Pure so the gate is testable without a GUI, and it is a gate worth testing: a
/// crop is applied immediately and is not undone by discarding unsaved changes.
/// It is therefore refused while another page operation runs, while the page
/// PREVIEW failed to decode (the user would be cropping a page they cannot see),
/// and while the state is not a legal engine request.
#[must_use]
fn confirm_enabled(
    op_in_progress: bool,
    preview_failed: bool,
    validation: Result<(), CropLayoutError>,
) -> bool {
    !op_in_progress && !preview_failed && validation.is_ok()
}

/// Maps a layout error to the localized message shown to the user.
///
/// The `CropLayoutError` `Display` texts are technical (log/English); this is the
/// single place that turns them into UI strings.
#[must_use]
fn layout_error_message(error: CropLayoutError) -> String {
    match error {
        // Reachable only while the page size is still unknown: `canvas()` cannot
        // produce an empty canvas otherwise.
        CropLayoutError::CanvasEmpty { .. } => {
            t!("page_manager.crop_dialog.loading_size").to_string()
        }
        CropLayoutError::FrameEmpty { .. } => {
            t!("page_manager.crop_dialog.frame_empty_error").to_string()
        }
        CropLayoutError::FrameOutsideCanvas { canvas, .. } => tf!(
            "page_manager.crop_dialog.frame_outside_error",
            width = canvas[0],
            height = canvas[1]
        ),
        // Both are rotations the engine refuses; the window normalizes every
        // rotation change, so they can only appear if that normalization is
        // bypassed, and reopening the window is the only useful advice.
        CropLayoutError::QuarterTurnsOutOfRange { .. }
        | CropLayoutError::AngleOutOfRange { .. } => {
            t!("page_manager.crop_dialog.rotation_invalid_error").to_string()
        }
    }
}

impl PageManagerTabState {
    /// Draws the "crop page" window. Returns the state to keep, or `None` when
    /// the dialog closed this frame (confirmed, cancelled, or invalidated).
    ///
    /// `page_infos` supplies authoritative page geometry; a page missing from it
    /// falls back to the thumbnail/preview probe, and the board stays in its
    /// "loading" state until the size is known.
    pub(super) fn draw_crop_dialog(
        &mut self,
        ctx: &egui::Context,
        mut state: CropDialogState,
        project: &ProjectData,
        page_infos: &HashMap<usize, PageImageInfo>,
        op_in_progress: bool,
        actions: &mut Vec<PageManagerAction>,
    ) -> Option<CropDialogState> {
        // `clamp_selection` drops out-of-range indices every frame, so a reload
        // can invalidate the page under an open dialog. Re-validate here, not
        // only when the window opened.
        if state.page_idx >= project.pages.len() {
            self.error_message =
                Some(t!("page_manager.crop_dialog.selection_lost_error").to_string());
            return None;
        }
        if state.page_size.is_none() {
            state.page_size = self.page_pixel_size(state.page_idx, project, page_infos);
        }
        state.ensure_frame();
        // `page_pixel_size` answers from `page_infos` alone, so the page size can
        // be known while the preview decode has FAILED (the page file was replaced
        // or became unreadable after it was first loaded). The board then shows a
        // grey quad and the user would be cropping a page they cannot see — with
        // an operation that applies immediately and is not undone by discarding
        // unsaved changes. Peeked, not touched: the LRU order stays the board's
        // business.
        let preview_failed = matches!(
            self.thumbs
                .preview_state_cached(&project.pages[state.page_idx].path),
            PreviewState::Failed
        );

        let mut keep_open = true;
        let mut close_clicked = false;
        let mut confirm_clicked = false;
        egui::Window::new(t!("page_manager.crop_dialog.title"))
            .id(egui::Id::new("page_manager_crop_dialog"))
            .open(&mut keep_open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(1040.0, 720.0))
            .min_width(760.0)
            .min_height(520.0)
            .show(ctx, |ui| {
                egui::Panel::top("page_manager_crop_settings").show(ui, |ui| {
                    draw_crop_settings(ui, &mut state);
                });
                egui::Panel::bottom("page_manager_crop_actions").show(ui, |ui| {
                    draw_crop_actions(
                        ui,
                        &state,
                        op_in_progress,
                        preview_failed,
                        &mut confirm_clicked,
                        &mut close_clicked,
                    );
                });
                egui::CentralPanel::default().show(ui, |ui| {
                    self.draw_crop_board(ui, &mut state, project);
                });
            });

        if confirm_clicked {
            match build_crop_op(&state) {
                Ok(op) => {
                    actions.push(PageManagerAction::RequestOp(op));
                    return None;
                }
                Err(error) => {
                    // Confirm is disabled while the state is invalid, so this can
                    // only be a race with the very frame it became invalid.
                    self.error_message = Some(layout_error_message(error));
                }
            }
        }
        if !keep_open || close_clicked {
            return None;
        }
        Some(state)
    }

    /// Draws the board: camera and frame input, the rotated page preview, the
    /// veil over the discarded region, and the crop frame with its handles.
    fn draw_crop_board(
        &mut self,
        ui: &mut egui::Ui,
        state: &mut CropDialogState,
        project: &ProjectData,
    ) {
        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, egui::CornerRadius::ZERO, ui.visuals().extreme_bg_color);

        let Some(page_size) = state.page_size else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                t!("page_manager.crop_dialog.loading_size"),
                egui::FontId::proportional(15.0),
                ui.visuals().weak_text_color(),
            );
            // The size arrives through the thumbnail worker; ask for it once per
            // frame until it does (the request is deduplicated by the runtime).
            self.thumbs
                .request_thumb_if_needed(&project.pages[state.page_idx].path, self.generation);
            return;
        };
        let Some(canvas) = state.canvas() else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                t!("page_manager.crop_dialog.rotation_unsupported_error"),
                egui::FontId::proportional(15.0),
                ui.visuals().warn_fg_color,
            );
            return;
        };

        self.handle_crop_board_input(ui, state, rect, &response, canvas);
        let view = state.viewport.transform(rect);
        self.paint_crop_page(ui, state, project, rect, &view, page_size);
        // `ensure_frame` guarantees a frame whenever the canvas is known, so this
        // early return is unreachable; it is the alternative to an `expect`.
        let Some(frame) = state.frame else {
            return;
        };
        let frame_screen = frame_screen_rect(&view, frame);
        paint_crop_overlay(&painter, rect, to_egui_rect(frame_screen));
        paint_crop_frame(&painter, frame_screen);
    }

    /// Applies the first-frame fit, the frame drag, wheel zoom and panning.
    ///
    /// A board drag is a FRAME drag when it started over a grab region and a pan
    /// otherwise; the decision is taken once, on `drag_started`, so a fast drag
    /// that leaves the handle keeps resizing instead of turning into a pan.
    fn handle_crop_board_input(
        &mut self,
        ui: &mut egui::Ui,
        state: &mut CropDialogState,
        rect: egui::Rect,
        response: &egui::Response,
        canvas: [u32; 2],
    ) {
        // Fit the whole canvas on the first frame that has a real rect, so even an
        // 18 000 px ribbon opens fully visible. Re-runs after a rotation, which
        // resizes the canvas.
        if !state.camera_fitted && rect.width() > 1.0 && rect.height() > 1.0 {
            // Infallible on every target this project builds for (32/64-bit
            // usize); a hypothetical 16-bit build would merely fit a clamped size.
            let width = usize::try_from(canvas[0]).unwrap_or(usize::MAX);
            let height = usize::try_from(canvas[1]).unwrap_or(usize::MAX);
            state.viewport.fit_page(rect, [width, height]);
            state.camera_fitted = true;
        }

        let view = state.viewport.transform(rect);
        let frame_screen = state.frame.map(|frame| frame_screen_rect(&view, frame));

        if response.drag_started() {
            // A MIDDLE-button drag ALWAYS pans, whatever it started over. Without
            // it the board can have no pannable pixel at all: as soon as the
            // frame's screen rect covers the board, `hit_test` answers
            // `CropHandle::Move` everywhere, and the DEFAULT full-canvas frame
            // does exactly that at any working zoom — so every attempt to pan
            // would silently shift the crop rect instead of moving the view.
            state.drag = if response.drag_started_by(egui::PointerButton::Middle) {
                None
            } else {
                match (state.frame, frame_screen, response.interact_pointer_pos()) {
                    (Some(frame), Some(screen), Some(pos)) => {
                        // The hit test — and its corner-beats-edge-beats-move
                        // priority — lives in `crop_layout`, where it is
                        // unit-tested against the very rects painted below. Nine
                        // overlapping `ui.interact` rects would hand that priority
                        // to egui's registration order instead, which is not the
                        // order the priority table states.
                        crop_layout::hit_test(screen, HANDLE_SIZE_POINTS, pos.x, pos.y).map(
                            |handle| CropDrag {
                                handle,
                                start_frame: frame,
                                delta: [0.0, 0.0],
                            },
                        )
                    }
                    _ => None,
                }
            };
        }

        let ratio = state.aspect();
        let mut pan = egui::Vec2::ZERO;
        if let Some(mut drag) = state.drag {
            let step = screen_delta_to_canvas(&view, response.drag_delta());
            drag.delta[0] += step[0];
            drag.delta[1] += step[1];
            state.frame = Some(crop_layout::apply_drag_with_ratio(
                drag.start_frame,
                drag.handle,
                drag.delta,
                canvas,
                crop_layout::MIN_FRAME_SIDE_PX,
                ratio,
            ));
            state.drag = Some(drag);
            ui.ctx().set_cursor_icon(cursor_for(drag.handle));
        } else {
            // A drag that started on the middle button, or outside every grab
            // region, is a pan. `drag_delta` is zero unless the board is dragged.
            pan = response.drag_delta();
            // The cursor is what tells the two apart before the button goes down:
            // a hand means "this drag pans", a resize arrow means "this drag
            // crops". The middle button pans from anywhere, which the board hint
            // says in words because no cursor can.
            if response.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            } else if let Some(pos) = response.hover_pos() {
                let over = frame_screen
                    .and_then(|screen| crop_layout::hit_test(screen, HANDLE_SIZE_POINTS, pos.x, pos.y));
                ui.ctx()
                    .set_cursor_icon(over.map_or(egui::CursorIcon::Grab, cursor_for));
            }
        }
        if response.drag_stopped() {
            state.drag = None;
        }

        // Wheel: sign only (see WHEEL_ZOOM_STEP), anchored on the cursor. Skipped
        // while a combo popup is open anywhere — a wheel-aware popup floating over
        // this board owns the notch (`egui-docs/04-widgets.md` §2) — and while a
        // frame drag is in progress, where a zoom change would fight the drag.
        let wheel_y = if response.hovered() && state.drag.is_none() && !combo_popup_open(ui.ctx())
        {
            ui.ctx()
                .input(|input| crate::input_util::raw_wheel_delta(input).y)
        } else {
            0.0
        };
        let wheel_for_zoom = if wheel_y > 0.0 {
            WHEEL_ZOOM_STEP
        } else if wheel_y < 0.0 {
            -WHEEL_ZOOM_STEP
        } else {
            0.0
        };
        let anchor = response.hover_pos().filter(|pos| rect.contains(*pos));
        state
            .viewport
            .handle_input(rect, anchor, wheel_for_zoom, pan);
    }

    /// Paints the page preview (or its placeholder) as the ROTATED quad it
    /// occupies inside the canvas.
    fn paint_crop_page(
        &mut self,
        ui: &mut egui::Ui,
        state: &CropDialogState,
        project: &ProjectData,
        rect: egui::Rect,
        view: &ViewTransform,
        page_size: [u32; 2],
    ) {
        let painter = ui.painter_at(rect);
        let visuals = ui.visuals().clone();
        // Unreachable: the caller already resolved the canvas from the same
        // rotation. Returning is the alternative to an `expect`.
        let Some(rotated) = rotated_page(page_size, state.quarter_turns, state.angle_deg) else {
            return;
        };
        let quad = page_quad_screen(&rotated, view);

        let path = &project.pages[state.page_idx].path;
        self.thumbs
            .request_preview_if_needed(path, SPLIT_PREVIEW_LONG_SIDE_PX, self.generation);
        let preview = self.thumbs.preview_state(path);
        match preview {
            // A degenerate texture would sample garbage over the whole quad, so
            // it is treated as a failed decode rather than painted.
            PreviewState::Ready { texture, size, .. } if size.x > 0.0 && size.y > 0.0 => {
                paint_textured_quad(&painter, texture, quad, egui::Color32::WHITE);
            }
            PreviewState::Ready { .. } | PreviewState::Pending | PreviewState::Failed => {
                painter.add(egui::Shape::convex_polygon(
                    quad.to_vec(),
                    visuals.widgets.noninteractive.weak_bg_fill,
                    egui::Stroke::NONE,
                ));
                let caption = match preview {
                    PreviewState::Failed => t!("page_manager.crop_dialog.preview_failed"),
                    PreviewState::Pending | PreviewState::Ready { .. } => {
                        t!("page_manager.crop_dialog.preview_loading")
                    }
                };
                painter.text(
                    quad_center(quad),
                    egui::Align2::CENTER_CENTER,
                    caption,
                    egui::FontId::proportional(13.0),
                    visuals.weak_text_color(),
                );
            }
        }
        // The page outline: four segments, because the quad is rotated and
        // `rect_stroke` draws an axis-aligned rectangle only.
        let outline = egui::Stroke::new(1.0, visuals.widgets.inactive.fg_stroke.color);
        for (from, to) in [(0, 1), (1, 2), (2, 3), (3, 0)] {
            painter.line_segment([quad[from], quad[to]], outline);
        }
    }
}

/// Centre of a quad, as the mean of its corners. Exact for the parallelograms a
/// rotation produces.
#[must_use]
fn quad_center(quad: [egui::Pos2; 4]) -> egui::Pos2 {
    let sum = quad
        .iter()
        .fold(egui::Vec2::ZERO, |acc, corner| acc + corner.to_vec2());
    (sum / 4.0).to_pos2()
}

/// Paints a texture over an arbitrary quad as a two-triangle `epaint::Mesh`.
///
/// `egui::Painter::image` draws an AXIS-ALIGNED quad only, so a rotated preview
/// has to go through a mesh; this mirrors `launcher/app.rs::paint_rotated_image`,
/// except that the corners arrive already rotated by the engine's mapping instead
/// of being rotated here. UV corners are fixed `(0,0) (1,0) (1,1) (0,1)`, which
/// is why `page_quad_screen` documents its corner ORDER as part of its contract.
fn paint_textured_quad(
    painter: &egui::Painter,
    texture_id: egui::TextureId,
    quad: [egui::Pos2; 4],
    tint: egui::Color32,
) {
    let uv = [
        egui::pos2(0.0, 0.0),
        egui::pos2(1.0, 0.0),
        egui::pos2(1.0, 1.0),
        egui::pos2(0.0, 1.0),
    ];
    let mut mesh = epaint::Mesh::with_texture(texture_id);
    mesh.reserve_vertices(quad.len());
    for (pos, uv) in quad.into_iter().zip(uv) {
        mesh.vertices.push(epaint::Vertex { pos, uv, color: tint });
    }
    // The mesh is fresh, so the four vertices are at indices 0..4.
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// Veils everything the crop DISCARDS: the board minus the kept region.
///
/// Painted as four bands around the frame rather than as one veil plus a
/// "brighten" pass, so the kept region is the untouched preview and the user
/// judges the crop on the real pixels.
fn paint_crop_overlay(painter: &egui::Painter, board: egui::Rect, frame: egui::Rect) {
    let shade = egui::Color32::from_black_alpha(DIM_ALPHA);
    let kept = frame.intersect(board);
    if !kept.is_positive() {
        // The frame is entirely off-board (the user panned away): nothing is kept
        // on screen, so nothing is left bright.
        painter.rect_filled(board, egui::CornerRadius::ZERO, shade);
        return;
    }
    let bands = [
        egui::Rect::from_min_max(board.min, egui::pos2(board.max.x, kept.min.y)),
        egui::Rect::from_min_max(egui::pos2(board.min.x, kept.max.y), board.max),
        egui::Rect::from_min_max(
            egui::pos2(board.min.x, kept.min.y),
            egui::pos2(kept.min.x, kept.max.y),
        ),
        egui::Rect::from_min_max(
            egui::pos2(kept.max.x, kept.min.y),
            egui::pos2(board.max.x, kept.max.y),
        ),
    ];
    for band in bands {
        if band.is_positive() {
            painter.rect_filled(band, egui::CornerRadius::ZERO, shade);
        }
    }
}

/// Paints the crop frame: its border, the rule-of-thirds guides and the eight
/// grab handles.
///
/// The handles come from `crop_layout::handle_rects` — the same rects the hit
/// test uses — so what the user sees is exactly what is grabbable.
fn paint_crop_frame(painter: &egui::Painter, frame: ScreenRect) {
    let rect = to_egui_rect(frame);
    let guide = egui::Stroke::new(1.0, GUIDE_COLOR);
    for fraction in [1.0 / 3.0, 2.0 / 3.0] {
        let x = rect.min.x + rect.width() * fraction;
        let y = rect.min.y + rect.height() * fraction;
        painter.line_segment([egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)], guide);
        painter.line_segment([egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y)], guide);
    }
    painter.rect_stroke(
        rect,
        egui::CornerRadius::ZERO,
        egui::Stroke::new(1.5, FRAME_COLOR),
        egui::StrokeKind::Inside,
    );
    for (_, handle_rect) in crop_layout::handle_rects(frame, HANDLE_SIZE_POINTS) {
        let handle = to_egui_rect(handle_rect);
        painter.rect_filled(handle, egui::CornerRadius::same(2), HANDLE_FILL);
        painter.rect_stroke(
            handle,
            egui::CornerRadius::same(2),
            egui::Stroke::new(1.0, HANDLE_STROKE),
            egui::StrokeKind::Inside,
        );
    }
}

/// Draws the settings strip: the quarter-turn buttons, the fine straightening
/// angle, the rotation reset, the aspect-ratio presets and the board hint.
fn draw_crop_settings(ui: &mut egui::Ui, state: &mut CropDialogState) {
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("page_manager.crop_dialog.rotation_label"));
        if ui
            .button(t!("page_manager.crop_dialog.rotate_ccw_button"))
            .clicked()
        {
            state.set_rotation(i32::from(state.quarter_turns) - 1, state.angle_deg);
        }
        if ui
            .button(t!("page_manager.crop_dialog.rotate_cw_button"))
            .clicked()
        {
            state.set_rotation(i32::from(state.quarter_turns) + 1, state.angle_deg);
        }
        ui.separator();
        ui.label(t!("page_manager.crop_dialog.fine_angle_label"));
        // Two widgets over ONE value: the slider is for finding the angle by eye
        // against the page, the spin box for entering or nudging an exact one.
        // They step and display at the SAME precision (`FINE_ANGLE_STEP_DEG`),
        // which is what stops the slider's clamping from re-snapping — and thus
        // silently coarsening — a value typed into the spin box.
        // The range is the engine's own bound INCLUSIVE, so reaching ±45 exactly
        // is legal and `set_rotation` rolls it into the next quarter turn rather
        // than refusing it.
        let limit = crop_layout::MAX_FINE_ANGLE_DEG;
        let mut angle = state.angle_deg;
        ui.add(
            WheelSlider::new(&mut angle, -limit..=limit)
                .step_by(FINE_ANGLE_STEP_DEG)
                .wheel_step(FINE_ANGLE_WHEEL_STEP_DEG)
                .fixed_decimals(FINE_ANGLE_DECIMALS)
                .show_value(false),
        );
        ui.add(
            WheelSpinBox::new(&mut angle)
                .speed(FINE_ANGLE_DRAG_SPEED)
                .wheel_step(FINE_ANGLE_WHEEL_STEP_DEG)
                .range(-limit..=limit)
                .fixed_decimals(FINE_ANGLE_DECIMALS),
        );
        // `set_rotation` is a no-op when nothing changed, so an untouched frame
        // costs one comparison and never re-fits the frame.
        state.set_rotation(i32::from(state.quarter_turns), angle);
        ui.separator();
        if ui
            .button(t!("page_manager.crop_dialog.reset_rotation_button"))
            .clicked()
        {
            state.set_rotation(0, 0.0);
        }
    });
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("page_manager.crop_dialog.ratio_label"));
        // A radio row, not a combo box: a ratio change RE-FITS the frame, and a
        // wheel-cycling combo would discard the user's framing on a stray notch
        // over a closed picker (the hazard `split.rs` documents for its own
        // picker). Three presets fit a row comfortably.
        let mut ratio = state.ratio;
        for candidate in [
            RatioChoice::Free,
            RatioChoice::Original,
            RatioChoice::Square,
        ] {
            ui.radio_value(&mut ratio, candidate, ratio_label(candidate));
        }
        state.set_ratio(ratio);
        ui.separator();
        if ui
            .button(t!("page_manager.crop_dialog.fit_inscribed_button"))
            .on_hover_text(t!("page_manager.crop_dialog.fit_inscribed_tooltip"))
            .clicked()
        {
            state.fit_inscribed();
        }
    });
    ui.add_space(4.0);
    ui.add(
        egui::Label::new(egui::RichText::new(t!("page_manager.crop_dialog.board_hint")).weak())
            .selectable(false)
            .wrap(),
    );
    ui.add_space(4.0);
}

/// Draws the bottom strip: the resulting page size, the validation message, the
/// resampling warning, the "applied immediately" warning, and the confirm /
/// cancel buttons.
///
/// `preview_failed` disables the confirm and explains why: a crop is immediate
/// and irreversible, so it is never offered over a page the board could not
/// render.
fn draw_crop_actions(
    ui: &mut egui::Ui,
    state: &CropDialogState,
    op_in_progress: bool,
    preview_failed: bool,
    confirm_clicked: &mut bool,
    close_clicked: &mut bool,
) {
    let validation = validate_state(state);
    ui.add_space(6.0);
    match (validation, state.frame) {
        (Ok(()), Some(frame)) => {
            ui.label(tf!(
                "page_manager.crop_dialog.result_size_label",
                width = frame.width(),
                height = frame.height()
            ));
        }
        // A frame is always present once the canvas is known, so the `None` arm
        // is the same "still loading" state `CanvasEmpty` reports.
        (Ok(()), None) => {
            ui.label(t!("page_manager.crop_dialog.loading_size"));
        }
        (Err(error), _) => {
            ui.colored_label(ui.visuals().warn_fg_color, layout_error_message(error));
        }
    }
    // A quarter turn is a lossless pixel permutation; any other angle is not, and
    // the losses are not visible on the board. Say so before the confirm.
    if !state.is_lossless() {
        ui.add(
            egui::Label::new(
                egui::RichText::new(t!("page_manager.crop_dialog.resample_warning"))
                    .color(ui.visuals().warn_fg_color),
            )
            .wrap(),
        );
    }
    if preview_failed {
        ui.add(
            egui::Label::new(
                egui::RichText::new(t!("page_manager.crop_dialog.preview_failed_error"))
                    .color(ui.visuals().warn_fg_color),
            )
            .wrap(),
        );
    }
    ui.add_space(4.0);
    ui.add(
        egui::Label::new(egui::RichText::new(t!(
            "page_manager.crop_dialog.apply_warning"
        )))
        .wrap(),
    );
    ui.add(
        egui::Label::new(tf!(
            "page_manager.crop_dialog.trash_note",
            dir = super::dialogs::PAGE_OP_TRASH_DIR
        ))
        .wrap(),
    );
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                confirm_enabled(op_in_progress, preview_failed, validation),
                egui::Button::new(t!("page_manager.crop_dialog.confirm_button")),
            )
            .clicked()
        {
            *confirm_clicked = true;
        }
        if ui.button(t!("page_manager.dialog.cancel_button")).clicked() {
            *close_clicked = true;
        }
    });
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A portrait page, so every quarter turn is visible in the canvas size.
    const PAGE: [u32; 2] = [800, 1200];

    /// The window's own default board: `default_size(1040, 720)` minus the top
    /// and bottom strips.
    fn board() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1024.0, 540.0))
    }

    /// A state that already knows its page size and has its frame seeded, i.e.
    /// the state every frame of the window works on.
    fn ready_state() -> CropDialogState {
        let mut state = CropDialogState::new(3);
        state.page_size = Some(PAGE);
        state.ensure_frame();
        state
    }

    /// A camera looking at the canvas at an exact zoom, with no float slack.
    fn view(zoom: f32) -> ViewTransform {
        ViewTransform {
            viewport_rect: board(),
            zoom,
            center_world: egui::vec2(600.0, 400.0),
        }
    }

    /// The state's canvas. A test whose state has none is itself broken, which is
    /// why this panics with the reason rather than unwrapping.
    fn canvas_of(state: &CropDialogState) -> [u32; 2] {
        match state.canvas() {
            Some(canvas) => canvas,
            None => panic!("the test state must know its canvas"),
        }
    }

    /// The state's frame, with the same contract as [`canvas_of`].
    fn frame_of(state: &CropDialogState) -> CropFrame {
        match state.frame {
            Some(frame) => frame,
            None => panic!("the test state must hold a frame"),
        }
    }

    /// A frame that must be valid; panics with the reason when a test builds a
    /// bad one, which is a defect in the test rather than in the module.
    fn frame_in(canvas: [u32; 2], rect: [u32; 4]) -> CropFrame {
        match CropFrame::new(canvas, rect) {
            Ok(built) => built,
            Err(error) => panic!("test frame {rect:?} is invalid in canvas {canvas:?}: {error}"),
        }
    }

    #[test]
    fn the_canvas_of_a_quarter_turn_is_the_turned_page() {
        let mut state = ready_state();
        for (turns, expected) in [(0u8, PAGE), (1, [1200, 800]), (2, PAGE), (3, [1200, 800])] {
            state.set_rotation(i32::from(turns), 0.0);
            assert_eq!(state.quarter_turns, turns);
            assert_eq!(state.canvas(), Some(expected));
            assert_eq!(state.turned_page(), Some(expected));
        }
    }

    #[test]
    fn a_fine_angle_grows_the_canvas_past_the_page() {
        let mut state = ready_state();
        state.set_rotation(0, 30.0);
        let canvas = canvas_of(&state);
        assert!(
            canvas[0] > PAGE[0] && canvas[1] > PAGE[1],
            "the bounding box of a rotated page must exceed it: {canvas:?}"
        );
        assert!(validate_state(&state).is_ok());
    }

    #[test]
    fn a_quarter_turn_rebuilds_the_frame_and_refits_the_camera() {
        let mut state = ready_state();
        state.viewport.set_camera(2.0, egui::vec2(100.0, 100.0));
        state.camera_fitted = true;
        state.frame = Some(frame_in(PAGE, [10, 20, 300, 400]));
        state.set_rotation(1, 0.0);
        // The canvas transposed, so the old coordinates mean something else:
        // both the frame and the camera start over.
        assert!(!state.camera_fitted, "a quarter turn must re-fit the camera");
        assert_eq!(frame_of(&state).rect(), [0, 0, 1200, 800]);
    }

    #[test]
    fn a_fine_angle_preserves_the_frame_and_the_camera() {
        let mut state = ready_state();
        state.viewport.set_camera(2.0, egui::vec2(400.0, 600.0));
        state.camera_fitted = true;
        let before_canvas = canvas_of(&state);
        state.frame = Some(frame_in(before_canvas, [100, 200, 300, 400]));

        state.set_rotation(0, 0.5);

        // Straightening by eye at working zoom is the whole point of the control:
        // the zoom must survive, the frame must keep its size, and both must move
        // with the page by half the canvas growth.
        assert!(state.camera_fitted, "a fine angle must not re-fit the camera");
        assert!((state.viewport.zoom() - 2.0).abs() < 1e-6);
        let after_canvas = canvas_of(&state);
        let shift = canvas_shift(before_canvas, after_canvas);
        let frame = frame_of(&state);
        assert_eq!([frame.width(), frame.height()], [300, 400]);
        assert_eq!(
            frame.rect(),
            crop_layout::recentre_frame(
                frame_in(before_canvas, [100, 200, 300, 400]),
                before_canvas,
                after_canvas,
                crop_layout::MIN_FRAME_SIDE_PX,
            )
            .rect()
        );
        let centre = state.viewport.center_world();
        assert!((centre.x - (400.0 + shift.x)).abs() < 1e-3, "{centre:?}");
        assert!((centre.y - (600.0 + shift.y)).abs() < 1e-3, "{centre:?}");
    }

    #[test]
    fn the_angle_controls_share_one_precision() {
        // The slider used to step more coarsely than the spin box displayed, so
        // its clamping silently re-quantized a typed value every frame.
        assert!((quantize_angle(12.34) - 12.34).abs() < 1e-9);
        assert!((quantize_angle(0.02) - 0.02).abs() < 1e-9);
        assert!((quantize_angle(0.004) - 0.0).abs() < 1e-9);
        assert!((quantize_angle(f64::NAN) - 0.0).abs() < 1e-9);
        // And a stored angle survives a round trip through the state unchanged.
        let mut state = ready_state();
        state.set_rotation(0, 12.34);
        assert!((state.angle_deg - 12.34).abs() < 1e-9, "{}", state.angle_deg);
        state.set_rotation(0, state.angle_deg);
        assert!((state.angle_deg - 12.34).abs() < 1e-9, "{}", state.angle_deg);
    }

    #[test]
    fn a_screen_delta_becomes_canvas_pixels_at_the_board_zoom() {
        // Half zoom: one screen point is two canvas pixels.
        let delta = screen_delta_to_canvas(&view(0.5), egui::vec2(10.0, -6.0));
        assert!((delta[0] - 20.0).abs() < 1e-9, "{delta:?}");
        assert!((delta[1] + 12.0).abs() < 1e-9, "{delta:?}");
        // Double zoom: one screen point is half a canvas pixel.
        let delta = screen_delta_to_canvas(&view(2.0), egui::vec2(10.0, -6.0));
        assert!((delta[0] - 5.0).abs() < 1e-9, "{delta:?}");
        assert!((delta[1] + 3.0).abs() < 1e-9, "{delta:?}");
    }

    #[test]
    fn a_drag_moves_the_frame_by_the_delta_in_canvas_pixels() {
        let mut state = ready_state();
        let canvas = canvas_of(&state);
        // Well inside the canvas on every side, so the move below is not clamped
        // by an edge — which is what this test is about.
        state.frame = Some(frame_in(canvas, [100, 100, 500, 900]));
        let before = frame_of(&state);
        let delta = screen_delta_to_canvas(&view(0.5), egui::vec2(20.0, 0.0));
        let after = crop_layout::apply_drag_with_ratio(
            before,
            CropHandle::Move,
            delta,
            canvas,
            crop_layout::MIN_FRAME_SIDE_PX,
            state.aspect(),
        );
        // 20 points at zoom 0.5 are 40 canvas pixels.
        assert_eq!(after.x(), before.x() + 40);
        assert_eq!(after.y(), before.y());
        assert_eq!(after.width(), before.width());
    }

    #[test]
    fn a_full_canvas_frame_leaves_no_pannable_pixel() {
        // The reason the board needs an explicit pan affordance: once the frame's
        // screen rect covers the board, EVERY board pixel hit-tests as `Move`, so
        // a drag anywhere would shift the crop instead of the view. The default
        // full-canvas frame does exactly that at any working zoom.
        let state = ready_state();
        let mut viewport = PsViewport::default();
        viewport.set_camera(2.0, egui::vec2(400.0, 600.0));
        let screen = frame_screen_rect(&viewport.transform(board()), frame_of(&state));
        let board = board();
        for probe in [
            board.left_top(),
            board.right_top(),
            board.left_bottom(),
            board.right_bottom(),
            board.center(),
        ] {
            assert_eq!(
                crop_layout::hit_test(screen, HANDLE_SIZE_POINTS, probe.x, probe.y),
                Some(CropHandle::Move),
                "{probe:?} is not covered by the frame"
            );
        }
    }

    #[test]
    fn the_emitted_op_carries_exactly_what_the_window_shows() {
        let mut state = ready_state();
        state.set_rotation(1, 0.0);
        let canvas = canvas_of(&state);
        state.frame = Some(frame_in(canvas, [10, 20, 300, 400]));
        let op = match build_crop_op(&state) {
            Ok(op) => op,
            Err(error) => panic!("a legal state must build an op: {error}"),
        };
        assert_eq!(
            op,
            PageOpKind::Crop {
                page_idx: 3,
                quarter_turns: 1,
                angle_deg: 0.0,
                rect: [10, 20, 300, 400],
            }
        );
        // What the confirm strip reports is the same rect.
        let frame = frame_of(&state);
        assert_eq!([frame.width(), frame.height()], [300, 400]);
    }

    #[test]
    fn the_confirm_is_refused_while_the_page_size_is_unknown() {
        let state = CropDialogState::new(0);
        assert!(state.canvas().is_none());
        assert!(matches!(
            validate_state(&state),
            Err(CropLayoutError::CanvasEmpty { .. })
        ));
        assert!(build_crop_op(&state).is_err());
    }

    #[test]
    fn the_confirm_is_refused_when_the_frame_left_the_canvas() {
        let mut state = ready_state();
        // A frame legal for the upright page, then a quarter turn WITHOUT the
        // re-fit `set_rotation` normally performs: the canvas is now 1200x800 and
        // a frame reaching y = 1100 no longer fits.
        state.frame = Some(frame_in(PAGE, [0, 100, 700, 1000]));
        state.quarter_turns = 1;
        assert!(matches!(
            validate_state(&state),
            Err(CropLayoutError::FrameOutsideCanvas { .. })
        ));
        // The invariant restores it instead of leaving the confirm broken.
        state.ensure_frame();
        assert!(validate_state(&state).is_ok());
    }

    #[test]
    fn the_confirm_gate_refuses_a_running_op_and_an_undecodable_preview() {
        let state = ready_state();
        let ok = validate_state(&state);
        assert!(ok.is_ok());
        assert!(confirm_enabled(false, false, ok));
        // A crop is immediate and irreversible, so it is never offered over a
        // page the board could not render, nor while another op is running.
        assert!(!confirm_enabled(false, true, ok));
        assert!(!confirm_enabled(true, false, ok));
        assert!(!confirm_enabled(true, true, ok));
        // Nor over a state the engine would refuse.
        let broken = validate_state(&CropDialogState::new(0));
        assert!(!confirm_enabled(false, false, broken));
    }

    #[test]
    fn only_a_non_zero_fine_angle_is_a_resampling_rotation() {
        let mut state = ready_state();
        assert!(state.is_lossless());
        for turns in 1..=3 {
            state.set_rotation(turns, 0.0);
            assert!(state.is_lossless(), "a quarter turn stays lossless");
        }
        state.set_rotation(0, 0.5);
        assert!(!state.is_lossless());
        state.set_rotation(0, 0.0);
        assert!(state.is_lossless(), "the reset returns to lossless");
    }

    #[test]
    fn straightening_past_the_boundary_rolls_into_the_next_quarter_turn() {
        let mut state = ready_state();
        state.set_rotation(0, 45.0);
        assert_eq!(state.quarter_turns, 1);
        assert!(
            state.angle_deg.abs() < crop_layout::MAX_FINE_ANGLE_DEG,
            "the residual angle must stay inside the engine's bound: {}",
            state.angle_deg
        );
        assert!(validate_state(&state).is_ok());
    }

    #[test]
    fn the_original_ratio_follows_the_quarter_turn() {
        assert_eq!(
            aspect_ratio(RatioChoice::Original, Some(PAGE), 0),
            AspectRatio::Locked { w: 800, h: 1200 }
        );
        assert_eq!(
            aspect_ratio(RatioChoice::Original, Some(PAGE), 1),
            AspectRatio::Locked { w: 1200, h: 800 }
        );
        assert_eq!(
            aspect_ratio(RatioChoice::Square, Some(PAGE), 1),
            AspectRatio::Locked { w: 1, h: 1 }
        );
        assert_eq!(
            aspect_ratio(RatioChoice::Free, Some(PAGE), 1),
            AspectRatio::Free
        );
        // No page size yet: never locked to a ratio derived from nothing.
        assert_eq!(
            aspect_ratio(RatioChoice::Original, None, 0),
            AspectRatio::Free
        );
    }

    #[test]
    fn a_ratio_preset_refits_the_frame_to_it() {
        let mut state = ready_state();
        state.set_ratio(RatioChoice::Square);
        let frame = frame_of(&state);
        assert_eq!(frame.width(), frame.height());
        assert_eq!(frame.width(), PAGE[0], "a square fits the page's short side");
    }

    #[test]
    fn the_fit_button_inscribes_the_frame_in_the_rotated_page() {
        let mut state = ready_state();
        // An unrotated page leaves no empty corner, so the fit is the whole page.
        state.fit_inscribed();
        assert_eq!(frame_of(&state).rect(), [0, 0, PAGE[0], PAGE[1]]);

        state.set_rotation(0, 10.0);
        let canvas = canvas_of(&state);
        state.fit_inscribed();
        let fitted = frame_of(&state);
        assert!(
            fitted.width() < canvas[0] && fitted.height() < canvas[1],
            "the inscribed fit must be smaller than the rotated canvas: {fitted:?} in {canvas:?}"
        );
        assert!(validate_state(&state).is_ok());
        // It must also be smaller than the PAGE on at least one axis — the whole
        // point is to exclude the wedges the rotation left empty.
        assert!(fitted.width() < PAGE[0] || fitted.height() < PAGE[1]);
    }

    #[test]
    fn the_page_quad_follows_the_rotation() {
        let Some(rotated) = rotated_page(PAGE, 1, 0.0) else {
            panic!("a quarter turn of a real page is a legal rotation");
        };
        assert_eq!(rotated.canvas_size(), [1200, 800]);
        let view = view(1.0);
        let quad = page_quad_screen(&rotated, &view);
        // A clockwise quarter turn sends the page's top-left corner to the
        // canvas' top-RIGHT corner and its top-right corner to the bottom-right.
        let top_left = view.world_to_screen(egui::pos2(1200.0, 0.0));
        let top_right = view.world_to_screen(egui::pos2(1200.0, 800.0));
        assert!((quad[0] - top_left).length() < 1e-3, "{quad:?}");
        assert!((quad[1] - top_right).length() < 1e-3, "{quad:?}");
    }

    #[test]
    fn the_frame_screen_rect_tracks_the_camera() {
        let mut state = ready_state();
        let canvas = canvas_of(&state);
        state.frame = Some(frame_in(canvas, [100, 200, 300, 400]));
        let view = view(0.5);
        let screen = frame_screen_rect(&view, frame_of(&state));
        assert!((screen.width() - 150.0).abs() < 1e-3, "{screen:?}");
        assert!((screen.height() - 200.0).abs() < 1e-3, "{screen:?}");
        // The rect the hit test sees is the rect the handles are painted on.
        let handles = crop_layout::handle_rects(screen, HANDLE_SIZE_POINTS);
        let (handle, rect) = handles[0];
        assert_eq!(handle, CropHandle::TopLeft);
        assert_eq!(
            crop_layout::hit_test(screen, HANDLE_SIZE_POINTS, rect.center().0, rect.center().1),
            Some(CropHandle::TopLeft)
        );
    }
}
