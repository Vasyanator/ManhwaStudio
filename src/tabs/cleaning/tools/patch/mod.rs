/*
File: tabs/cleaning/tools/patch/mod.rs

Purpose:
The «Заплатка» cleaning tool — Photoshop's Patch Tool. The user draws a free-form (lasso) or
rectangular selection directly on the page canvas, then drags from inside it onto a clean SOURCE
area; on release the source pixels are copied into the selection and colour-adapted to the
destination's contour by the gradient-domain solve in `membrane.rs`.

Main responsibilities:
- Own the selection (in PAGE pixels, re-projected every frame) and the two-phase gesture.
- Turn a finished drag into an off-thread job: load the ROI, solve the membrane, build the
  clean-overlay chunk, commit it as ONE undo step.
- Paint the dashed selection outlines and the tool's controls.

Key structures:
- `PatchTool`: the `CleaningTool` implementation and all of its state.
- `PatchShape`: free-form or rectangular selection.
- `PatchSelection`: the closed polygon plus the page it belongs to.
- `PatchGesture`: the live gesture (lasso, rectangle drag, source drag).
- `PendingLoad` / `PatchOutcome`: the two worker hand-offs.
- `DragGeometry` / `DragRefusal`: what a source drag resolves to, or why it is refused.
- `PatchInputError` / `ApplyError`: the worker's and the commit's refusal reasons.

Key functions:
- `drag_geometry()`: THE refusal predicate — the preview outline and the release both read it.
- `start_job()`: the two region-loader requests for an accepted drag.
- `run_patch_job()`: the whole worker-side pass (rasterize, solve, build the overlay chunk).
- `gesture_for_press()`: the whole press-time state machine, canvas-free and unit-tested.
- `resolve_pending_end()`: turns a parked `stroke_end` into a commit or a cancellation.
- `clear_selection()`: drops the selection AND cancels the work in flight (Escape, the button).
- `check_chunk_fits()`: the bounds guard run against the LIVE overlay before the write.
- `paint_outline()`: the zoom-invariant two-tone dashed outline, unclamped and marked when refused.
- `scene_pos_in_page()`: the unclamped page-pixel -> scene map that outline is projected with.

Notes:
This tool is NOT a region editor: no floating window, no main dock panel. It rides the tab's
ordinary stroke pipeline, which is why `captures_canvas_pointer()` and `block_canvas_zoom()` both
stay `false` — see the tests at the bottom of this file for what each of them would break.
*/
mod membrane;

use super::base::{
    BrushToolBase, CleaningTool, RegionLoadRequest, RegionLoadResult, StrokePoint,
    extract_overlay_chunk, overlay_pixel_for_final_color, scene_pos_to_overlay_pos,
    spawn_region_loader_thread,
};
use super::region_edit_v2::frame::page_source_size;
use super::region_edit_v2::geometry::usable_viewport_for;
use crate::canvas::{CanvasView, OverlayRectPx};
use crate::project::ProjectData;
use crate::tools::fill_polygon_spans;
use crate::widgets::WheelSlider;
use eframe::egui;
use egui::{Color32, Pos2};
use membrane::{PatchBlend, PatchRequest, solve_patch};
use ms_thread::{self as thread, JoinHandle};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

/// Smallest bounding-box span, in page pixels, a selection must have on BOTH axes.
///
/// Below this a "selection" is a stray click or a one-pixel line: it has no interior for the
/// solve and no visible outline, so it is rejected instead of left half-built.
const MIN_SELECTION_SPAN_PX: f32 = 2.0;

/// Minimum distance between two consecutive lasso vertices, in page pixels.
///
/// The same rule the PS-editor's lasso uses: without it a slow drag stores thousands of
/// coincident vertices, which cost memory and make the outline's dash spacing meaningless.
const VERTEX_MIN_STEP_PX: f32 = 2.0;

/// Padding around the solve's ROI, in page pixels.
///
/// The shared SOR kernel never writes its 1-pixel border, and the membrane needs a ring of known
/// destination values immediately outside the selection, so two pixels is the minimum that gives
/// both. Also the reason a selection whose padded ROI leaves the page is refused.
const ROI_PAD_PX: usize = 2;

/// Upper bound of the feather control, in page pixels.
const MAX_FEATHER_PX: usize = 32;

/// Target number of dashes around a selection outline.
///
/// The dash LENGTH is derived from the outline's on-screen perimeter divided by this, so the
/// number of dashes stays put at any zoom instead of the dashes growing into a solid line.
const OUTLINE_DASH_COUNT: f32 = 48.0;

/// Floor for a single dash, in screen points, so a tiny selection still reads as dashed.
const OUTLINE_MIN_DASH: f32 = 3.0;

/// The dark half of the two-tone outline. Paired with white, the outline survives both black
/// line art and white paper — the same rule `stamp.rs`'s source ring follows.
const OUTLINE_DARK: Color32 = Color32::from_rgb(20, 20, 20);

/// The light half of an outline whose release would be REFUSED.
///
/// The same red the sibling region editor paints a frame with whose size its consumer rejects
/// (`FRAME_INVALID_COLOR` in `../region_edit_v2/render.rs`, itself `FLUX2_STATUS_ERROR_COLOR`),
/// used the same way: the dark backing dash stays, only the light half changes colour, so the
/// project keeps ONE meaning for "this state is refused" on the canvas.
const OUTLINE_REFUSED: Color32 = Color32::from_rgb(255, 120, 120);

/// Which shape the next selection is drawn with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PatchShape {
    /// Free-form lasso: every stroke-move appends a vertex.
    #[default]
    Lasso,
    /// Axis-aligned rectangle spanned by the drag.
    Rect,
}

impl PatchShape {
    /// Localized caption of the shape button.
    fn title(self) -> &'static str {
        match self {
            Self::Lasso => t!("cleaning.tools.patch.shape_free"),
            Self::Rect => t!("cleaning.tools.patch.shape_rect"),
        }
    }
}

/// Localized caption of a colour-adaptation button.
fn blend_title(blend: PatchBlend) -> &'static str {
    match blend {
        PatchBlend::None => t!("cleaning.tools.patch.blend_none"),
        PatchBlend::Linear => t!("cleaning.tools.patch.blend_linear"),
        PatchBlend::Multiplicative => t!("cleaning.tools.patch.blend_multiplicative"),
    }
}

/// A closed selection, stored in PAGE pixels so it survives scrolling and zooming.
///
/// Screen positions are re-derived every frame from the page's current scene rect; a stored
/// screen rectangle would drift the moment the canvas moves.
#[derive(Debug, Clone)]
struct PatchSelection {
    page_idx: usize,
    /// Vertices of an implicitly closed polygon, in page pixels.
    points: Vec<(f32, f32)>,
}

impl PatchSelection {
    /// The selection's bounding box in page pixels, as `(min_x, min_y, max_x, max_y)`.
    ///
    /// `None` for a polygon with fewer than three vertices, which is not a selection.
    fn bounds(&self) -> Option<(f32, f32, f32, f32)> {
        if self.points.len() < 3 {
            return None;
        }
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        for (x, y) in &self.points {
            min_x = min_x.min(*x);
            min_y = min_y.min(*y);
            max_x = max_x.max(*x);
            max_y = max_y.max(*y);
        }
        Some((min_x, min_y, max_x, max_y))
    }

    /// Whether a page-pixel position lies inside the polygon, by the even-odd rule.
    ///
    /// The same even-odd rule `fill_polygon_spans` rasterizes with, but sampled at the exact
    /// position rather than at the scanline centre the rasterizer uses (`y + 0.5`,
    /// `tools/polygon_mask.rs`). The two therefore answer differently for a press within half a
    /// pixel of a near-horizontal edge. That is harmless: this test only decides WHICH gesture a
    /// press starts, never which pixels a patch covers.
    fn contains(&self, x: f32, y: f32) -> bool {
        let mut inside = false;
        let count = self.points.len();
        if count < 3 {
            return false;
        }
        for idx in 0..count {
            let (x0, y0) = self.points[idx];
            let (x1, y1) = self.points[(idx + 1) % count];
            // Half-open vertical test, so a vertex exactly on the ray is counted once.
            if (y0 <= y && y1 > y) || (y1 <= y && y0 > y) {
                let t = (y - y0) / (y1 - y0);
                if x < x0 + t * (x1 - x0) {
                    inside = !inside;
                }
            }
        }
        inside
    }
}

/// The gesture currently in flight.
#[derive(Debug, Clone)]
enum PatchGesture {
    /// A free-form selection being drawn.
    Lasso {
        page_idx: usize,
        points: Vec<(f32, f32)>,
    },
    /// A rectangular selection being spanned.
    Rect {
        page_idx: usize,
        start: (f32, f32),
        current: (f32, f32),
    },
    /// A finished selection being dragged onto its source.
    Source {
        page_idx: usize,
        start: (f32, f32),
        current: (f32, f32),
    },
}

impl PatchGesture {
    /// The page the gesture started on. A gesture never spans two pages — see `stroke_begin`.
    fn page_idx(&self) -> usize {
        match self {
            Self::Lasso { page_idx, .. }
            | Self::Rect { page_idx, .. }
            | Self::Source { page_idx, .. } => *page_idx,
        }
    }

    /// A stable English name for the log; never shown to the user.
    fn kind(&self) -> &'static str {
        match self {
            Self::Lasso { .. } => "lasso",
            Self::Rect { .. } => "rectangle",
            Self::Source { .. } => "source drag",
        }
    }
}

/// Which of the worker's three ROI-sized inputs disagreed with the ROI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchBuffer {
    /// The page composited with the clean overlay — the membrane's destination.
    Composite,
    /// The ORIGINAL page pixels under the ROI — `overlay_pixel_for_final_color`'s base.
    Page,
    /// The clean-overlay pixels the patch is written into.
    OverlayChunk,
}

impl std::fmt::Display for PatchBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Composite => "composited destination region",
            Self::Page => "original page region",
            Self::OverlayChunk => "clean-overlay chunk",
        };
        f.write_str(name)
    }
}

/// Why the worker refused to build a patch chunk.
///
/// None of these is recoverable by guessing. Substituting a value for a buffer whose size the
/// worker cannot explain would commit that guess over the user's cleaning work — an
/// all-transparent stand-in for the clean-overlay chunk erases every pre-existing pixel of the
/// ROI — so every one of them is REFUSED and reported instead (§14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum PatchInputError {
    /// The ROI has no pixels at all, so nothing can be solved or written.
    #[error("the ROI is {w}x{h}, which has no pixels")]
    EmptyRoi { w: usize, h: usize },
    /// One of the three ROI-sized inputs is not exactly `roi.w` x `roi.h`.
    #[error(
        "the {buffer} is {got_w}x{got_h} with {got_len} pixels, expected {want_w}x{want_h} with {want_len}"
    )]
    BufferSize {
        buffer: PatchBuffer,
        got_w: usize,
        got_h: usize,
        got_len: usize,
        want_w: usize,
        want_h: usize,
        want_len: usize,
    },
    /// The solver answered with buffers that do not cover the ROI.
    #[error("the solver answered {rgb} colours and {coverage} coverage values, expected {want} of each")]
    SolvedLength {
        rgb: usize,
        coverage: usize,
        want: usize,
    },
}

impl PatchInputError {
    /// The already-localized sentence this refusal shows the user.
    ///
    /// The technical numbers stay in the log; the user only needs to know which stage refused.
    fn user_message(self) -> String {
        match self {
            Self::EmptyRoi { .. } => t!("cleaning.tools.patch.error_region_size").to_string(),
            Self::BufferSize { buffer, .. } => match buffer {
                PatchBuffer::Composite | PatchBuffer::Page => {
                    t!("cleaning.tools.patch.error_region_size").to_string()
                }
                PatchBuffer::OverlayChunk => {
                    t!("cleaning.tools.patch.error_overlay_size").to_string()
                }
            },
            Self::SolvedLength { .. } => t!("cleaning.tools.patch.error_solve").to_string(),
        }
    }
}

/// Why a finished chunk may not be written into the clean overlay.
///
/// The check exists because `CanvasView::replace_overlay_region_px` REPAIRS instead of refusing:
/// it clips a target rectangle that no longer fits the overlay and then nearest-rescales the
/// whole chunk into what is left, overwriting alpha wholesale. The same reason
/// `ai_editor::check_result_fits` and `region_edit_v2/frame.rs`'s D7 exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum ApplyError {
    /// The solved chunk does not have exactly the ROI's size.
    #[error("the solved chunk is {chunk_w}x{chunk_h}, the ROI is {roi_w}x{roi_h}")]
    ChunkSize {
        chunk_w: usize,
        chunk_h: usize,
        roi_w: usize,
        roi_h: usize,
    },
    /// The ROI does not lie inside the page's LIVE clean overlay.
    #[error("the ROI {x};{y} {w}x{h} does not fit the {overlay_w}x{overlay_h} clean overlay")]
    OutOfBounds {
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        overlay_w: usize,
        overlay_h: usize,
    },
}

/// The geometry one accepted source drag resolves to.
///
/// The two values travel together because they are decided together: the ROI is only valid
/// against the `source_size` it was bounds-checked with, and the loader crops against that same
/// size (`RegionLoadRequest`).
#[derive(Debug, Clone, Copy)]
struct DragGeometry {
    /// The padded ROI covering the selection's box and its translated copy, in page pixels.
    roi: OverlayRectPx,
    /// The page's source size the ROI was validated against.
    source_size: [usize; 2],
}

/// Why a source drag cannot be turned into a patch.
///
/// Produced by `PatchTool::drag_geometry`, which is the ONLY place the rule lives: the preview
/// outline asks it every frame of the drag and `start_job` asks it at the release, so what the
/// user sees while dragging and what happens when the button comes up cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
enum DragRefusal {
    /// The selection has fewer than three vertices, so it has no box to translate.
    #[error("the selection on page {page_idx} has fewer than three vertices")]
    DegenerateSelection { page_idx: usize },
    /// The page is not laid out this frame, so its source size is unknown.
    #[error("page {page_idx} is not laid out, so its source size is unknown")]
    PageNotLaidOut { page_idx: usize },
    /// The padded ROI would leave the page — the refusal `roi_for` exists for.
    #[error(
        "the padded ROI for bounds {bounds:?} at offset {offset:?} leaves the {source_w}x{source_h} page"
    )]
    SourceOutsidePage {
        bounds: (f32, f32, f32, f32),
        offset: (i32, i32),
        source_w: usize,
        source_h: usize,
    },
}

impl DragRefusal {
    /// The already-localized sentence this refusal shows the user.
    ///
    /// The coordinates stay in the log; the user only needs to know what to move.
    fn user_message(self) -> String {
        match self {
            Self::DegenerateSelection { .. } => {
                t!("cleaning.tools.patch.status_selection_too_small").to_string()
            }
            Self::PageNotLaidOut { page_idx } => {
                tf!("cleaning.tools.patch.error_page_missing", page = page_idx + 1)
            }
            Self::SourceOutsidePage { .. } => {
                t!("cleaning.tools.patch.error_source_outside_page").to_string()
            }
        }
    }
}

/// Whether `chunk_size` may be written into `roi` of an overlay of `overlay_size`.
///
/// `overlay_size` is `None` while the page has no clean overlay allocated yet; the write then
/// creates one at the page size and there is nothing to bounds-check against, so only the size
/// equality is enforced. Run against the overlay as it is AT COMMIT TIME, not as it was when the
/// job started: the solve is off-thread and the page may have been undone, cleared or re-laid out
/// in between.
///
/// # Errors
/// [`ApplyError::ChunkSize`] when the chunk is not exactly the ROI size, and
/// [`ApplyError::OutOfBounds`] when the ROI leaves the existing overlay.
fn check_chunk_fits(
    chunk_size: [usize; 2],
    roi: OverlayRectPx,
    overlay_size: Option<[usize; 2]>,
) -> Result<(), ApplyError> {
    if chunk_size != [roi.w, roi.h] {
        return Err(ApplyError::ChunkSize {
            chunk_w: chunk_size[0],
            chunk_h: chunk_size[1],
            roi_w: roi.w,
            roi_h: roi.h,
        });
    }
    if let Some([overlay_w, overlay_h]) = overlay_size
        && (roi.x.saturating_add(roi.w) > overlay_w || roi.y.saturating_add(roi.h) > overlay_h)
    {
        return Err(ApplyError::OutOfBounds {
            x: roi.x,
            y: roi.y,
            w: roi.w,
            h: roi.h,
            overlay_w,
            overlay_h,
        });
    }
    Ok(())
}

/// A job waiting for its two region loads.
///
/// Two loads, not one: the membrane needs the COMPOSITED destination (page + clean overlay),
/// while `overlay_pixel_for_final_color` needs the ORIGINAL page pixel under it. The shared
/// loader answers both from the same cached decoded page, so the second request costs a crop.
#[derive(Debug)]
struct PendingLoad {
    composite_job_id: u64,
    page_job_id: u64,
    page_idx: usize,
    roi: OverlayRectPx,
    offset: (i32, i32),
    /// The selection polygon translated into ROI-local coordinates.
    polygon_roi: Vec<(f32, f32)>,
    /// The clean-overlay pixels currently under the ROI; preserved where coverage is zero.
    overlay_chunk: egui::ColorImage,
    blend: PatchBlend,
    feather_px: usize,
    composite: Option<egui::ColorImage>,
    page: Option<egui::ColorImage>,
}

/// One finished solve, ready to be written into the clean overlay.
#[derive(Debug)]
struct PatchOutcome {
    page_idx: usize,
    roi: OverlayRectPx,
    /// The overlay chunk on success, an already-localized message on failure.
    chunk: Result<egui::ColorImage, String>,
}

/// Everything the worker needs to solve and build one patch, with no canvas access.
#[derive(Debug)]
struct PatchJobInput {
    page_idx: usize,
    roi: OverlayRectPx,
    offset: (i32, i32),
    polygon_roi: Vec<(f32, f32)>,
    composite: egui::ColorImage,
    page: egui::ColorImage,
    overlay_chunk: egui::ColorImage,
    blend: PatchBlend,
    feather_px: usize,
}

/// Photoshop-style patch tool: select, drag onto a source, get a colour-adapted copy.
pub struct PatchTool {
    shape: PatchShape,
    blend: PatchBlend,
    feather_px: usize,
    selection: Option<PatchSelection>,
    gesture: Option<PatchGesture>,
    /// A gesture whose `stroke_end` has arrived but whose fate is not decided yet.
    ///
    /// `stroke_end` cannot tell a real pointer release from the end+begin pair the tab issues
    /// when a drag crosses onto another page, because the hook is handed no pointer state. The
    /// finished gesture is therefore parked here and resolved where that state exists: the
    /// following `stroke_begin` (the crossing) or this frame's `draw_overlay_ui` (the release).
    pending_end: Option<PatchGesture>,
    space_pan_active: bool,
    panel_rects: Vec<egui::Rect>,
    status: Option<String>,
    /// A finished source drag waiting for the next `draw_overlay_ui`.
    ///
    /// `stroke_end` is not given the project, and the job needs the page's file path, so the
    /// offset is parked for exactly one frame instead of being started from the wrong hook.
    pending_offset: Option<(i32, i32)>,
    next_job_id: u64,
    pending_load: Option<PendingLoad>,
    solve_rx: Option<Receiver<PatchOutcome>>,
    load_tx: Sender<Option<RegionLoadRequest>>,
    load_rx: Receiver<RegionLoadResult>,
    loader_handle: Option<JoinHandle<()>>,
}

impl Default for PatchTool {
    fn default() -> Self {
        let (load_tx, load_rx, loader_handle) = spawn_region_loader_thread();
        Self {
            shape: PatchShape::default(),
            blend: PatchBlend::default(),
            feather_px: 0,
            selection: None,
            gesture: None,
            pending_end: None,
            space_pan_active: false,
            panel_rects: Vec::new(),
            status: None,
            pending_offset: None,
            next_job_id: 0,
            pending_load: None,
            solve_rx: None,
            load_tx,
            load_rx,
            loader_handle: Some(loader_handle),
        }
    }
}

impl Drop for PatchTool {
    /// Stops the shared region loader.
    ///
    /// Its ownership contract: send `None` and join, or the thread and the page it decoded
    /// outlive the tool.
    fn drop(&mut self) {
        // A failed send means the worker already exited, which is the state this asks for; a
        // failed join means it panicked, and there is no recovery available inside `drop`.
        let _ = self.load_tx.send(None);
        if let Some(handle) = self.loader_handle.take() {
            let _ = handle.join();
        }
    }
}

impl PatchTool {
    /// Whether a solve or a load is in flight, so a new gesture must be refused.
    fn busy(&self) -> bool {
        self.pending_load.is_some() || self.solve_rx.is_some() || self.pending_offset.is_some()
    }

    /// Reports a failure to the user and to the log.
    ///
    /// `text` is the localized, user-facing sentence; `detail` is the technical context that
    /// only belongs in the log.
    fn report_error(&mut self, text: String, detail: &dyn std::fmt::Display) {
        crate::runtime_log::log_warn(format!("[cleaning/patch] {text} | {detail}"));
        self.status = Some(text);
    }

    /// A stroke position as fractional PAGE pixels of its page.
    ///
    /// `None` when the page has no clean overlay or no scene rect yet, which is also when the
    /// selection could not be projected back for painting.
    fn page_pos(canvas: &CanvasView, point: StrokePoint) -> Option<(f32, f32)> {
        let pos = scene_pos_to_overlay_pos(canvas, point.page_idx, point.scene_pos)?;
        Some((pos.x, pos.y))
    }

    /// Clears the selection, any live gesture and everything in flight.
    ///
    /// `reason` is the English context for the log; the user is told nothing, because the
    /// outline disappearing and the spinner stopping already say it.
    ///
    /// The work in flight goes WITH the selection: a load or a solve that survived would land a
    /// patch for a selection the user has explicitly dismissed, over whatever was drawn in the
    /// meantime — the hazard `cancel_in_flight` documents, reached here by Escape and by the
    /// «clear selection» button instead of by a tool switch. Cancelling first, so the log names
    /// what was actually dropped; the tool is immediately usable for a fresh selection because
    /// `busy()` reads the very fields this clears.
    fn clear_selection(&mut self, reason: &dyn std::fmt::Display) {
        self.cancel_in_flight(reason);
        self.selection = None;
        self.gesture = None;
        self.pending_end = None;
    }

    /// Abandons a parked gesture end, telling the user and the log why.
    ///
    /// `text` is the localized sentence; `detail` is the technical context. A COMPLETED selection
    /// is deliberately left alone — only the gesture that was still in flight is dropped, so a
    /// cancelled source drag returns the user to the selection they already had.
    fn cancel_gesture(
        &mut self,
        gesture: &PatchGesture,
        text: String,
        detail: &dyn std::fmt::Display,
    ) {
        crate::runtime_log::log_info(format!(
            "[cleaning/patch] the {} gesture on page {} was cancelled: {detail}",
            gesture.kind(),
            gesture.page_idx()
        ));
        self.status = Some(text);
    }

    /// Turns the parked `stroke_end` into a commit or a cancellation.
    ///
    /// `released` must be the answer to "has the primary pointer button actually been released?",
    /// read from this frame's input. When it is `false` the `stroke_end` came from the tab's
    /// end+begin pair or from the pointer leaving the canvas, NOT from the user finishing the
    /// gesture, and committing then would close a lasso the user is still drawing or apply a
    /// patch while the button is still held.
    fn resolve_pending_end(&mut self, released: bool) {
        let Some(gesture) = self.pending_end.take() else {
            return;
        };
        if !released {
            self.cancel_gesture(
                &gesture,
                t!("cleaning.tools.patch.status_gesture_cancelled").to_string(),
                &"the stroke ended with the primary button still held",
            );
            return;
        }
        match gesture {
            PatchGesture::Lasso { page_idx, points } => self.commit_selection(page_idx, points),
            PatchGesture::Rect {
                page_idx,
                start,
                current,
            } => self.commit_selection(page_idx, rect_corners(start, current)),
            PatchGesture::Source { start, current, .. } => {
                let offset = drag_offset(start, current);
                if offset == (0, 0) {
                    // A click inside the selection is not a patch; it keeps the selection.
                    return;
                }
                // `start_job` needs the project, which no stroke hook is given; the drag is
                // parked once more and consumed at the top of the next `draw_overlay_ui`.
                self.pending_offset = Some(offset);
            }
        }
    }

    /// Abandons everything in flight and records what was dropped.
    ///
    /// `draw_overlay_ui` runs for the ACTIVE tool only (`../../tab.rs`), so a load or a solve left
    /// running past a tool switch is never polled again — and when it finally were, it would be
    /// applied against the overlay snapshot taken before the switch, over whatever the user did
    /// with another tool in between. `ai_editor::cancel_run` abandons its run for the same reason.
    ///
    /// The shared region loader THREAD is deliberately not stopped here: it belongs to the tool,
    /// not to one gesture, and `Drop` is what honours its ownership contract (send `None`, then
    /// join). Dropping `pending_load` is enough to abandon a job, because `poll_region_loads`
    /// discards every answer whose `job_id` no longer matches a pending one.
    fn cancel_in_flight(&mut self, reason: &dyn std::fmt::Display) {
        if self.pending_end.is_none()
            && self.pending_offset.is_none()
            && self.pending_load.is_none()
            && self.solve_rx.is_none()
        {
            return;
        }
        crate::runtime_log::log_info(format!(
            "[cleaning/patch] abandoning the work in flight ({reason}): parked end {}, parked drag {}, region load {}, solve {}",
            self.pending_end.is_some(),
            self.pending_offset.is_some(),
            self.pending_load.is_some(),
            self.solve_rx.is_some()
        ));
        self.pending_end = None;
        self.pending_offset = None;
        self.pending_load = None;
        self.solve_rx = None;
        self.status = None;
    }

    /// Accepts a freshly drawn polygon as the selection, or rejects it as degenerate.
    ///
    /// A degenerate selection (fewer than three vertices, or a bounding box thinner than
    /// `MIN_SELECTION_SPAN_PX` on either axis) leaves NO selection behind: a half-built outline
    /// the user cannot drag would be worse than none.
    fn commit_selection(&mut self, page_idx: usize, points: Vec<(f32, f32)>) {
        let candidate = PatchSelection { page_idx, points };
        let usable = candidate.bounds().is_some_and(|(min_x, min_y, max_x, max_y)| {
            max_x - min_x >= MIN_SELECTION_SPAN_PX && max_y - min_y >= MIN_SELECTION_SPAN_PX
        });
        if usable {
            self.selection = Some(candidate);
            self.status = Some(t!("cleaning.tools.patch.status_selection_ready").to_string());
        } else {
            self.selection = None;
            self.status = Some(t!("cleaning.tools.patch.status_selection_too_small").to_string());
        }
    }

    /// The ROI a patch needs: the selection's box and its translated copy, padded.
    ///
    /// Returns `None` when the padded union does not fit inside a `source_w` x `source_h` page —
    /// which is the refusal case, not something to clamp: a clamped ROI would silently sample
    /// pixels the user did not point at.
    fn roi_for(
        bounds: (f32, f32, f32, f32),
        offset: (i32, i32),
        source_w: usize,
        source_h: usize,
    ) -> Option<OverlayRectPx> {
        let (min_x, min_y, max_x, max_y) = bounds;
        // Page dimensions are far inside `f32`'s exact-integer range, so the comparisons below
        // are exact; `f32 as i64` saturates, so a wild polygon cannot wrap.
        let dst_x0 = min_x.floor() as i64;
        let dst_y0 = min_y.floor() as i64;
        let dst_x1 = max_x.ceil() as i64;
        let dst_y1 = max_y.ceil() as i64;
        let off_x = i64::from(offset.0);
        let off_y = i64::from(offset.1);
        let pad = ROI_PAD_PX as i64;
        let x0 = dst_x0.min(dst_x0 + off_x) - pad;
        let y0 = dst_y0.min(dst_y0 + off_y) - pad;
        let x1 = dst_x1.max(dst_x1 + off_x) + pad;
        let y1 = dst_y1.max(dst_y1 + off_y) + pad;
        if x0 < 0 || y0 < 0 || x1 > source_w as i64 || y1 > source_h as i64 {
            return None;
        }
        let w = usize::try_from(x1 - x0).ok()?;
        let h = usize::try_from(y1 - y0).ok()?;
        if w == 0 || h == 0 {
            return None;
        }
        Some(OverlayRectPx {
            x: usize::try_from(x0).ok()?,
            y: usize::try_from(y0).ok()?,
            w,
            h,
        })
    }

    /// The geometry a source drag at `offset` resolves to, or why it is refused.
    ///
    /// THE single evaluation of the refusal rule. `start_job` needs the rectangle and the page
    /// size, the preview outline needs only whether there is one, and a second copy of the rule
    /// would let the outline promise a patch the release then refuses — the reason this helper
    /// exists rather than two calls to `roi_for` with separately fetched page sizes.
    ///
    /// # Errors
    /// [`DragRefusal::DegenerateSelection`] for a selection that is not a polygon,
    /// [`DragRefusal::PageNotLaidOut`] while the page has no placement this frame, and
    /// [`DragRefusal::SourceOutsidePage`] when the padded ROI leaves the page.
    fn drag_geometry(
        canvas: &CanvasView,
        selection: &PatchSelection,
        offset: (i32, i32),
    ) -> Result<DragGeometry, DragRefusal> {
        let page_idx = selection.page_idx;
        let bounds = selection
            .bounds()
            .ok_or(DragRefusal::DegenerateSelection { page_idx })?;
        let source_size = page_source_size(canvas, page_idx, canvas.zoom())
            .ok_or(DragRefusal::PageNotLaidOut { page_idx })?;
        let roi = Self::roi_for(bounds, offset, source_size[0], source_size[1]).ok_or(
            DragRefusal::SourceOutsidePage {
                bounds,
                offset,
                source_w: source_size[0],
                source_h: source_size[1],
            },
        )?;
        Ok(DragGeometry { roi, source_size })
    }

    /// Turns a finished source drag into two region-load requests.
    ///
    /// Nothing is decoded here: the GUI thread only measures the geometry and copies the
    /// clean-overlay chunk it already holds in memory.
    fn start_job(
        &mut self,
        canvas: &CanvasView,
        project: &ProjectData,
        offset: (i32, i32),
    ) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        let page_idx = selection.page_idx;
        let Some(page) = project.pages.iter().find(|page| page.idx == page_idx) else {
            self.report_error(
                tf!("cleaning.tools.patch.error_page_missing", page = page_idx + 1),
                &format!("page {page_idx} is not in the project"),
            );
            return;
        };
        let page_path: PathBuf = page.path.clone();
        // The SAME predicate the preview outline is painted with (`paint`), evaluated in one
        // place: an outline that promised a patch the release then refuses is the bug this
        // shared helper exists to make impossible.
        let DragGeometry { roi, source_size } = match Self::drag_geometry(canvas, &selection, offset)
        {
            Ok(geometry) => geometry,
            Err(refusal) => {
                self.report_error(refusal.user_message(), &refusal);
                return;
            }
        };
        let Some(overlay) = canvas.overlay_image(page_idx) else {
            self.report_error(
                tf!("cleaning.tools.patch.error_no_overlay", page = page_idx + 1),
                &format!("page {page_idx} has no clean overlay allocated"),
            );
            return;
        };
        let overlay_chunk = extract_overlay_chunk(overlay, roi);

        // Page pixels -> ROI-local pixels. The polygon is inside its own bounding box and the
        // ROI pads that box by `ROI_PAD_PX`, so the rasterized mask cannot reach the ROI border.
        let polygon_roi: Vec<(f32, f32)> = selection
            .points
            .iter()
            .map(|(x, y)| (x - roi.x as f32, y - roi.y as f32))
            .collect();

        let composite_job_id = self.next_job_id;
        let page_job_id = self.next_job_id.saturating_add(1);
        self.next_job_id = self.next_job_id.saturating_add(2);

        let composite_request = RegionLoadRequest {
            job_id: composite_job_id,
            page_idx,
            source_rect: roi,
            source_size,
            page_path: page_path.clone(),
            overlay_chunk: Some(overlay_chunk.clone()),
            shared_overlays_model: canvas.clean_overlays_model_handle(),
        };
        // `overlay_chunk: None` asks the same loader for the page WITHOUT the clean overlay —
        // the `base` argument `overlay_pixel_for_final_color` is defined against.
        let page_request = RegionLoadRequest {
            job_id: page_job_id,
            page_idx,
            source_rect: roi,
            source_size,
            page_path,
            overlay_chunk: None,
            shared_overlays_model: canvas.clean_overlays_model_handle(),
        };
        if self.load_tx.send(Some(composite_request)).is_err()
            || self.load_tx.send(Some(page_request)).is_err()
        {
            self.report_error(
                t!("cleaning.tools.patch.error_region_load").to_string(),
                &"the region loader worker is gone",
            );
            return;
        }

        self.pending_load = Some(PendingLoad {
            composite_job_id,
            page_job_id,
            page_idx,
            roi,
            offset,
            polygon_roi,
            overlay_chunk,
            blend: self.blend,
            feather_px: self.feather_px,
            composite: None,
            page: None,
        });
        self.status = Some(t!("cleaning.tools.patch.status_running").to_string());
    }

    /// Drains the region loader and spawns the solve once both halves have arrived.
    fn poll_region_loads(&mut self) {
        let mut failure: Option<String> = None;
        let mut disconnected = false;
        loop {
            match self.load_rx.try_recv() {
                Ok(result) => {
                    let Some(pending) = self.pending_load.as_mut() else {
                        continue;
                    };
                    let slot = if result.job_id == pending.composite_job_id {
                        &mut pending.composite
                    } else if result.job_id == pending.page_job_id {
                        &mut pending.page
                    } else {
                        // An answer to an abandoned job; dropping it is what makes a cancel free.
                        continue;
                    };
                    match result.image {
                        Ok(image) => *slot = Some(image),
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if let Some(error) = failure {
            self.pending_load = None;
            self.report_error(t!("cleaning.tools.patch.error_region_load").to_string(), &error);
            return;
        }
        if disconnected {
            if self.pending_load.take().is_some() {
                self.report_error(
                    t!("cleaning.tools.patch.error_region_load").to_string(),
                    &"the region loader worker died with a job in flight",
                );
            }
            return;
        }

        let ready = self
            .pending_load
            .as_ref()
            .is_some_and(|pending| pending.composite.is_some() && pending.page.is_some());
        if !ready {
            return;
        }
        let Some(mut pending) = self.pending_load.take() else {
            return;
        };
        let (Some(composite), Some(page)) = (pending.composite.take(), pending.page.take()) else {
            return;
        };
        let input = PatchJobInput {
            page_idx: pending.page_idx,
            roi: pending.roi,
            offset: pending.offset,
            polygon_roi: pending.polygon_roi,
            composite,
            page,
            overlay_chunk: pending.overlay_chunk,
            blend: pending.blend,
            feather_px: pending.feather_px,
        };
        let (tx, rx) = mpsc::channel::<PatchOutcome>();
        thread::spawn(move || {
            let outcome = run_patch_job(input);
            // A failed send means the tool was dropped or the job abandoned while this worker
            // ran; there is nobody left to report to and nothing to clean up.
            let _ = tx.send(outcome);
        });
        self.solve_rx = Some(rx);
    }

    /// Applies a finished solve to the clean overlay as ONE undo step.
    fn poll_solve(&mut self, canvas: &mut CanvasView) {
        let Some(rx) = self.solve_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                match outcome.chunk {
                    Ok(chunk) => {
                        // Bounds are re-checked against the overlay AS IT IS NOW, not as it was
                        // when the job started: the solve ran off-thread, and
                        // `replace_overlay_region_px` clips a rectangle that no longer fits and
                        // nearest-rescales the chunk into the remainder instead of refusing.
                        if let Err(error) = check_chunk_fits(
                            chunk.size,
                            outcome.roi,
                            canvas.overlay_size(outcome.page_idx),
                        ) {
                            self.report_error(
                                t!("cleaning.tools.patch.error_overlay_changed").to_string(),
                                &error,
                            );
                            return;
                        }
                        // `replace_overlay_region_px` syncs the shared model and records ONE
                        // region diff there (`CleanOverlaysModel::replace_region`), so the whole
                        // patch is a single Ctrl+Z and nothing further has to be published.
                        if canvas.replace_overlay_region_px(outcome.page_idx, outcome.roi, &chunk) {
                            self.status =
                                Some(t!("cleaning.tools.patch.status_applied").to_string());
                        } else {
                            self.report_error(
                                t!("cleaning.tools.patch.error_apply").to_string(),
                                &format!(
                                    "the canvas refused the overlay write for page {}",
                                    outcome.page_idx
                                ),
                            );
                        }
                    }
                    Err(error) => {
                        self.report_error(t!("cleaning.tools.patch.error_solve").to_string(), &error);
                    }
                }
            }
            Err(TryRecvError::Empty) => self.solve_rx = Some(rx),
            Err(TryRecvError::Disconnected) => {
                self.report_error(
                    t!("cleaning.tools.patch.error_solve").to_string(),
                    &"the patch worker died before answering",
                );
            }
        }
    }

    /// The polygon a live gesture would produce, in page pixels.
    ///
    /// `None` while nothing is being drawn; a rectangle drag answers with its four corners so
    /// the preview outline and the committed selection are built from one rule.
    fn gesture_polygon(&self) -> Option<(usize, Vec<(f32, f32)>)> {
        match self.gesture.as_ref()? {
            PatchGesture::Lasso { page_idx, points } => Some((*page_idx, points.clone())),
            PatchGesture::Rect {
                page_idx,
                start,
                current,
            } => Some((*page_idx, rect_corners(*start, *current))),
            PatchGesture::Source { .. } => None,
        }
    }

    /// Paints the selection outlines for this frame.
    ///
    /// Painted from `draw_overlay_ui` through a bare layer painter, NOT from `draw_cursor`:
    /// `draw_cursor` is pointer-gated and returns early whenever the pointer leaves the canvas
    /// or is occluded, which would make a session-long selection blink out. A bare layer painter
    /// registers no interactable `Area`, so it steals no input from the canvas.
    fn paint(&self, ctx: &egui::Context, canvas: &CanvasView) {
        let Some(viewport) = canvas.visible_scene_rect() else {
            return;
        };
        // The dock panels are cut out of the viewport, so the outline never paints over one.
        let usable = usable_viewport_for(viewport, viewport, &self.panel_rects);
        if !usable.is_positive() {
            return;
        }
        let painter = ctx
            .layer_painter(egui::LayerId::new(
                egui::Order::Middle,
                egui::Id::new("cleaning_patch_outline"),
            ))
            .with_clip_rect(usable);

        if let Some((page_idx, points)) = self.gesture_polygon() {
            paint_outline(&painter, canvas, page_idx, &points, (0, 0), OutlineTone::Normal);
            return;
        }
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        paint_outline(
            &painter,
            canvas,
            selection.page_idx,
            &selection.points,
            (0, 0),
            OutlineTone::Normal,
        );
        if let Some(PatchGesture::Source {
            page_idx,
            start,
            current,
        }) = self.gesture.as_ref()
            && *page_idx == selection.page_idx
        {
            let offset = drag_offset(*start, *current);
            if offset != (0, 0) {
                // The translated outline shows exactly which pixels are about to be copied — and
                // is marked REFUSED while the release would be, by asking `drag_geometry`, the same
                // predicate `start_job` decides with. The user therefore sees the refusal during
                // the drag instead of learning about it after letting go.
                let tone = OutlineTone::for_drag(&Self::drag_geometry(canvas, selection, offset));
                paint_outline(
                    &painter,
                    canvas,
                    selection.page_idx,
                    &selection.points,
                    offset,
                    tone,
                );
            }
        }
    }
}

/// Which gesture a primary press starts.
///
/// A press INSIDE the current selection (same page, even-odd inside test) begins the source
/// drag; anything else begins a new selection of the configured shape. Split out of
/// `stroke_begin` because it is the whole state machine and the only part of it that can be
/// exercised without a laid-out canvas.
fn gesture_for_press(
    selection: Option<&PatchSelection>,
    shape: PatchShape,
    page_idx: usize,
    pos: (f32, f32),
) -> PatchGesture {
    let inside = selection
        .is_some_and(|selection| selection.page_idx == page_idx && selection.contains(pos.0, pos.1));
    if inside {
        return PatchGesture::Source {
            page_idx,
            start: pos,
            current: pos,
        };
    }
    match shape {
        PatchShape::Lasso => PatchGesture::Lasso {
            page_idx,
            points: vec![pos],
        },
        PatchShape::Rect => PatchGesture::Rect {
            page_idx,
            start: pos,
            current: pos,
        },
    }
}

/// The four corners of the rectangle spanned by two page-pixel positions.
fn rect_corners(start: (f32, f32), current: (f32, f32)) -> Vec<(f32, f32)> {
    let x0 = start.0.min(current.0);
    let x1 = start.0.max(current.0);
    let y0 = start.1.min(current.1);
    let y1 = start.1.max(current.1);
    vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
}

/// The drag offset in whole page pixels.
///
/// Whole pixels because the source is SAMPLED at `destination + offset`: a fractional offset
/// would need resampling, which would blur the copied texture the membrane exists to preserve.
fn drag_offset(start: (f32, f32), current: (f32, f32)) -> (i32, i32) {
    // `f32 as i32` saturates in Rust, so a wild position cannot wrap into a valid-looking offset.
    (
        (current.0 - start.0).round() as i32,
        (current.1 - start.1).round() as i32,
    )
}

/// How an outline is stroked: an ordinary one, or one whose release would be refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutlineTone {
    /// The selection itself, a selection being drawn, or a source drag that will be accepted.
    Normal,
    /// A source drag `PatchTool::drag_geometry` refuses — no patch can be applied from here.
    Refused,
}

impl OutlineTone {
    /// The light half of the two-tone dash. The dark half is `OUTLINE_DARK` either way, so the
    /// outline stays readable over both black line art and white paper in both states.
    fn light(self) -> Color32 {
        match self {
            Self::Normal => Color32::WHITE,
            Self::Refused => OUTLINE_REFUSED,
        }
    }

    /// The tone a source drag's outline is painted in, read off the SAME answer the release
    /// acts on (`PatchTool::drag_geometry`, consumed by `start_job`).
    ///
    /// Total and reason-agnostic on purpose: a refusal reason that painted as `Normal` would be
    /// a preview promising a patch the release then refuses, which is the whole defect this
    /// mapping exists to make impossible.
    fn for_drag(geometry: &Result<DragGeometry, DragRefusal>) -> Self {
        match geometry {
            Ok(_) => Self::Normal,
            Err(_) => Self::Refused,
        }
    }
}

/// A page-pixel position of `page_idx` as a scene (== screen) position, WITHOUT clamping.
///
/// `base::overlay_pos_to_scene_pos` is the same affine map but CLAMPS its argument to the page,
/// so a point outside the page comes back pinned to the border — which for an aiming outline
/// would claim the source sits somewhere it does not. This is that map minus the clamp, i.e. the
/// point form of `base::overlay_rect_to_scene_rect`, which is already unclamped and uses exactly
/// this expression. Positions outside the page therefore map outside `page_scene_rect`, as they
/// should; the painter's clip rect is what keeps them off the dock panels.
///
/// `None` when the page has no scene rect, no overlay, a zero-sized overlay or a degenerate
/// page rect — the same refusals the clamped helper makes.
fn page_pos_to_scene_pos_unclamped(
    canvas: &CanvasView,
    page_idx: usize,
    pos: [f32; 2],
) -> Option<Pos2> {
    let page_rect = canvas.page_scene_rect(page_idx)?;
    let overlay_size = canvas.overlay_size(page_idx)?;
    scene_pos_in_page(page_rect, overlay_size, pos)
}

/// The unclamped page-pixel -> scene map itself, without the canvas lookups.
///
/// Split from `page_pos_to_scene_pos_unclamped` because this is the part that must be pinned by
/// a test: a laid-out page cannot be built in a unit test, but the mapping can, and "a point
/// outside the page maps outside the page rect" is exactly the property the aiming outline
/// depends on. `None` for a zero-sized overlay or a degenerate page rect.
fn scene_pos_in_page(
    page_scene_rect: egui::Rect,
    overlay_size: [usize; 2],
    pos: [f32; 2],
) -> Option<Pos2> {
    let [overlay_w, overlay_h] = overlay_size;
    if overlay_w == 0 || overlay_h == 0 || !page_scene_rect.is_positive() {
        return None;
    }
    Some(egui::pos2(
        page_scene_rect.left() + page_scene_rect.width() * (pos[0] / overlay_w as f32),
        page_scene_rect.top() + page_scene_rect.height() * (pos[1] / overlay_h as f32),
    ))
}

/// Paints one closed polygon as a two-tone dashed outline, translated by `offset` page pixels.
///
/// The dashes are STATIC: they have no phase, and the tool asks for a repaint only while a job is
/// running (`draw_overlay_ui`), so nothing here marches. Animating them would mean a repaint every
/// frame for the whole session, which a selection that lives on the canvas that long must not cost.
///
/// The projection is UNCLAMPED, so a translated outline that leaves the page is drawn where the
/// source actually is instead of pinned to the page border; `tone` is what says the drag is
/// unusable. Nothing is painted when the page is not laid out or has no overlay, which is also
/// when the page-pixel space the polygon lives in cannot be projected onto the screen.
fn paint_outline(
    painter: &egui::Painter,
    canvas: &CanvasView,
    page_idx: usize,
    points: &[(f32, f32)],
    offset: (i32, i32),
    tone: OutlineTone,
) {
    if points.len() < 2 {
        return;
    }
    let mut path: Vec<Pos2> = Vec::with_capacity(points.len() + 1);
    for (x, y) in points {
        let moved = [*x + offset.0 as f32, *y + offset.1 as f32];
        let Some(pos) = page_pos_to_scene_pos_unclamped(canvas, page_idx, moved) else {
            return;
        };
        path.push(pos);
    }
    // `dashed_line` connects consecutive points only, so the first point is repeated to close
    // the loop.
    if let Some(first) = path.first().copied() {
        path.push(first);
    }
    let perimeter: f32 = path
        .windows(2)
        .map(|pair| pair[0].distance(pair[1]))
        .sum();
    if perimeter <= f32::EPSILON {
        return;
    }
    // Dash and gap scale with the on-screen perimeter, so the dash COUNT is zoom-invariant.
    let dash = (perimeter / (OUTLINE_DASH_COUNT * 2.0)).max(OUTLINE_MIN_DASH);
    painter.extend(egui::Shape::dashed_line(
        &path,
        egui::Stroke::new(3.0, OUTLINE_DARK),
        dash,
        dash,
    ));
    painter.extend(egui::Shape::dashed_line(
        &path,
        egui::Stroke::new(1.0, tone.light()),
        dash,
        dash,
    ));
}

/// Linearly blends two opaque RGB triples.
fn lerp_rgb(from: [u8; 3], to: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let mut out = [0u8; 3];
    for channel in 0..3 {
        let value = f32::from(from[channel]) + (f32::from(to[channel]) - f32::from(from[channel])) * t;
        // Clamped first, and `f32 as u8` saturates, so the conversion cannot wrap.
        out[channel] = value.round().clamp(0.0, 255.0) as u8;
    }
    out
}

/// The opaque RGB triple of an overlay/page pixel.
fn rgb_of(color: Color32) -> [u8; 3] {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    [r, g, b]
}

/// The whole worker-side pass: rasterize, solve, build the clean-overlay chunk.
///
/// Runs on a worker thread and touches no canvas. Errors come back as already-localized
/// sentences, because the GUI thread has nothing left to add to them.
fn run_patch_job(input: PatchJobInput) -> PatchOutcome {
    let roi = input.roi;
    let count = roi.w.saturating_mul(roi.h);
    // EVERY ROI-sized buffer is checked here, once, before anything is computed or written. The
    // per-pixel loop below therefore never has to decide what a length disagreement means — a
    // decision it could only make by skipping pixels or substituting values, i.e. by guessing
    // over the user's cleaning work.
    if let Err(error) = validate_job_input(&input) {
        return refuse(input.page_idx, roi, &error);
    }

    let mut mask = vec![false; count];
    fill_polygon_spans(&input.polygon_roi, roi.w, roi.h, |y, x0, x1| {
        let row = y.saturating_mul(roi.w);
        for x in x0..=x1 {
            if let Some(cell) = mask.get_mut(row + x) {
                *cell = true;
            }
        }
    });

    let dst: Vec<[u8; 3]> = input.composite.pixels.iter().copied().map(rgb_of).collect();
    let src = translated_source(&dst, roi.w, roi.h, input.offset);
    let request = PatchRequest {
        roi_w: roi.w,
        roi_h: roi.h,
        dst,
        src,
        mask,
        feather_px: input.feather_px,
        blend: input.blend,
    };
    let solved = match solve_patch(&request) {
        Ok(solved) => solved,
        Err(error) => {
            crate::runtime_log::log_error(format!("[cleaning/patch] the membrane solve refused the job: {error}"));
            return PatchOutcome {
                page_idx: input.page_idx,
                roi,
                chunk: Err(t!("cleaning.tools.patch.error_solve").to_string()),
            };
        }
    };

    if solved.rgb.len() != count || solved.coverage.len() != count {
        return refuse(
            input.page_idx,
            roi,
            &PatchInputError::SolvedLength {
                rgb: solved.rgb.len(),
                coverage: solved.coverage.len(),
                want: count,
            },
        );
    }

    // Start from the overlay that is already there: a pixel the patch does not cover must keep
    // the surrounding cleaning work instead of being erased by the commit.
    let mut chunk = input.overlay_chunk;
    // Every one of the five buffers is exactly `count` long by the checks above, so the zip
    // visits each ROI pixel exactly once and no pixel can be skipped over a length disagreement.
    let per_pixel = chunk
        .pixels
        .iter_mut()
        .zip(input.page.pixels.iter())
        .zip(solved.rgb.iter())
        .zip(solved.coverage.iter())
        .zip(request.dst.iter());
    for ((((target, base), solved_rgb), coverage), current) in per_pixel {
        if *coverage <= 0.0 {
            continue;
        }
        let final_rgb = lerp_rgb(*current, *solved_rgb, *coverage);
        *target = overlay_pixel_for_final_color(
            *base,
            Color32::from_rgb(final_rgb[0], final_rgb[1], final_rgb[2]),
            *coverage,
        );
    }

    PatchOutcome {
        page_idx: input.page_idx,
        roi,
        chunk: Ok(chunk),
    }
}

/// Checks the three ROI-sized inputs of one job against the ROI, once, before any work.
///
/// # Errors
/// [`PatchInputError::EmptyRoi`] for an ROI with no pixels, and
/// [`PatchInputError::BufferSize`] naming the first buffer whose size or pixel count disagrees.
fn validate_job_input(input: &PatchJobInput) -> Result<(), PatchInputError> {
    let roi = input.roi;
    let count = roi.w.saturating_mul(roi.h);
    if count == 0 {
        return Err(PatchInputError::EmptyRoi { w: roi.w, h: roi.h });
    }
    let buffers = [
        (PatchBuffer::Composite, &input.composite),
        (PatchBuffer::Page, &input.page),
        (PatchBuffer::OverlayChunk, &input.overlay_chunk),
    ];
    for (buffer, image) in buffers {
        if image.size != [roi.w, roi.h] || image.pixels.len() != count {
            return Err(PatchInputError::BufferSize {
                buffer,
                got_w: image.size[0],
                got_h: image.size[1],
                got_len: image.pixels.len(),
                want_w: roi.w,
                want_h: roi.h,
                want_len: count,
            });
        }
    }
    Ok(())
}

/// Turns a refused job into the outcome the GUI thread reports, with the numbers in the log.
fn refuse(page_idx: usize, roi: OverlayRectPx, error: &PatchInputError) -> PatchOutcome {
    crate::runtime_log::log_error(format!(
        "[cleaning/patch] the job for page {page_idx} was refused, ROI {roi:?}: {error}"
    ));
    PatchOutcome {
        page_idx,
        roi,
        chunk: Err(error.user_message()),
    }
}

/// Samples the ROI composite translated by `offset`, so `src[i]` is what lands on `dst[i]`.
///
/// Coordinates outside the ROI are CLAMPED. They can only occur far from the selection: the ROI
/// contains the selection's box and its translated copy with `ROI_PAD_PX` around both, so every
/// pixel of the selection and of the boundary ring the membrane reads has a real source. A
/// clamped cell is outside the selection, is pinned to its own boundary difference and is
/// separated from the selection by other pinned cells, so its value cannot reach the solution —
/// and its output colour is the destination's regardless.
fn translated_source(dst: &[[u8; 3]], w: usize, h: usize, offset: (i32, i32)) -> Vec<[u8; 3]> {
    let mut out = vec![[0u8; 3]; dst.len()];
    let max_x = w.saturating_sub(1) as i64;
    let max_y = h.saturating_sub(1) as i64;
    for y in 0..h {
        for x in 0..w {
            let sx = (x as i64 + i64::from(offset.0)).clamp(0, max_x);
            let sy = (y as i64 + i64::from(offset.1)).clamp(0, max_y);
            let src_idx = (sy as usize).saturating_mul(w).saturating_add(sx as usize);
            if let (Some(cell), Some(value)) = (out.get_mut(y * w + x), dst.get(src_idx)) {
                *cell = *value;
            }
        }
    }
    out
}

impl CleaningTool for PatchTool {
    fn tool_id(&self) -> &'static str {
        "patch"
    }

    fn title(&self) -> &'static str {
        t!("cleaning.tools.patch.title")
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        // The selection survives a tool switch (Photoshop keeps it). Nothing else does: the live
        // gesture has no meaning without the pointer that started it, and everything in flight
        // would otherwise finish unpolled and land on an overlay the user has meanwhile changed
        // — see `cancel_in_flight`.
        self.gesture = None;
        self.space_pan_active = false;
        self.cancel_in_flight(&"the tool was deactivated");
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(t!("cleaning.tools.patch.shape_label"));
        ui.horizontal(|ui| {
            for shape in [PatchShape::Lasso, PatchShape::Rect] {
                if ui
                    .add(egui::Button::new(shape.title()).selected(self.shape == shape))
                    .clicked()
                {
                    self.shape = shape;
                    self.gesture = None;
                }
            }
        });

        ui.label(t!("cleaning.tools.patch.blend_label"));
        ui.horizontal_wrapped(|ui| {
            for blend in [
                PatchBlend::None,
                PatchBlend::Linear,
                PatchBlend::Multiplicative,
            ] {
                if ui
                    .add(egui::Button::new(blend_title(blend)).selected(self.blend == blend))
                    .clicked()
                {
                    self.blend = blend;
                }
            }
        });

        let mut feather = self.feather_px;
        if ui
            .add(
                WheelSlider::new(&mut feather, 0..=MAX_FEATHER_PX)
                    .text(t!("cleaning.tools.patch.feather_label")),
            )
            .changed()
        {
            self.feather_px = feather.min(MAX_FEATHER_PX);
        }

        // Enabled while a job runs, and deliberately so: the button is then the way to call a
        // running patch off, and `clear_selection` cancels it.
        let has_selection = self.selection.is_some();
        if ui
            .add_enabled(
                has_selection,
                egui::Button::new(t!("cleaning.tools.patch.clear_selection_button")),
            )
            .clicked()
        {
            self.clear_selection(&"the clear-selection button was pressed");
            self.status = None;
        }

        if self.busy() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.small(t!("cleaning.tools.patch.status_running"));
            });
        } else if let Some(status) = self.status.as_ref() {
            ui.small(status.clone());
        }
        ui.small(t!("cleaning.tools.patch.usage_hint"));
    }

    fn stroke_begin(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        // The tab ends and immediately re-begins the stroke when the pointer moves onto a
        // different page mid-drag (`../../tab.rs::handle_active_tool_input`), and it only ever
        // does that with the primary button DOWN. So a parked gesture end found here on another
        // page IS that crossing — never a release — and the gesture is dropped instead of being
        // committed: a selection and the source it is dragged onto must live on one page, and a
        // lasso truncated at the seam is not the shape the user is drawing. A parked end on the
        // SAME page cannot come from a crossing, so it is a genuine release followed by a fresh
        // press inside one frame and is resolved as such.
        if let Some(ended) = self.pending_end.take() {
            if ended.page_idx() == point.page_idx {
                self.pending_end = Some(ended);
                self.resolve_pending_end(true);
            } else {
                self.cancel_gesture(
                    &ended,
                    t!("cleaning.tools.patch.status_page_changed").to_string(),
                    &format!(
                        "the pointer moved from page {} onto page {} while the button was held",
                        ended.page_idx(),
                        point.page_idx
                    ),
                );
                return;
            }
        }
        if self.space_pan_active {
            return;
        }
        if self.busy() {
            self.status = Some(t!("cleaning.tools.patch.status_busy").to_string());
            return;
        }
        // The page-pixel space the selection lives in is the clean overlay's, so it has to exist
        // before the first vertex is taken. A refusal only means the page is not laid out yet,
        // which the `page_pos` guard below reports as "no gesture".
        let _ = BrushToolBase::ensure_overlay_under_point(canvas, point);
        let Some(pos) = Self::page_pos(canvas, point) else {
            return;
        };
        let gesture = gesture_for_press(self.selection.as_ref(), self.shape, point.page_idx, pos);
        if !matches!(gesture, PatchGesture::Source { .. }) {
            // A press outside the selection always starts a NEW one, which is also how a
            // selection on another page is replaced.
            self.selection = None;
        }
        self.gesture = Some(gesture);
    }

    fn stroke_update(&mut self, canvas: &mut CanvasView, _from: StrokePoint, to: StrokePoint) {
        let Some(pos) = Self::page_pos(canvas, to) else {
            return;
        };
        match self.gesture.as_mut() {
            Some(PatchGesture::Lasso { page_idx, points }) => {
                if *page_idx != to.page_idx {
                    return;
                }
                // Skip a vertex that is barely away from the previous one; a dense run of
                // near-identical vertices carries no shape and costs memory.
                let far_enough = points.last().is_none_or(|(x, y)| {
                    let dx = pos.0 - x;
                    let dy = pos.1 - y;
                    dx * dx + dy * dy >= VERTEX_MIN_STEP_PX * VERTEX_MIN_STEP_PX
                });
                if far_enough {
                    points.push(pos);
                }
            }
            Some(
                PatchGesture::Rect {
                    page_idx, current, ..
                }
                | PatchGesture::Source {
                    page_idx, current, ..
                },
            ) if *page_idx == to.page_idx => *current = pos,
            Some(_) => {}
            None => {}
        }
    }

    /// Parks the finished gesture; NOTHING is committed here.
    ///
    /// The tab calls this hook for a real pointer release AND as one half of the end+begin pair it
    /// issues when a drag crosses onto another page, and for a pointer that left the canvas
    /// rectangle with the button still held — but it hands the hook no pointer state to tell them
    /// apart. For every other cleaning tool the difference is harmless ("end this page's stroke");
    /// for this one it would mean "commit", which is why the decision waits for
    /// `resolve_pending_end` and the frame's real pointer state.
    fn stroke_end(&mut self, _canvas: &mut CanvasView) {
        let Some(gesture) = self.gesture.take() else {
            return;
        };
        self.pending_end = Some(gesture);
    }

    fn set_space_pan_active(&mut self, active: bool) {
        self.space_pan_active = active;
    }

    fn space_pan_active(&self) -> bool {
        self.space_pan_active
    }

    fn block_canvas_drag_scroll_on_primary(&self) -> bool {
        !self.space_pan_active
    }

    /// Escape clears the selection and cancels the gesture and everything in flight.
    ///
    /// It answers `false` when there is nothing to clear, so the key stays available to whatever
    /// else wants it.
    fn on_key_event(&mut self, ctx: &egui::Context) -> bool {
        // A parked gesture end counts as something to clear: it would otherwise be committed by
        // this frame's `draw_overlay_ui`, right after the user asked for it to go away. So does
        // work in flight, which Escape must be able to call off even in the state where the
        // selection is already gone.
        if self.selection.is_none()
            && self.gesture.is_none()
            && self.pending_end.is_none()
            && !self.busy()
        {
            return false;
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.clear_selection(&"Escape was pressed");
            self.status = None;
            return true;
        }
        false
    }

    fn set_panel_rects(&mut self, rects: &[egui::Rect]) {
        self.panel_rects.clear();
        self.panel_rects.extend_from_slice(rects);
    }

    fn draw_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        // The only place in the frame where the pointer state is available (`../../tab.rs` runs
        // this after all stroke dispatch), and therefore the only place a parked `stroke_end` that
        // no `stroke_begin` claimed can be judged. `primary_released` is checked as well as
        // `primary_down`, so a release followed by a fresh press inside one frame still counts as
        // the release it was.
        let released = ctx.input(|input| {
            !input.pointer.primary_down() || input.pointer.primary_released()
        });
        self.resolve_pending_end(released);
        if let Some(offset) = self.pending_offset.take() {
            self.gesture = None;
            self.start_job(canvas, project, offset);
        }
        self.poll_region_loads();
        self.poll_solve(canvas);
        self.paint(ctx, canvas);
        if self.busy() {
            // The GUI stays responsive and keeps repainting while the worker runs.
            ctx.request_repaint();
        }
    }

    /// Never `true`.
    ///
    /// `block_canvas_zoom` also disables the clean-overlay Ctrl+Z / Ctrl+Shift+Z shortcuts
    /// (`tab.rs::handle_history_hotkeys`), which is acceptable for a modal editor window but not
    /// for a surface that lives on the canvas for a whole session.
    fn block_canvas_zoom(&self) -> bool {
        false
    }

    /// Never `true`.
    ///
    /// The tab ORs this flag into `canvas_pointer_occluded` and then refuses every stroke, key
    /// and cursor callback over the captured area — and this tool drives its entire gesture
    /// through those callbacks.
    fn captures_canvas_pointer(&self, _pointer_pos: Pos2) -> bool {
        false
    }

    fn ensure_hover_overlay(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        // Same reason as in `stroke_begin`: the selection's coordinate space is the overlay's.
        // A page that is not laid out yet simply gets no overlay this frame.
        let _ = BrushToolBase::ensure_overlay_under_point(canvas, point);
    }
}

#[cfg(test)]
mod tests {
    use super::super::base::StrokeModifiers;
    use super::*;

    /// An axis-aligned selection on page 0, given as `(x0, y0, x1, y1)` page pixels.
    fn selection(x0: f32, y0: f32, x1: f32, y1: f32) -> PatchSelection {
        PatchSelection {
            page_idx: 0,
            points: rect_corners((x0, y0), (x1, y1)),
        }
    }

    /// A press on `page_idx`. The scene position is irrelevant to every test here: a default
    /// `CanvasView` has no laid-out page, so `page_pos` refuses and the canvas-dependent half of
    /// `stroke_begin` is a no-op.
    fn press(page_idx: usize) -> StrokePoint {
        StrokePoint {
            page_idx,
            scene_pos: egui::pos2(0.0, 0.0),
            modifiers: StrokeModifiers::default(),
        }
    }

    /// A press inside the current selection begins the SOURCE drag, not a new selection.
    #[test]
    fn a_press_inside_the_selection_starts_the_source_drag() {
        let current = selection(10.0, 10.0, 40.0, 40.0);
        let gesture = gesture_for_press(Some(&current), PatchShape::Lasso, 0, (25.0, 25.0));
        assert!(matches!(gesture, PatchGesture::Source { page_idx: 0, .. }));
    }

    /// A press outside it — including on another page — begins a new selection of the
    /// configured shape instead.
    #[test]
    fn a_press_outside_the_selection_starts_a_new_one() {
        let current = selection(10.0, 10.0, 40.0, 40.0);
        assert!(matches!(
            gesture_for_press(Some(&current), PatchShape::Lasso, 0, (80.0, 80.0)),
            PatchGesture::Lasso { .. }
        ));
        assert!(matches!(
            gesture_for_press(Some(&current), PatchShape::Rect, 0, (80.0, 80.0)),
            PatchGesture::Rect { .. }
        ));
        // Same coordinates, different page: the selection belongs to page 0 only.
        assert!(matches!(
            gesture_for_press(Some(&current), PatchShape::Lasso, 1, (25.0, 25.0)),
            PatchGesture::Lasso { .. }
        ));
        assert!(matches!(
            gesture_for_press(None, PatchShape::Rect, 0, (25.0, 25.0)),
            PatchGesture::Rect { .. }
        ));
    }

    /// A degenerate selection leaves NO selection behind, so the user cannot drag a
    /// half-built outline; a usable one is accepted.
    #[test]
    fn a_degenerate_selection_is_rejected_and_leaves_nothing() {
        let mut tool = PatchTool::default();
        tool.commit_selection(0, vec![(5.0, 5.0), (6.0, 5.0)]);
        assert!(tool.selection.is_none(), "two vertices are not a polygon");
        tool.commit_selection(0, rect_corners((5.0, 5.0), (6.0, 40.0)));
        assert!(tool.selection.is_none(), "a one-pixel-wide box has no interior");
        tool.commit_selection(0, rect_corners((5.0, 5.0), (40.0, 40.0)));
        assert!(tool.selection.is_some(), "a real box must be accepted");
    }

    /// Escape clears the selection, and is not consumed when there is nothing to clear.
    #[test]
    fn escape_clears_the_selection_and_is_otherwise_not_consumed() {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });

        let mut tool = PatchTool::default();
        let mut handled_when_empty = true;
        let _ = ctx.run_ui(input.clone(), |ui| {
            handled_when_empty = tool.on_key_event(ui.ctx());
        });
        assert!(!handled_when_empty, "Escape must stay available when nothing is selected");

        tool.selection = Some(selection(10.0, 10.0, 40.0, 40.0));
        let mut handled = false;
        let _ = ctx.run_ui(input, |ui| {
            handled = tool.on_key_event(ui.ctx());
        });
        assert!(handled, "Escape must be consumed when it clears a selection");
        assert!(tool.selection.is_none());
    }

    /// Escape does not merely drop the selection: it CANCELS the work in flight.
    ///
    /// A job that survived would land a patch for a selection the user has explicitly dismissed,
    /// over whatever was drawn in the meantime — and would keep `busy()` true, refusing the next
    /// selection with `status_busy`.
    #[test]
    fn escape_cancels_the_work_in_flight() {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });

        let mut tool = PatchTool::default();
        tool.selection = Some(selection(10.0, 10.0, 40.0, 40.0));
        tool.pending_offset = Some((5, 5));
        let (_solve_tx, solve_rx) = mpsc::channel::<PatchOutcome>();
        tool.solve_rx = Some(solve_rx);
        assert!(tool.busy(), "the fixture is a tool with a job in flight");

        let mut handled = false;
        let _ = ctx.run_ui(input, |ui| {
            handled = tool.on_key_event(ui.ctx());
        });
        assert!(handled, "Escape is consumed when it has something to cancel");
        assert!(tool.selection.is_none());
        assert!(!tool.busy(), "a job the user dismissed must not still be in flight");
        assert!(tool.pending_offset.is_none() && tool.solve_rx.is_none());
        assert!(tool.pending_load.is_none());

        // And the tool is immediately usable again: a fresh press is not bounced as busy.
        let mut canvas = CanvasView::default();
        tool.stroke_begin(&mut canvas, press(0));
        assert!(tool.status.is_none(), "a fresh selection must not be refused after a cancel");
    }

    /// The «clear selection» button cancels the work in flight for the same reason.
    ///
    /// It and Escape both go through `clear_selection`, which is what makes the two entry points
    /// impossible to fix apart; a parked gesture end goes with them, so nothing can be committed
    /// after the selection it belongs to is gone.
    #[test]
    fn clearing_the_selection_cancels_the_work_in_flight() {
        let mut tool = PatchTool::default();
        tool.selection = Some(selection(10.0, 10.0, 40.0, 40.0));
        tool.pending_end = Some(PatchGesture::Source {
            page_idx: 0,
            start: (20.0, 20.0),
            current: (60.0, 60.0),
        });
        tool.pending_offset = Some((5, 5));
        let (_solve_tx, solve_rx) = mpsc::channel::<PatchOutcome>();
        tool.solve_rx = Some(solve_rx);

        tool.clear_selection(&"the clear-selection button was pressed");
        assert!(tool.selection.is_none());
        assert!(tool.pending_end.is_none(), "a parked end must not outlive its selection");
        assert!(!tool.busy(), "the button must call a running patch off, not leave it landing later");
        assert!(tool.status.is_none());
    }

    /// The source-drag outline is drawn where the source ACTUALLY is, never pinned to the page.
    ///
    /// `base::overlay_pos_to_scene_pos` CLAMPS, which for an aiming outline would claim the
    /// pixels are read at the page border. This map must not: a translated point outside the
    /// page has to land outside the page's scene rect, on the side it left by.
    #[test]
    fn the_source_outline_is_not_clamped_to_the_page() {
        // A 256x256-pixel page drawn into a 256x256-unit rect: one page pixel is one scene unit,
        // and every coordinate below is exact in binary.
        let page = egui::Rect::from_min_max(egui::pos2(100.0, 50.0), egui::pos2(356.0, 306.0));
        let size = [256, 256];

        let inside = scene_pos_in_page(page, size, [64.0, 128.0]).expect("the page rect is usable");
        assert_eq!(inside, egui::pos2(164.0, 178.0));

        let before = scene_pos_in_page(page, size, [-32.0, -16.0]).expect("the page rect is usable");
        assert_eq!(before, egui::pos2(68.0, 34.0));
        assert!(
            before.x < page.left() && before.y < page.top(),
            "a clamped map would have pinned this to the page's corner"
        );

        let after = scene_pos_in_page(page, size, [288.0, 320.0]).expect("the page rect is usable");
        assert_eq!(after, egui::pos2(388.0, 370.0));
        assert!(after.x > page.right() && after.y > page.bottom());

        // Degenerate inputs answer `None` instead of dividing by zero.
        assert!(scene_pos_in_page(page, [0, 256], [1.0, 1.0]).is_none());
        let empty = egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(10.0, 10.0));
        assert!(scene_pos_in_page(empty, size, [1.0, 1.0]).is_none());
    }

    /// What the outline says and what the release does come from ONE predicate.
    ///
    /// Both `paint` and `start_job` consume `drag_geometry`'s answer, so the whole surface on
    /// which they could disagree is this mapping — which is why it is total: EVERY refusal
    /// reason marks the outline unusable, and every one of them reaches the user.
    #[test]
    fn every_refusal_reason_marks_the_source_outline_unusable() {
        let accepted = DragGeometry {
            roi: OverlayRectPx {
                x: 2,
                y: 2,
                w: 40,
                h: 40,
            },
            source_size: [200, 200],
        };
        assert_eq!(OutlineTone::for_drag(&Ok(accepted)), OutlineTone::Normal);
        for refusal in [
            DragRefusal::DegenerateSelection { page_idx: 0 },
            DragRefusal::PageNotLaidOut { page_idx: 0 },
            DragRefusal::SourceOutsidePage {
                bounds: (0.0, 0.0, 40.0, 40.0),
                offset: (170, 0),
                source_w: 200,
                source_h: 200,
            },
        ] {
            assert_eq!(
                OutlineTone::for_drag(&Err(refusal)),
                OutlineTone::Refused,
                "{refusal} must be visible before the release, not only after it"
            );
            assert!(!refusal.user_message().is_empty(), "{refusal} must reach the user");
        }
        assert_ne!(
            OutlineTone::Refused.light(),
            OutlineTone::Normal.light(),
            "a refused drag must not look like an accepted one"
        );

        // The only canvas state a unit test can build — no page laid out — travels through the
        // shared helper to the outline exactly as it travels to `start_job`.
        let canvas = CanvasView::default();
        let answer = PatchTool::drag_geometry(&canvas, &selection(10.0, 10.0, 40.0, 40.0), (5, 5));
        assert!(matches!(answer, Err(DragRefusal::PageNotLaidOut { page_idx: 0 })));
        assert_eq!(OutlineTone::for_drag(&answer), OutlineTone::Refused);
    }

    /// The ROI is the selection's box unioned with its translated copy, padded on every side.
    #[test]
    fn the_roi_covers_both_boxes_with_padding() {
        let roi = PatchTool::roi_for((20.0, 30.0, 40.0, 50.0), (10, -10), 200, 200)
            .expect("the padded union fits on a 200x200 page");
        assert_eq!(roi.x, 20 - ROI_PAD_PX);
        assert_eq!(roi.y, 30 - 10 - ROI_PAD_PX);
        assert_eq!(roi.x + roi.w, 40 + 10 + ROI_PAD_PX);
        assert_eq!(roi.y + roi.h, 50 + ROI_PAD_PX);
    }

    /// A padded ROI that leaves the page is REFUSED, never clamped: a clamped ROI would
    /// silently sample pixels the user did not point at.
    #[test]
    fn a_roi_leaving_the_page_is_refused() {
        // The source drag runs off the right edge.
        assert!(PatchTool::roi_for((20.0, 20.0, 40.0, 40.0), (170, 0), 200, 200).is_none());
        // The source drag runs off the top edge.
        assert!(PatchTool::roi_for((20.0, 20.0, 40.0, 40.0), (0, -30), 200, 200).is_none());
        // No drag at all, but the selection itself sits flush against the page corner, so the
        // padding alone leaves it.
        assert!(PatchTool::roi_for((0.0, 0.0, 40.0, 40.0), (0, 0), 200, 200).is_none());
    }

    /// The drag offset is whole page pixels, because the source is SAMPLED at
    /// `destination + offset` and a fractional offset would need resampling.
    #[test]
    fn the_drag_offset_is_whole_pixels() {
        assert_eq!(drag_offset((10.0, 10.0), (17.4, 2.6)), (7, -7));
        assert_eq!(drag_offset((10.0, 10.0), (10.2, 9.8)), (0, 0));
    }

    /// The translated source lands the offset pixel on each destination pixel.
    #[test]
    fn the_translated_source_shifts_by_the_offset() {
        let w = 4;
        let h = 3;
        let dst: Vec<[u8; 3]> = (0..w * h).map(|idx| [idx as u8, 0, 0]).collect();
        let src = translated_source(&dst, w, h, (1, 1));
        // (1,1) reads (2,2) == index 10.
        assert_eq!(src[w + 1], [10, 0, 0]);
        // The far corner has no source inside the ROI and clamps to the last pixel.
        assert_eq!(src[2 * w + 3], [11, 0, 0]);
    }

    /// `block_canvas_zoom` must stay `false`.
    ///
    /// The flag also disables the clean-overlay Ctrl+Z / Ctrl+Shift+Z shortcuts
    /// (`tab.rs::handle_history_hotkeys`), and this tool's selection lives on the canvas for a
    /// whole session — undo has to keep working the entire time.
    #[test]
    fn canvas_zoom_is_never_blocked() {
        assert!(!PatchTool::default().block_canvas_zoom());
    }

    /// `captures_canvas_pointer` must stay `false`.
    ///
    /// The tab ORs it into `canvas_pointer_occluded` and then calls `finish_stroke()` and
    /// returns, so a capturing tool receives no `stroke_begin`/`stroke_update`/`stroke_end`, no
    /// `on_key_event` and no `draw_cursor` — and this tool's entire gesture rides those.
    #[test]
    fn the_canvas_pointer_is_never_captured() {
        let tool = PatchTool::default();
        for pos in [egui::pos2(0.0, 0.0), egui::pos2(500.0, 500.0)] {
            assert!(!tool.captures_canvas_pointer(pos));
        }
    }

    /// A `stroke_end` that is not a real pointer release must NEITHER apply a patch NOR close a
    /// lasso.
    ///
    /// The tab issues such an end whenever a drag crosses a page seam or leaves the canvas
    /// rectangle with the button still held; treating it as "the user let go" would commit the
    /// patch mid-gesture and truncate a selection the user is still drawing.
    #[test]
    fn a_stroke_end_with_the_button_still_held_neither_applies_nor_closes() {
        let mut canvas = CanvasView::default();

        // A source drag: the parked end must not turn into a job.
        let mut tool = PatchTool::default();
        tool.selection = Some(selection(10.0, 10.0, 40.0, 40.0));
        tool.gesture = Some(PatchGesture::Source {
            page_idx: 0,
            start: (20.0, 20.0),
            current: (60.0, 60.0),
        });
        tool.stroke_end(&mut canvas);
        assert!(tool.pending_end.is_some(), "the end is parked, not acted on");
        assert!(tool.pending_offset.is_none(), "nothing may be applied from `stroke_end` itself");
        tool.resolve_pending_end(false);
        assert!(tool.pending_offset.is_none(), "a held button must not start a patch");
        assert!(!tool.busy(), "and must leave no work in flight");
        assert!(tool.selection.is_some(), "the completed selection survives the cancellation");

        // The same drag, released for real: this is what a patch looks like.
        tool.gesture = Some(PatchGesture::Source {
            page_idx: 0,
            start: (20.0, 20.0),
            current: (60.0, 60.0),
        });
        tool.stroke_end(&mut canvas);
        tool.resolve_pending_end(true);
        assert_eq!(tool.pending_offset, Some((40, 40)));

        // A lasso: the parked end must not close the polygon into a selection.
        let mut tool = PatchTool::default();
        tool.gesture = Some(PatchGesture::Lasso {
            page_idx: 0,
            points: rect_corners((5.0, 5.0), (50.0, 50.0)),
        });
        tool.stroke_end(&mut canvas);
        tool.resolve_pending_end(false);
        assert!(tool.selection.is_none(), "a held button must not close a lasso");
        assert!(tool.gesture.is_none(), "the truncated shape is discarded, not kept");

        // Released for real, the same polygon does become the selection.
        tool.gesture = Some(PatchGesture::Lasso {
            page_idx: 0,
            points: rect_corners((5.0, 5.0), (50.0, 50.0)),
        });
        tool.stroke_end(&mut canvas);
        tool.resolve_pending_end(true);
        assert!(tool.selection.is_some());
    }

    /// A gesture that crosses onto ANOTHER page is cancelled, never applied.
    ///
    /// `tab.rs::handle_active_tool_input` ends and immediately re-begins the stroke at the seam.
    /// A selection and the source it is dragged onto must live on one page, so both gesture kinds
    /// are dropped there: the source drag without applying, the lasso without being truncated.
    #[test]
    fn a_page_change_cancels_the_gesture_instead_of_applying_it() {
        let mut canvas = CanvasView::default();

        // A source drag crossing the seam: no job, and the selection it started from survives.
        let mut tool = PatchTool::default();
        tool.selection = Some(selection(10.0, 10.0, 40.0, 40.0));
        tool.gesture = Some(PatchGesture::Source {
            page_idx: 0,
            start: (20.0, 20.0),
            current: (60.0, 60.0),
        });
        tool.stroke_end(&mut canvas);
        tool.stroke_begin(&mut canvas, press(1));
        assert!(tool.pending_offset.is_none(), "the patch must not be applied at the seam");
        assert!(tool.pending_end.is_none(), "and the parked end must not linger");
        assert!(!tool.busy());
        assert!(tool.gesture.is_none(), "no gesture is started on the page it crossed onto");
        let surviving = tool.selection.as_ref().expect("the completed selection survives");
        assert_eq!(surviving.page_idx, 0);
        assert!(tool.status.is_some(), "the cancellation is reported to the user");

        // A lasso crossing the seam: discarded, not closed into a truncated selection.
        let mut tool = PatchTool::default();
        tool.gesture = Some(PatchGesture::Lasso {
            page_idx: 0,
            points: rect_corners((5.0, 5.0), (50.0, 50.0)),
        });
        tool.stroke_end(&mut canvas);
        tool.stroke_begin(&mut canvas, press(1));
        assert!(tool.selection.is_none(), "a lasso must not be closed at the seam");
        assert!(tool.pending_end.is_none());
        assert!(tool.gesture.is_none());

        // A parked end on the SAME page cannot be a crossing — the tab only ends and re-begins
        // when the page differs — so it is the release it looks like and still commits.
        let mut tool = PatchTool::default();
        tool.gesture = Some(PatchGesture::Lasso {
            page_idx: 0,
            points: rect_corners((5.0, 5.0), (50.0, 50.0)),
        });
        tool.stroke_end(&mut canvas);
        tool.stroke_begin(&mut canvas, press(0));
        assert!(tool.selection.is_some(), "a same-page press resolves the end as a release");
    }

    /// `deactivate` abandons everything in flight.
    ///
    /// `draw_overlay_ui` runs for the ACTIVE tool only, so a load or a solve left running past a
    /// tool switch is never polled again — and would later be applied against the overlay snapshot
    /// taken before the switch, over whatever the user did in between.
    #[test]
    fn deactivating_abandons_every_job_in_flight() {
        let mut canvas = CanvasView::default();
        let mut tool = PatchTool::default();
        tool.selection = Some(selection(10.0, 10.0, 40.0, 40.0));
        tool.pending_offset = Some((5, 5));
        tool.pending_end = Some(PatchGesture::Lasso {
            page_idx: 0,
            points: rect_corners((5.0, 5.0), (50.0, 50.0)),
        });
        tool.pending_load = Some(PendingLoad {
            composite_job_id: 0,
            page_job_id: 1,
            page_idx: 0,
            roi: OverlayRectPx {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            },
            offset: (1, 1),
            polygon_roi: rect_corners((2.0, 2.0), (6.0, 6.0)),
            overlay_chunk: egui::ColorImage::filled([8, 8], Color32::TRANSPARENT),
            blend: PatchBlend::None,
            feather_px: 0,
            composite: None,
            page: None,
        });
        let (_tx, rx) = mpsc::channel::<PatchOutcome>();
        tool.solve_rx = Some(rx);
        assert!(tool.busy(), "the fixture really is in flight");

        tool.deactivate(&mut canvas);

        assert!(!tool.busy(), "nothing may survive the tool switch");
        assert!(tool.pending_end.is_none());
        assert!(tool.pending_offset.is_none());
        assert!(tool.pending_load.is_none());
        assert!(tool.solve_rx.is_none());
        assert!(tool.gesture.is_none());
        assert!(tool.selection.is_some(), "the selection itself is kept, as Photoshop does");
    }

    /// The commit refuses a chunk that does not fit the LIVE overlay instead of letting
    /// `replace_overlay_region_px` clip the rectangle and nearest-rescale the chunk into it.
    #[test]
    fn a_chunk_that_does_not_fit_the_live_overlay_is_refused() {
        let roi = OverlayRectPx {
            x: 10,
            y: 10,
            w: 20,
            h: 20,
        };
        assert!(check_chunk_fits([20, 20], roi, Some([200, 200])).is_ok());
        // No overlay allocated yet: the write creates one, so only the size equality applies.
        assert!(check_chunk_fits([20, 20], roi, None).is_ok());
        assert!(matches!(
            check_chunk_fits([19, 20], roi, Some([200, 200])),
            Err(ApplyError::ChunkSize { .. })
        ));
        // The page shrank while the solve ran.
        assert!(matches!(
            check_chunk_fits([20, 20], roi, Some([25, 200])),
            Err(ApplyError::OutOfBounds { .. })
        ));
        assert!(matches!(
            check_chunk_fits([20, 20], roi, Some([200, 25])),
            Err(ApplyError::OutOfBounds { .. })
        ));
    }

    /// One well-formed worker job, 8x8 with a 4x4 selection well clear of the ROI border.
    fn job_input() -> PatchJobInput {
        let roi = OverlayRectPx {
            x: 0,
            y: 0,
            w: 8,
            h: 8,
        };
        PatchJobInput {
            page_idx: 0,
            roi,
            offset: (0, 0),
            polygon_roi: rect_corners((2.0, 2.0), (6.0, 6.0)),
            composite: egui::ColorImage::filled([8, 8], Color32::from_rgb(200, 200, 200)),
            page: egui::ColorImage::filled([8, 8], Color32::WHITE),
            overlay_chunk: egui::ColorImage::filled([8, 8], Color32::TRANSPARENT),
            blend: PatchBlend::None,
            feather_px: 0,
        }
    }

    /// A malformed input can never yield `Ok`, and therefore can never reach the overlay.
    ///
    /// `run_patch_job` is a pure function of its input — it is handed no canvas — so refusing here
    /// is what keeps a size disagreement from being committed. The clean-overlay chunk is the
    /// dangerous one: substituting a transparent stand-in for it would erase every pre-existing
    /// cleaning pixel of the ROI the moment the outcome is applied.
    #[test]
    fn a_malformed_worker_input_is_refused_and_never_yields_a_chunk() {
        assert!(run_patch_job(job_input()).chunk.is_ok(), "the fixture itself must solve");

        for (name, input) in [
            ("clean-overlay chunk", {
                let mut input = job_input();
                input.overlay_chunk = egui::ColorImage::filled([1, 1], Color32::TRANSPARENT);
                input
            }),
            ("composited destination", {
                let mut input = job_input();
                input.composite = egui::ColorImage::filled([4, 4], Color32::WHITE);
                input
            }),
            ("original page", {
                let mut input = job_input();
                input.page = egui::ColorImage::filled([9, 8], Color32::WHITE);
                input
            }),
            ("empty ROI", {
                let mut input = job_input();
                input.roi = OverlayRectPx {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                };
                input
            }),
        ] {
            let outcome = run_patch_job(input);
            let Err(message) = outcome.chunk else {
                panic!("a malformed {name} must be refused, not substituted");
            };
            assert!(!message.is_empty(), "the refusal carries a user-facing sentence");
        }
    }

    /// Every ROI-sized buffer is checked ONCE, up front, and the first disagreement is named.
    #[test]
    fn the_worker_names_the_buffer_that_disagrees_with_the_roi() {
        let mut input = job_input();
        input.overlay_chunk = egui::ColorImage::filled([1, 1], Color32::TRANSPARENT);
        assert!(matches!(
            validate_job_input(&input),
            Err(PatchInputError::BufferSize {
                buffer: PatchBuffer::OverlayChunk,
                got_w: 1,
                got_h: 1,
                want_w: 8,
                want_h: 8,
                ..
            })
        ));
        assert!(validate_job_input(&job_input()).is_ok());
    }
}
