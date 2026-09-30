/*
File: tools/patch/mod.rs

Purpose:
The host-neutral core of the «Заплатка» (patch) tool — Photoshop's Patch Tool. The user draws a
free-form (lasso) or rectangular selection on a page, then drags from inside it onto a clean SOURCE
area; on release the source pixels are copied into the selection and colour-adapted to the
destination's contour by the gradient-domain solve in `membrane.rs`.

Main responsibilities:
- Own the selection (in PAGE pixels, re-projected every frame) and the two-phase gesture.
- Turn a finished drag into an off-thread job: ask the host to load the ROI, solve the membrane,
  and hand the host `(page, rect, rgb, coverage)`.
- Paint the dashed selection outlines and the tool's controls.

Key structures:
- `PatchToolCore`: the whole state machine, driven by a host through the hooks below.
- `PatchHost`: everything the core needs from its host — geometry, the region load, the commit.
- `PatchShape`: free-form or rectangular selection.
- `PatchSelection`: the closed polygon plus the page it belongs to.
- `PatchGesture`: the live gesture (lasso, rectangle drag, source drag).
- `PatchCommit`: what one finished patch hands the host.
- `DragGeometry` / `DragRefusal`: what a source drag resolves to, or why it is refused.
- `PatchInputError`: the refusal reasons of every ROI-sized buffer, core-side and host-side alike.

Key functions:
- `drag_geometry()`: THE refusal predicate — the preview outline and the release both read it.
- `start_job()`: the region-load request for an accepted drag.
- `run_patch_job()`: the whole worker-side pass (rasterize, solve, blend by coverage).
- `gesture_for_press()`: the whole press-time state machine, host-free and unit-tested.
- `resolve_pending_end()`: turns a parked `stroke_end` into a commit or a cancellation.
- `clear_selection()`: drops the selection AND cancels the work in flight (Escape, the button).
- `paint_outline()`: the zoom-invariant two-tone dashed outline, unclamped and marked when refused.
- `scene_pos_in_page()`: the unclamped page-pixel -> scene map that outline is projected with.

Notes:
The core STOPS at `(page_idx, roi, rgb, coverage)`. How those pixels are stored — over which
backdrop, into which layer, as which undo step — is the host's business and is deliberately absent
from this module, together with every canvas, project and overlay type. A host that stores into a
clean overlay needs the ORIGINAL page pixels under the ROI as well; it issues that second load
itself, because it is the only party that consumes it.
*/
mod membrane;

use ms_canvas::OverlayRectPx;
use crate::fill_polygon_spans;
use ms_widgets::WheelSlider;
use eframe::egui;
use egui::{Color32, Pos2};
use membrane::{PatchBlend, PatchRequest, solve_patch};
use ms_thread as thread;
use std::sync::mpsc::{self, Receiver, TryRecvError};

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

/// Where one page sits on screen, and how many pixels its page-pixel space has.
///
/// The two values travel together because the projection needs both and neither is meaningful
/// without the other: the scene rect says where the page is drawn, `pixel_size` says what one
/// page pixel is worth in it.
#[derive(Debug, Clone, Copy)]
pub struct PagePixelProjection {
    /// The whole page on screen, in scene (== screen) points.
    pub scene_rect: egui::Rect,
    /// The page's size in the page-pixel space the selection is stored in.
    pub pixel_size: [usize; 2],
}

/// One region the core needs loaded before it can solve.
///
/// `source_size` is the page size the ROI was bounds-checked against; a host that crops must crop
/// against exactly that size, or the answer cannot be the rectangle the core measured.
#[derive(Debug, Clone, Copy)]
pub struct PatchRegionRequest {
    pub page_idx: usize,
    /// The padded ROI, in page pixels.
    pub roi: OverlayRectPx,
    /// The page's size in source pixels, as the core validated the ROI against.
    pub source_size: [usize; 2],
}

/// What a host answers when the core polls the region load it asked for.
#[derive(Debug)]
pub enum PatchRegionPoll {
    /// Nothing has arrived yet; the core polls again next frame.
    Pending,
    /// The COMPOSITED destination pixels over the whole ROI, exactly `roi.w` x `roi.h`.
    Ready(egui::ColorImage),
    /// The load failed. The payload is English technical context for the log; the core supplies
    /// the localized sentence.
    Failed(String),
}

/// Why a host refused to start a region load.
///
/// Two fields because the two audiences are different, exactly as `PatchToolCore::report_error`
/// separates them: the user gets a sentence, the log gets the numbers.
#[derive(Debug, Clone)]
pub struct PatchHostError {
    /// The already-localized sentence shown to the user.
    pub user_message: String,
    /// English technical context; log only.
    pub detail: String,
}

/// One finished patch, handed to the host to store.
///
/// `rgb` is the colour each ROI pixel must END UP showing — the membrane's solution already
/// blended into the destination by `coverage` — and `coverage` is how strongly the patch claims
/// that pixel, in `0..=1`. Both are `roi.w * roi.h` entries, row-major, and are the same length.
///
/// A pixel whose `coverage` is `0.0` is NOT part of the patch: the host must leave whatever is
/// already stored there untouched, because the commit replaces the whole ROI and anything else
/// would erase the surrounding work. Its `rgb` entry is the destination colour and carries no
/// information.
#[derive(Debug, Clone, Copy)]
pub struct PatchCommit<'a> {
    pub page_idx: usize,
    pub roi: OverlayRectPx,
    /// The final colour of each ROI pixel.
    pub rgb: &'a [[u8; 3]],
    /// How strongly the patch claims each ROI pixel, `0..=1`.
    pub coverage: &'a [f32],
}

/// Everything `PatchToolCore` needs from the surface it is driven on.
///
/// The core knows its host through this trait ONLY. Three groups: the geometry the on-canvas
/// gesture is projected with, the region load (which must not decode on the GUI thread — see
/// CLAUDE.md §5), and the commit. Nothing here mentions a canvas, a project, an overlay or an
/// undo stack: which of those a patch lands in is exactly what the trait exists to abstract.
pub trait PatchHost {
    /// The page's size in SOURCE pixels, or `None` while the page is not laid out this frame.
    ///
    /// This is what the ROI is bounds-checked against and what a loader crops against, so the two
    /// can never disagree about the page.
    fn page_source_size(&self, page_idx: usize) -> Option<[usize; 2]>;

    /// Where the page is drawn and how large its page-pixel space is, or `None` when it is not
    /// laid out, has no pixel space yet, or either is degenerate.
    fn page_projection(&self, page_idx: usize) -> Option<PagePixelProjection>;

    /// The part of the viewport the outline may be painted in, with any dock panels already cut
    /// out. `None` when there is no viewport or nothing is left of it.
    fn usable_viewport(&self) -> Option<egui::Rect>;

    /// A scene (== screen) position as fractional PAGE pixels of `page_idx`, CLAMPED to the page.
    ///
    /// `None` when the page has no page-pixel space or no scene rect yet — which is also when the
    /// selection could not be projected back for painting.
    fn scene_pos_to_page_pos(&self, page_idx: usize, scene_pos: Pos2) -> Option<(f32, f32)>;

    /// Makes sure `page_idx` has a page-pixel space under `scene_pos` before a vertex is taken.
    ///
    /// The selection's coordinate space is the host's storage space, so it has to exist before
    /// the first vertex. A host with nothing to allocate implements this as a no-op.
    fn ensure_page_pixels(&mut self, page_idx: usize, scene_pos: Pos2);

    /// Starts loading the COMPOSITED destination pixels over `request.roi`.
    ///
    /// Must not decode on the GUI thread. Any state the host needs at commit time — a second
    /// load, a snapshot of what is stored under the ROI — is started and retained here.
    ///
    /// # Errors
    /// A `PatchHostError` whose `user_message` the core shows and whose `detail` it logs.
    fn start_region_load(&mut self, request: &PatchRegionRequest) -> Result<(), PatchHostError>;

    /// Polls the load started by `start_region_load`. Non-blocking.
    ///
    /// Called on EVERY frame pass, not only while a load is outstanding: a host whose answer
    /// arrives on a worker channel can only RELEASE an abandoned job's buffers — for the cleaning
    /// host, a decoded page region of several megabytes — on a poll, and one that happened solely
    /// while a job was pending would hold them until the next job drained the channel. An
    /// implementation must therefore be idempotent with nothing in flight and answer `Pending`
    /// there; anything else it returns while the core is not waiting is dropped.
    fn poll_region_load(&mut self) -> PatchRegionPoll;

    /// Drops everything retained for the current region LOAD.
    ///
    /// Called when the user cancels and after every finished solve, so a host never holds an
    /// ROI-sized buffer for a job that will never be committed. Must be idempotent.
    ///
    /// It is called immediately after an accepted [`PatchHost::commit_patch`], so a host that
    /// DEFERS its write must not drop it here: what this discards is the job's scratch state, not
    /// the patch the host has already accepted.
    fn discard_region_load(&mut self);

    /// Stores one finished patch as ONE undo step.
    ///
    /// # Errors
    /// An already-localized sentence for the user. The host logs its own technical detail; the
    /// core only shows what comes back.
    fn commit_patch(&mut self, patch: &PatchCommit<'_>) -> Result<(), String>;
}

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
            Self::Lasso => t!("tools.patch.shape_free"),
            Self::Rect => t!("tools.patch.shape_rect"),
        }
    }
}

/// Localized caption of a colour-adaptation button.
fn blend_title(blend: PatchBlend) -> &'static str {
    match blend {
        PatchBlend::None => t!("tools.patch.blend_none"),
        PatchBlend::Linear => t!("tools.patch.blend_linear"),
        PatchBlend::Multiplicative => t!("tools.patch.blend_multiplicative"),
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

/// Which ROI-sized buffer disagreed with the ROI.
///
/// `Composite` is checked by the core; the other two belong to a host that stores over an
/// original backdrop, and are named here so both sides refuse with ONE vocabulary and one set of
/// localized sentences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchBuffer {
    /// The page composited with whatever is already stored — the membrane's destination.
    Composite,
    /// The ORIGINAL page pixels under the ROI, the backdrop a host solves its stored pixel over.
    Page,
    /// The already-stored pixels the patch is written into.
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

/// Why a patch could not be built or stored.
///
/// None of these is recoverable by guessing. Substituting a value for a buffer whose size cannot
/// be explained would commit that guess over the user's work — an all-transparent stand-in for
/// the stored chunk erases every pre-existing pixel of the ROI — so every one of them is REFUSED
/// and reported instead (CLAUDE.md §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PatchInputError {
    /// The ROI has no pixels at all, so nothing can be solved or written.
    #[error("the ROI is {w}x{h}, which has no pixels")]
    EmptyRoi { w: usize, h: usize },
    /// One of the ROI-sized inputs is not exactly `roi.w` x `roi.h`.
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
    #[must_use]
    pub fn user_message(self) -> String {
        match self {
            Self::EmptyRoi { .. } => t!("tools.patch.error_region_size").to_string(),
            Self::BufferSize { buffer, .. } => match buffer {
                PatchBuffer::Composite | PatchBuffer::Page => {
                    t!("tools.patch.error_region_size").to_string()
                }
                PatchBuffer::OverlayChunk => {
                    t!("tools.patch.error_overlay_size").to_string()
                }
            },
            Self::SolvedLength { .. } => t!("tools.patch.error_solve").to_string(),
        }
    }
}

/// The geometry one accepted source drag resolves to.
///
/// The two values travel together because they are decided together: the ROI is only valid
/// against the `source_size` it was bounds-checked with, and the loader crops against that same
/// size (`PatchRegionRequest`).
#[derive(Debug, Clone, Copy)]
struct DragGeometry {
    /// The padded ROI covering the selection's box and its translated copy, in page pixels.
    roi: OverlayRectPx,
    /// The page's source size the ROI was validated against.
    source_size: [usize; 2],
}

/// Why a source drag cannot be turned into a patch.
///
/// Produced by `PatchToolCore::drag_geometry`, which is the ONLY place the rule lives: the
/// preview outline asks it every frame of the drag and `start_job` asks it at the release, so
/// what the user sees while dragging and what happens when the button comes up cannot disagree.
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
                t!("tools.patch.status_selection_too_small").to_string()
            }
            Self::PageNotLaidOut { page_idx } => {
                tf!("tools.patch.error_page_missing", page = page_idx + 1)
            }
            Self::SourceOutsidePage { .. } => {
                t!("tools.patch.error_source_outside_page").to_string()
            }
        }
    }
}

/// A job waiting for its region load.
///
/// Only the values the SOLVE needs. What a host has to keep in order to store the answer — a
/// second load, a snapshot of the destination — is the host's own state, not this.
#[derive(Debug)]
struct PendingLoad {
    page_idx: usize,
    roi: OverlayRectPx,
    offset: (i32, i32),
    /// The selection polygon translated into ROI-local coordinates.
    polygon_roi: Vec<(f32, f32)>,
    blend: PatchBlend,
    feather_px: usize,
}

/// The pixels one finished solve produced, over the whole ROI.
#[derive(Debug)]
struct PatchSolved {
    /// The colour each ROI pixel must end up showing.
    rgb: Vec<[u8; 3]>,
    /// How strongly the patch claims each ROI pixel, `0..=1`.
    coverage: Vec<f32>,
}

/// One finished solve, ready to be handed to the host.
#[derive(Debug)]
struct PatchOutcome {
    page_idx: usize,
    roi: OverlayRectPx,
    /// The solved pixels on success, an already-localized message on failure.
    solved: Result<PatchSolved, String>,
}

/// Everything the worker needs to solve one patch, with no host access.
#[derive(Debug)]
struct PatchJobInput {
    /// The host's log tag, so a refusal logged from the worker names the surface it came from.
    tag: &'static str,
    page_idx: usize,
    roi: OverlayRectPx,
    offset: (i32, i32),
    polygon_roi: Vec<(f32, f32)>,
    composite: egui::ColorImage,
    blend: PatchBlend,
    feather_px: usize,
}

/// Photoshop-style patch tool: select, drag onto a source, get a colour-adapted copy.
///
/// Host-neutral. Every hook takes a `&dyn PatchHost` / `&mut dyn PatchHost`; the core itself
/// stores nothing and knows nothing about where the answer lands.
pub struct PatchToolCore {
    /// Log prefix, printed as `[{tag}]`, so two hosts' log lines stay tellable apart.
    tag: &'static str,
    /// `egui::Id` source of the bare layer the outline is painted on. Distinct per host, so two
    /// hosts never share one layer.
    outline_layer_id: &'static str,
    shape: PatchShape,
    blend: PatchBlend,
    feather_px: usize,
    selection: Option<PatchSelection>,
    gesture: Option<PatchGesture>,
    /// A gesture whose `stroke_end` has arrived but whose fate is not decided yet.
    ///
    /// `stroke_end` cannot tell a real pointer release from the end+begin pair a host issues
    /// when a drag crosses onto another page, because the hook is handed no pointer state. The
    /// finished gesture is therefore parked here and resolved where that state exists: the
    /// following `stroke_begin` (the crossing) or this frame's `draw_overlay_ui` (the release).
    pending_end: Option<PatchGesture>,
    space_pan_active: bool,
    status: Option<String>,
    /// A finished source drag waiting for the next `draw_overlay_ui`.
    ///
    /// `stroke_end` is not the place to start a load, so the offset is parked for exactly one
    /// frame instead of a job being started from the wrong hook.
    pending_offset: Option<(i32, i32)>,
    pending_load: Option<PendingLoad>,
    solve_rx: Option<Receiver<PatchOutcome>>,
    /// A cancellation that still has to reach the host.
    ///
    /// `clear_selection` is reachable from hooks that are handed no host (the Escape key and the
    /// controls pane), so the host-facing half of the cancellation is parked here and flushed at
    /// the top of the next `draw_overlay_ui`.
    host_cancel_pending: bool,
}

impl std::fmt::Debug for PatchToolCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatchToolCore")
            .field("tag", &self.tag)
            .field("shape", &self.shape)
            .field("blend", &self.blend)
            .field("feather_px", &self.feather_px)
            .field("has_selection", &self.selection.is_some())
            .field("busy", &self.busy())
            .finish_non_exhaustive()
    }
}

impl PatchToolCore {
    /// A fresh core for one host.
    ///
    /// `tag` is the log prefix (printed as `[{tag}]`) and `outline_layer_id` the `egui::Id`
    /// source of the bare layer the outline is painted on. Both must be unique per host: two
    /// hosts sharing a layer id would paint into one layer.
    #[must_use]
    pub fn new(tag: &'static str, outline_layer_id: &'static str) -> Self {
        Self {
            tag,
            outline_layer_id,
            shape: PatchShape::default(),
            blend: PatchBlend::default(),
            feather_px: 0,
            selection: None,
            gesture: None,
            pending_end: None,
            space_pan_active: false,
            status: None,
            pending_offset: None,
            pending_load: None,
            solve_rx: None,
            host_cancel_pending: false,
        }
    }

    /// Whether a solve or a load is in flight, so a new gesture must be refused.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.pending_load.is_some() || self.solve_rx.is_some() || self.pending_offset.is_some()
    }

    /// Whether a gesture is LIVE right now — being drawn or dragged, with its start accepted.
    ///
    /// A press the core refused (it was busy, space-pan was held, the page had no placement) is
    /// deliberately NOT one: nothing was started, so nothing is running. A host drives its
    /// `stroke_end` off this, so that a release swallowed by a canvas pan can still be detected as
    /// a LEVEL on the next routed frame.
    #[must_use]
    pub fn gesture_active(&self) -> bool {
        self.gesture.is_some()
    }

    /// Whether a gesture is live OR its end is parked and not yet judged.
    ///
    /// The wider question, and the one a host's own "is a gesture in flight" hook must answer: a
    /// parked end is still an unfinished gesture, because the frame that decides whether it was a
    /// release has not run yet. A running SOLVE is not a gesture — see [`PatchToolCore::busy`].
    #[must_use]
    pub fn gesture_in_flight(&self) -> bool {
        self.gesture.is_some() || self.pending_end.is_some()
    }

    /// Whether space-pan is being held, which suspends the gesture.
    #[must_use]
    pub fn space_pan_active(&self) -> bool {
        self.space_pan_active
    }

    /// Tells the core the host's space-pan state for this frame.
    pub fn set_space_pan_active(&mut self, active: bool) {
        self.space_pan_active = active;
    }

    /// Reports a failure to the user and to the log.
    ///
    /// `text` is the localized, user-facing sentence; `detail` is the technical context that
    /// only belongs in the log.
    fn report_error(&mut self, text: String, detail: &dyn std::fmt::Display) {
        ms_log::runtime_log::log_warn(format!("[{}] {text} | {detail}", self.tag));
        self.status = Some(text);
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
        ms_log::runtime_log::log_info(format!(
            "[{}] the {} gesture on page {} was cancelled: {detail}",
            self.tag,
            gesture.kind(),
            gesture.page_idx()
        ));
        self.status = Some(text);
    }

    /// Turns the parked `stroke_end` into a commit or a cancellation.
    ///
    /// `released` must be the answer to "has the primary pointer button actually been released?",
    /// read from this frame's input. When it is `false` the `stroke_end` came from the host's
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
                t!("tools.patch.status_gesture_cancelled").to_string(),
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
                // `start_job` needs the host, which no stroke hook is given; the drag is
                // parked once more and consumed at the top of the next `draw_overlay_ui`.
                self.pending_offset = Some(offset);
            }
        }
    }

    /// Abandons everything in flight and records what was dropped.
    ///
    /// `draw_overlay_ui` runs for the ACTIVE tool only, so a load or a solve left running past a
    /// tool switch is never polled again — and when it finally were, it would be applied against
    /// the snapshot taken before the switch, over whatever the user did with another tool in
    /// between.
    ///
    /// The host's loader is deliberately not torn down here: it belongs to the host, not to one
    /// gesture. `host_cancel_pending` is what tells the host to drop the buffers it retained.
    fn cancel_in_flight(&mut self, reason: &dyn std::fmt::Display) {
        if self.pending_end.is_none()
            && self.pending_offset.is_none()
            && self.pending_load.is_none()
            && self.solve_rx.is_none()
        {
            return;
        }
        ms_log::runtime_log::log_info(format!(
            "[{}] abandoning the work in flight ({reason}): parked end {}, parked drag {}, region load {}, solve {}",
            self.tag,
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
        self.host_cancel_pending = true;
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
            self.status = Some(t!("tools.patch.status_selection_ready").to_string());
        } else {
            self.selection = None;
            self.status = Some(t!("tools.patch.status_selection_too_small").to_string());
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
        host: &dyn PatchHost,
        selection: &PatchSelection,
        offset: (i32, i32),
    ) -> Result<DragGeometry, DragRefusal> {
        let page_idx = selection.page_idx;
        let bounds = selection
            .bounds()
            .ok_or(DragRefusal::DegenerateSelection { page_idx })?;
        let source_size = host
            .page_source_size(page_idx)
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

    /// Turns a finished source drag into the host's region load.
    ///
    /// Nothing is decoded here: the GUI thread only measures the geometry and hands the host a
    /// rectangle.
    fn start_job(&mut self, host: &mut dyn PatchHost, offset: (i32, i32)) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        // The SAME predicate the preview outline is painted with (`paint`), evaluated in one
        // place: an outline that promised a patch the release then refuses is the bug this
        // shared helper exists to make impossible.
        let DragGeometry { roi, source_size } = match Self::drag_geometry(host, &selection, offset)
        {
            Ok(geometry) => geometry,
            Err(refusal) => {
                self.report_error(refusal.user_message(), &refusal);
                return;
            }
        };
        let page_idx = selection.page_idx;

        // Page pixels -> ROI-local pixels. The polygon is inside its own bounding box and the
        // ROI pads that box by `ROI_PAD_PX`, so the rasterized mask cannot reach the ROI border.
        let polygon_roi: Vec<(f32, f32)> = selection
            .points
            .iter()
            .map(|(x, y)| (x - roi.x as f32, y - roi.y as f32))
            .collect();

        let request = PatchRegionRequest {
            page_idx,
            roi,
            source_size,
        };
        if let Err(error) = host.start_region_load(&request) {
            self.report_error(error.user_message, &error.detail);
            return;
        }

        self.pending_load = Some(PendingLoad {
            page_idx,
            roi,
            offset,
            polygon_roi,
            blend: self.blend,
            feather_px: self.feather_px,
        });
        self.status = Some(t!("tools.patch.status_running").to_string());
    }

    /// Polls the host's region load and spawns the solve once the destination has arrived.
    ///
    /// The host is polled UNCONDITIONALLY, before the pending job is looked at, so a channel-backed
    /// host gets its per-frame chance to drain — see [`PatchHost::poll_region_load`]. An answer
    /// that arrives while nothing is pending belongs to a job that is already over and is dropped
    /// here, which is what makes a cancellation free of a lingering multi-megabyte buffer.
    fn poll_region_load(&mut self, host: &mut dyn PatchHost) {
        let poll = host.poll_region_load();
        if self.pending_load.is_none() {
            return;
        }
        match poll {
            PatchRegionPoll::Pending => {}
            PatchRegionPoll::Failed(error) => {
                self.pending_load = None;
                self.report_error(
                    t!("tools.patch.error_region_load").to_string(),
                    &error,
                );
            }
            PatchRegionPoll::Ready(composite) => {
                let Some(pending) = self.pending_load.take() else {
                    return;
                };
                let input = PatchJobInput {
                    tag: self.tag,
                    page_idx: pending.page_idx,
                    roi: pending.roi,
                    offset: pending.offset,
                    polygon_roi: pending.polygon_roi,
                    composite,
                    blend: pending.blend,
                    feather_px: pending.feather_px,
                };
                let (tx, rx) = mpsc::channel::<PatchOutcome>();
                thread::spawn(move || {
                    let outcome = run_patch_job(input);
                    // A failed send means the tool was dropped or the job abandoned while this
                    // worker ran; there is nobody left to report to and nothing to clean up.
                    let _ = tx.send(outcome);
                });
                self.solve_rx = Some(rx);
            }
        }
    }

    /// Hands a finished solve to the host, which decides how it is stored.
    fn poll_solve(&mut self, host: &mut dyn PatchHost) {
        let Some(rx) = self.solve_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                match outcome.solved {
                    Ok(solved) => {
                        let commit = PatchCommit {
                            page_idx: outcome.page_idx,
                            roi: outcome.roi,
                            rgb: &solved.rgb,
                            coverage: &solved.coverage,
                        };
                        match host.commit_patch(&commit) {
                            Ok(()) => {
                                self.status =
                                    Some(t!("tools.patch.status_applied").to_string());
                            }
                            // The host has already logged its own technical detail; re-logging
                            // here would only duplicate the line.
                            Err(message) => self.status = Some(message),
                        }
                    }
                    Err(error) => {
                        self.report_error(
                            t!("tools.patch.error_solve").to_string(),
                            &error,
                        );
                    }
                }
                // The job is over either way: nothing the host retained for it is needed again.
                host.discard_region_load();
            }
            Err(TryRecvError::Empty) => self.solve_rx = Some(rx),
            Err(TryRecvError::Disconnected) => {
                self.report_error(
                    t!("tools.patch.error_solve").to_string(),
                    &"the patch worker died before answering",
                );
                host.discard_region_load();
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
    /// Painted from `draw_overlay_ui` through a bare layer painter, NOT from a pointer-gated
    /// cursor hook, which returns early whenever the pointer leaves the canvas or is occluded and
    /// would make a session-long selection blink out. A bare layer painter registers no
    /// interactable `Area`, so it steals no input from the canvas.
    fn paint(&self, ctx: &egui::Context, host: &dyn PatchHost) {
        // The host has already cut its dock panels out, so the outline never paints over one.
        let Some(usable) = host.usable_viewport() else {
            return;
        };
        let painter = ctx
            .layer_painter(egui::LayerId::new(
                egui::Order::Middle,
                egui::Id::new(self.outline_layer_id),
            ))
            .with_clip_rect(usable);

        if let Some((page_idx, points)) = self.gesture_polygon() {
            paint_outline(&painter, host, page_idx, &points, (0, 0), OutlineTone::Normal);
            return;
        }
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        paint_outline(
            &painter,
            host,
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
                let tone = OutlineTone::for_drag(&Self::drag_geometry(host, selection, offset));
                paint_outline(
                    &painter,
                    host,
                    selection.page_idx,
                    &selection.points,
                    offset,
                    tone,
                );
            }
        }
    }

    /// Draws the tool's controls: shape, colour adaptation, feather, and the clear button.
    pub fn draw_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(t!("tools.patch.shape_label"));
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

        ui.label(t!("tools.patch.blend_label"));
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
                    .text(t!("tools.patch.feather_label")),
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
                egui::Button::new(t!("tools.patch.clear_selection_button")),
            )
            .clicked()
        {
            self.clear_selection(&"the clear-selection button was pressed");
            self.status = None;
        }

        if self.busy() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.small(t!("tools.patch.status_running"));
            });
        } else if let Some(status) = self.status.as_ref() {
            ui.small(status.clone());
        }
        ui.small(t!("tools.patch.usage_hint"));
    }

    /// Begins a gesture at `scene_pos` on `page_idx`.
    ///
    /// A parked gesture end found here on ANOTHER page is the host's end+begin pair for a drag
    /// that crossed a page seam — never a release — and the gesture is dropped instead of being
    /// committed: a selection and the source it is dragged onto must live on one page, and a
    /// lasso truncated at the seam is not the shape the user is drawing. A parked end on the SAME
    /// page cannot come from a crossing, so it is a genuine release followed by a fresh press
    /// inside one frame and is resolved as such.
    pub fn stroke_begin(&mut self, host: &mut dyn PatchHost, page_idx: usize, scene_pos: Pos2) {
        if let Some(ended) = self.pending_end.take() {
            if ended.page_idx() == page_idx {
                self.pending_end = Some(ended);
                self.resolve_pending_end(true);
            } else {
                self.cancel_gesture(
                    &ended,
                    t!("tools.patch.status_page_changed").to_string(),
                    &format!(
                        "the pointer moved from page {} onto page {page_idx} while the button was held",
                        ended.page_idx()
                    ),
                );
                return;
            }
        }
        if self.space_pan_active {
            return;
        }
        if self.busy() {
            self.status = Some(t!("tools.patch.status_busy").to_string());
            return;
        }
        // The page-pixel space the selection lives in is the host's storage space, so it has to
        // exist before the first vertex is taken. A refusal only means the page is not laid out
        // yet, which the `scene_pos_to_page_pos` guard below reports as "no gesture".
        host.ensure_page_pixels(page_idx, scene_pos);
        let Some(pos) = host.scene_pos_to_page_pos(page_idx, scene_pos) else {
            return;
        };
        let gesture = gesture_for_press(self.selection.as_ref(), self.shape, page_idx, pos);
        if !matches!(gesture, PatchGesture::Source { .. }) {
            // A press outside the selection always starts a NEW one, which is also how a
            // selection on another page is replaced.
            self.selection = None;
        }
        self.gesture = Some(gesture);
    }

    /// Extends the live gesture to `scene_pos` on `page_idx`.
    pub fn stroke_update(&mut self, host: &dyn PatchHost, page_idx: usize, scene_pos: Pos2) {
        let Some(pos) = host.scene_pos_to_page_pos(page_idx, scene_pos) else {
            return;
        };
        match self.gesture.as_mut() {
            Some(PatchGesture::Lasso {
                page_idx: gesture_page,
                points,
            }) => {
                if *gesture_page != page_idx {
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
                    page_idx: gesture_page,
                    current,
                    ..
                }
                | PatchGesture::Source {
                    page_idx: gesture_page,
                    current,
                    ..
                },
            ) if *gesture_page == page_idx => *current = pos,
            Some(_) => {}
            None => {}
        }
    }

    /// Parks the finished gesture; NOTHING is committed here.
    ///
    /// A host calls this hook for a real pointer release AND as one half of the end+begin pair it
    /// issues when a drag crosses onto another page, and for a pointer that left the canvas
    /// rectangle with the button still held — but it hands the hook no pointer state to tell them
    /// apart. For an ordinary brush the difference is harmless ("end this page's stroke"); for
    /// this tool it would mean "commit", which is why the decision waits for `resolve_pending_end`
    /// and the frame's real pointer state.
    pub fn stroke_end(&mut self) {
        let Some(gesture) = self.gesture.take() else {
            return;
        };
        self.pending_end = Some(gesture);
    }

    /// Escape clears the selection and cancels the gesture and everything in flight.
    ///
    /// Answers `false` when there is nothing to clear, so the key stays available to whatever
    /// else wants it.
    pub fn on_escape(&mut self, ctx: &egui::Context) -> bool {
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

    /// Abandons everything in flight when the tool stops being the active one.
    ///
    /// The selection survives (Photoshop keeps it). Nothing else does: the live gesture has no
    /// meaning without the pointer that started it, and everything in flight would otherwise
    /// finish unpolled and land on a page the user has meanwhile changed.
    pub fn deactivate(&mut self, host: &mut dyn PatchHost) {
        self.gesture = None;
        self.space_pan_active = false;
        self.cancel_in_flight(&"the tool was deactivated");
        self.flush_host_cancel(host);
    }

    /// Drops what the host retained for a cancelled job, if a cancellation is parked.
    fn flush_host_cancel(&mut self, host: &mut dyn PatchHost) {
        if self.host_cancel_pending {
            self.host_cancel_pending = false;
            host.discard_region_load();
        }
    }

    /// The once-per-frame pass: resolve the parked end, drive the job, paint the outlines.
    ///
    /// `released` must be this frame's answer to "has the primary pointer button actually been
    /// released?". It is checked as `!primary_down || primary_released`, so a release followed by
    /// a fresh press inside one frame still counts as the release it was.
    pub fn draw_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        host: &mut dyn PatchHost,
        released: bool,
    ) {
        self.flush_host_cancel(host);
        self.resolve_pending_end(released);
        if let Some(offset) = self.pending_offset.take() {
            self.gesture = None;
            self.start_job(host, offset);
        }
        self.poll_region_load(host);
        self.poll_solve(host);
        self.paint(ctx, host);
        if self.busy() {
            // The GUI stays responsive and keeps repainting while the worker runs.
            ctx.request_repaint();
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
    /// A source drag `PatchToolCore::drag_geometry` refuses — no patch can be applied from here.
    Refused,
}

impl OutlineTone {
    /// The light half of the two-tone dash. The dark half is `ms_theme::canvas::OUTLINE_BACKING`
    /// either way, so the outline stays readable over both black line art and white paper in both
    /// states; a refused drag uses the canvas-wide `ms_theme::canvas::REFUSED`, the one meaning of
    /// "this state is refused" shared with the cleaning tab's region editor.
    fn light(self) -> Color32 {
        match self {
            Self::Normal => Color32::WHITE,
            Self::Refused => ms_theme::canvas::REFUSED,
        }
    }

    /// The tone a source drag's outline is painted in, read off the SAME answer the release
    /// acts on (`PatchToolCore::drag_geometry`, consumed by `start_job`).
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
/// `PatchHost::scene_pos_to_page_pos` is the same affine map inverted, and it CLAMPS, so its
/// forward twin would come back pinned to the border for a point outside the page — which for an
/// aiming outline would claim the source sits somewhere it does not. This is that map minus the
/// clamp. Positions outside the page therefore map outside the page's scene rect, as they should;
/// the painter's clip rect is what keeps them off the dock panels.
///
/// `None` when the page has no projection, a zero-sized pixel space or a degenerate page rect.
fn page_pos_to_scene_pos_unclamped(
    host: &dyn PatchHost,
    page_idx: usize,
    pos: [f32; 2],
) -> Option<Pos2> {
    let projection = host.page_projection(page_idx)?;
    scene_pos_in_page(projection.scene_rect, projection.pixel_size, pos)
}

/// The unclamped page-pixel -> scene map itself, without the host lookups.
///
/// Split from `page_pos_to_scene_pos_unclamped` because this is the part that must be pinned by
/// a test: a laid-out page cannot be built in a unit test, but the mapping can, and "a point
/// outside the page maps outside the page rect" is exactly the property the aiming outline
/// depends on. `None` for a zero-sized pixel space or a degenerate page rect.
fn scene_pos_in_page(
    page_scene_rect: egui::Rect,
    page_pixel_size: [usize; 2],
    pos: [f32; 2],
) -> Option<Pos2> {
    let [page_w, page_h] = page_pixel_size;
    if page_w == 0 || page_h == 0 || !page_scene_rect.is_positive() {
        return None;
    }
    Some(egui::pos2(
        page_scene_rect.left() + page_scene_rect.width() * (pos[0] / page_w as f32),
        page_scene_rect.top() + page_scene_rect.height() * (pos[1] / page_h as f32),
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
/// unusable. Nothing is painted when the page is not laid out, which is also when the page-pixel
/// space the polygon lives in cannot be projected onto the screen.
fn paint_outline(
    painter: &egui::Painter,
    host: &dyn PatchHost,
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
        let Some(pos) = page_pos_to_scene_pos_unclamped(host, page_idx, moved) else {
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
        egui::Stroke::new(3.0, ms_theme::canvas::OUTLINE_BACKING),
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

/// The opaque RGB triple of a pixel.
fn rgb_of(color: Color32) -> [u8; 3] {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    [r, g, b]
}

/// The whole worker-side pass: rasterize, solve, blend the solution into the destination.
///
/// Runs on a worker thread and touches no host. Errors come back as already-localized sentences,
/// because the GUI thread has nothing left to add to them.
///
/// It STOPS at `(rgb, coverage)`: how those pixels are stored is the host's decision, and the
/// backdrop such a store needs is loaded by the host, not by this pass.
fn run_patch_job(input: PatchJobInput) -> PatchOutcome {
    let roi = input.roi;
    let count = roi.w.saturating_mul(roi.h);
    // The ROI-sized input is checked here, once, before anything is computed. The per-pixel work
    // below therefore never has to decide what a length disagreement means — a decision it could
    // only make by skipping pixels or substituting values, i.e. by guessing over the user's work.
    if let Err(error) = validate_job_input(&input) {
        return refuse(input.tag, input.page_idx, roi, &error);
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
            ms_log::runtime_log::log_error(format!(
                "[{}] the membrane solve refused the job: {error}",
                input.tag
            ));
            return PatchOutcome {
                page_idx: input.page_idx,
                roi,
                solved: Err(t!("tools.patch.error_solve").to_string()),
            };
        }
    };

    if solved.rgb.len() != count || solved.coverage.len() != count {
        return refuse(
            input.tag,
            input.page_idx,
            roi,
            &PatchInputError::SolvedLength {
                rgb: solved.rgb.len(),
                coverage: solved.coverage.len(),
                want: count,
            },
        );
    }

    // The colour each ROI pixel must END UP showing: the membrane's answer faded into the
    // destination by its own coverage. A pixel with zero coverage keeps the destination exactly
    // (`lerp_rgb` clamps `t`), and the host skips it anyway — see `PatchCommit`.
    // All three buffers are exactly `count` long by the checks above, so the zip visits each ROI
    // pixel exactly once and no pixel can be skipped over a length disagreement.
    let rgb: Vec<[u8; 3]> = request
        .dst
        .iter()
        .zip(solved.rgb.iter())
        .zip(solved.coverage.iter())
        .map(|((current, solved_rgb), coverage)| lerp_rgb(*current, *solved_rgb, *coverage))
        .collect();

    PatchOutcome {
        page_idx: input.page_idx,
        roi,
        solved: Ok(PatchSolved {
            rgb,
            coverage: solved.coverage,
        }),
    }
}

/// Checks the worker's ROI-sized input against the ROI, once, before any work.
///
/// # Errors
/// [`PatchInputError::EmptyRoi`] for an ROI with no pixels, and
/// [`PatchInputError::BufferSize`] when the composite is not exactly the ROI's size.
fn validate_job_input(input: &PatchJobInput) -> Result<(), PatchInputError> {
    let roi = input.roi;
    let count = roi.w.saturating_mul(roi.h);
    if count == 0 {
        return Err(PatchInputError::EmptyRoi { w: roi.w, h: roi.h });
    }
    check_roi_buffer(
        PatchBuffer::Composite,
        input.composite.size,
        input.composite.pixels.len(),
        roi,
    )
}

/// Whether one ROI-sized image really is `roi.w` x `roi.h` with that many pixels.
///
/// Shared with hosts, which check the buffers of their own store step against the same ROI and
/// must refuse with the same vocabulary and the same localized sentences.
///
/// # Errors
/// [`PatchInputError::BufferSize`] naming `buffer`, its actual size and the expected one.
pub fn check_roi_buffer(
    buffer: PatchBuffer,
    size: [usize; 2],
    pixels: usize,
    roi: OverlayRectPx,
) -> Result<(), PatchInputError> {
    let count = roi.w.saturating_mul(roi.h);
    if size != [roi.w, roi.h] || pixels != count {
        return Err(PatchInputError::BufferSize {
            buffer,
            got_w: size[0],
            got_h: size[1],
            got_len: pixels,
            want_w: roi.w,
            want_h: roi.h,
            want_len: count,
        });
    }
    Ok(())
}

/// Turns a refused job into the outcome the GUI thread reports, with the numbers in the log.
fn refuse(
    tag: &'static str,
    page_idx: usize,
    roi: OverlayRectPx,
    error: &PatchInputError,
) -> PatchOutcome {
    ms_log::runtime_log::log_error(format!(
        "[{tag}] the job for page {page_idx} was refused, ROI {roi:?}: {error}"
    ));
    PatchOutcome {
        page_idx,
        roi,
        solved: Err(error.user_message()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One `commit_patch` call, recorded so a test can assert what crossed the seam.
    #[derive(Debug)]
    struct RecordedCommit {
        page_idx: usize,
        roi: OverlayRectPx,
        rgb: Vec<[u8; 3]>,
        coverage: Vec<f32>,
    }

    /// A host that records what the core asked of it and answers whatever the test set up.
    ///
    /// It is what pins the seam: the core hands it `(page, rect, rgb, coverage)` and nothing
    /// else, and everything about storage — whether it succeeds, what it writes — is decided
    /// here, on the host's side of the trait.
    #[derive(Default)]
    struct FakeHost {
        source_size: Option<[usize; 2]>,
        projection: Option<PagePixelProjection>,
        viewport: Option<egui::Rect>,
        page_pos: Option<(f32, f32)>,
        /// Every region load the core started, in order.
        started: Vec<PatchRegionRequest>,
        /// What the next `poll_region_load` answers with.
        region: Option<egui::ColorImage>,
        /// How many times the core polled the load, pending or not.
        polls: usize,
        /// What `commit_patch` answers with; `None` means it succeeds.
        commit_refusal: Option<String>,
        /// Every commit the core handed over, in order.
        committed: Vec<RecordedCommit>,
        discards: usize,
        ensured: Vec<(usize, Pos2)>,
    }

    impl PatchHost for FakeHost {
        fn page_source_size(&self, _page_idx: usize) -> Option<[usize; 2]> {
            self.source_size
        }
        fn page_projection(&self, _page_idx: usize) -> Option<PagePixelProjection> {
            self.projection
        }
        fn usable_viewport(&self) -> Option<egui::Rect> {
            self.viewport
        }
        fn scene_pos_to_page_pos(&self, _page_idx: usize, _scene_pos: Pos2) -> Option<(f32, f32)> {
            self.page_pos
        }
        fn ensure_page_pixels(&mut self, page_idx: usize, scene_pos: Pos2) {
            self.ensured.push((page_idx, scene_pos));
        }
        fn start_region_load(
            &mut self,
            request: &PatchRegionRequest,
        ) -> Result<(), PatchHostError> {
            self.started.push(*request);
            Ok(())
        }
        fn poll_region_load(&mut self) -> PatchRegionPoll {
            self.polls += 1;
            match self.region.take() {
                Some(image) => PatchRegionPoll::Ready(image),
                None => PatchRegionPoll::Pending,
            }
        }
        fn discard_region_load(&mut self) {
            self.discards += 1;
        }
        fn commit_patch(&mut self, patch: &PatchCommit<'_>) -> Result<(), String> {
            self.committed.push(RecordedCommit {
                page_idx: patch.page_idx,
                roi: patch.roi,
                rgb: patch.rgb.to_vec(),
                coverage: patch.coverage.to_vec(),
            });
            match self.commit_refusal.clone() {
                Some(message) => Err(message),
                None => Ok(()),
            }
        }
    }

    /// A core tagged the way the cleaning host tags it, so the log lines read as they do live.
    fn core() -> PatchToolCore {
        PatchToolCore::new("cleaning/patch", "cleaning_patch_outline")
    }

    /// An axis-aligned selection on page 0, given as `(x0, y0, x1, y1)` page pixels.
    fn selection(x0: f32, y0: f32, x1: f32, y1: f32) -> PatchSelection {
        PatchSelection {
            page_idx: 0,
            points: rect_corners((x0, y0), (x1, y1)),
        }
    }

    /// A press on `page_idx`. The scene position is irrelevant to every test here: the fake host
    /// answers `None` for the page position unless a test says otherwise, so the projected half
    /// of `stroke_begin` is a no-op exactly as it is against a canvas with no laid-out page.
    fn press(page_idx: usize) -> (usize, Pos2) {
        (page_idx, egui::pos2(0.0, 0.0))
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
        let mut tool = core();
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

        let mut tool = core();
        let mut handled_when_empty = true;
        // Headless pass: no renderer applies the font-atlas upload, and egui 0.36 panics on an
        // unapplied `TexturesDelta` drop, so every pass's deltas are discarded explicitly.
        ctx.run_ui(input.clone(), |ui| {
            handled_when_empty = tool.on_escape(ui.ctx());
        })
        .drop_without_applying_deltas();
        assert!(!handled_when_empty, "Escape must stay available when nothing is selected");

        tool.selection = Some(selection(10.0, 10.0, 40.0, 40.0));
        let mut handled = false;
        ctx.run_ui(input, |ui| {
            handled = tool.on_escape(ui.ctx());
        })
        .drop_without_applying_deltas();
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

        let mut tool = core();
        tool.selection = Some(selection(10.0, 10.0, 40.0, 40.0));
        tool.pending_offset = Some((5, 5));
        let (_solve_tx, solve_rx) = mpsc::channel::<PatchOutcome>();
        tool.solve_rx = Some(solve_rx);
        assert!(tool.busy(), "the fixture is a tool with a job in flight");

        let mut handled = false;
        // Headless pass: the unapplied font-atlas upload must be discarded explicitly (egui 0.36
        // panics when a `TexturesDelta` is dropped with pending deltas).
        ctx.run_ui(input, |ui| {
            handled = tool.on_escape(ui.ctx());
        })
        .drop_without_applying_deltas();
        assert!(handled, "Escape is consumed when it has something to cancel");
        assert!(tool.selection.is_none());
        assert!(!tool.busy(), "a job the user dismissed must not still be in flight");
        assert!(tool.pending_offset.is_none() && tool.solve_rx.is_none());
        assert!(tool.pending_load.is_none());
        assert!(
            tool.host_cancel_pending,
            "the host must be told to drop what it retained for the cancelled job"
        );

        // And the tool is immediately usable again: a fresh press is not bounced as busy.
        let mut host = FakeHost::default();
        let (page_idx, scene_pos) = press(0);
        tool.stroke_begin(&mut host, page_idx, scene_pos);
        assert!(tool.status.is_none(), "a fresh selection must not be refused after a cancel");
    }

    /// The «clear selection» button cancels the work in flight for the same reason.
    ///
    /// It and Escape both go through `clear_selection`, which is what makes the two entry points
    /// impossible to fix apart; a parked gesture end goes with them, so nothing can be committed
    /// after the selection it belongs to is gone.
    #[test]
    fn clearing_the_selection_cancels_the_work_in_flight() {
        let mut tool = core();
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
    /// `PatchHost::scene_pos_to_page_pos` CLAMPS, which for an aiming outline would claim the
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

        // The only host state that says "no page laid out" travels through the shared helper to
        // the outline exactly as it travels to `start_job`.
        let host = FakeHost::default();
        let answer =
            PatchToolCore::drag_geometry(&host, &selection(10.0, 10.0, 40.0, 40.0), (5, 5));
        assert!(matches!(answer, Err(DragRefusal::PageNotLaidOut { page_idx: 0 })));
        assert_eq!(OutlineTone::for_drag(&answer), OutlineTone::Refused);
    }

    /// The ROI is the selection's box unioned with its translated copy, padded on every side.
    #[test]
    fn the_roi_covers_both_boxes_with_padding() {
        let roi = PatchToolCore::roi_for((20.0, 30.0, 40.0, 50.0), (10, -10), 200, 200)
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
        assert!(PatchToolCore::roi_for((20.0, 20.0, 40.0, 40.0), (170, 0), 200, 200).is_none());
        // The source drag runs off the top edge.
        assert!(PatchToolCore::roi_for((20.0, 20.0, 40.0, 40.0), (0, -30), 200, 200).is_none());
        // No drag at all, but the selection itself sits flush against the page corner, so the
        // padding alone leaves it.
        assert!(PatchToolCore::roi_for((0.0, 0.0, 40.0, 40.0), (0, 0), 200, 200).is_none());
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

    /// A `stroke_end` that is not a real pointer release must NEITHER apply a patch NOR close a
    /// lasso.
    ///
    /// A host issues such an end whenever a drag crosses a page seam or leaves the canvas
    /// rectangle with the button still held; treating it as "the user let go" would commit the
    /// patch mid-gesture and truncate a selection the user is still drawing.
    #[test]
    fn a_stroke_end_with_the_button_still_held_neither_applies_nor_closes() {
        // A source drag: the parked end must not turn into a job.
        let mut tool = core();
        tool.selection = Some(selection(10.0, 10.0, 40.0, 40.0));
        tool.gesture = Some(PatchGesture::Source {
            page_idx: 0,
            start: (20.0, 20.0),
            current: (60.0, 60.0),
        });
        tool.stroke_end();
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
        tool.stroke_end();
        tool.resolve_pending_end(true);
        assert_eq!(tool.pending_offset, Some((40, 40)));

        // A lasso: the parked end must not close the polygon into a selection.
        let mut tool = core();
        tool.gesture = Some(PatchGesture::Lasso {
            page_idx: 0,
            points: rect_corners((5.0, 5.0), (50.0, 50.0)),
        });
        tool.stroke_end();
        tool.resolve_pending_end(false);
        assert!(tool.selection.is_none(), "a held button must not close a lasso");
        assert!(tool.gesture.is_none(), "the truncated shape is discarded, not kept");

        // Released for real, the same polygon does become the selection.
        tool.gesture = Some(PatchGesture::Lasso {
            page_idx: 0,
            points: rect_corners((5.0, 5.0), (50.0, 50.0)),
        });
        tool.stroke_end();
        tool.resolve_pending_end(true);
        assert!(tool.selection.is_some());
    }

    /// A gesture that crosses onto ANOTHER page is cancelled, never applied.
    ///
    /// A host ends and immediately re-begins the stroke at the seam. A selection and the source
    /// it is dragged onto must live on one page, so both gesture kinds are dropped there: the
    /// source drag without applying, the lasso without being truncated.
    #[test]
    fn a_page_change_cancels_the_gesture_instead_of_applying_it() {
        let mut host = FakeHost::default();

        // A source drag crossing the seam: no job, and the selection it started from survives.
        let mut tool = core();
        tool.selection = Some(selection(10.0, 10.0, 40.0, 40.0));
        tool.gesture = Some(PatchGesture::Source {
            page_idx: 0,
            start: (20.0, 20.0),
            current: (60.0, 60.0),
        });
        tool.stroke_end();
        let (page_idx, scene_pos) = press(1);
        tool.stroke_begin(&mut host, page_idx, scene_pos);
        assert!(tool.pending_offset.is_none(), "the patch must not be applied at the seam");
        assert!(tool.pending_end.is_none(), "and the parked end must not linger");
        assert!(!tool.busy());
        assert!(tool.gesture.is_none(), "no gesture is started on the page it crossed onto");
        let surviving = tool.selection.as_ref().expect("the completed selection survives");
        assert_eq!(surviving.page_idx, 0);
        assert!(tool.status.is_some(), "the cancellation is reported to the user");

        // A lasso crossing the seam: discarded, not closed into a truncated selection.
        let mut tool = core();
        tool.gesture = Some(PatchGesture::Lasso {
            page_idx: 0,
            points: rect_corners((5.0, 5.0), (50.0, 50.0)),
        });
        tool.stroke_end();
        let (page_idx, scene_pos) = press(1);
        tool.stroke_begin(&mut host, page_idx, scene_pos);
        assert!(tool.selection.is_none(), "a lasso must not be closed at the seam");
        assert!(tool.pending_end.is_none());
        assert!(tool.gesture.is_none());

        // A parked end on the SAME page cannot be a crossing — a host only ends and re-begins
        // when the page differs — so it is the release it looks like and still commits.
        let mut tool = core();
        tool.gesture = Some(PatchGesture::Lasso {
            page_idx: 0,
            points: rect_corners((5.0, 5.0), (50.0, 50.0)),
        });
        tool.stroke_end();
        let (page_idx, scene_pos) = press(0);
        tool.stroke_begin(&mut host, page_idx, scene_pos);
        assert!(tool.selection.is_some(), "a same-page press resolves the end as a release");
    }

    /// `deactivate` abandons everything in flight and tells the host to let go of it too.
    ///
    /// `draw_overlay_ui` runs for the ACTIVE tool only, so a load or a solve left running past a
    /// tool switch is never polled again — and would later be applied against the snapshot taken
    /// before the switch, over whatever the user did in between.
    #[test]
    fn deactivating_abandons_every_job_in_flight() {
        let mut host = FakeHost::default();
        let mut tool = core();
        tool.selection = Some(selection(10.0, 10.0, 40.0, 40.0));
        tool.pending_offset = Some((5, 5));
        tool.pending_end = Some(PatchGesture::Lasso {
            page_idx: 0,
            points: rect_corners((5.0, 5.0), (50.0, 50.0)),
        });
        tool.pending_load = Some(PendingLoad {
            page_idx: 0,
            roi: OverlayRectPx {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            },
            offset: (1, 1),
            polygon_roi: rect_corners((2.0, 2.0), (6.0, 6.0)),
            blend: PatchBlend::None,
            feather_px: 0,
        });
        let (_tx, rx) = mpsc::channel::<PatchOutcome>();
        tool.solve_rx = Some(rx);
        assert!(tool.busy(), "the fixture really is in flight");

        tool.deactivate(&mut host);

        assert!(!tool.busy(), "nothing may survive the tool switch");
        assert!(tool.pending_end.is_none());
        assert!(tool.pending_offset.is_none());
        assert!(tool.pending_load.is_none());
        assert!(tool.solve_rx.is_none());
        assert!(tool.gesture.is_none());
        assert!(tool.selection.is_some(), "the selection itself is kept, as Photoshop does");
        assert_eq!(host.discards, 1, "the host must drop what it retained for the job");
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
            tag: "cleaning/patch",
            page_idx: 0,
            roi,
            offset: (0, 0),
            polygon_roi: rect_corners((2.0, 2.0), (6.0, 6.0)),
            composite: egui::ColorImage::filled([8, 8], Color32::from_rgb(200, 200, 200)),
            blend: PatchBlend::None,
            feather_px: 0,
        }
    }

    /// A malformed input can never yield `Ok`, and therefore can never reach a host.
    ///
    /// `run_patch_job` is a pure function of its input — it is handed no host — so refusing here
    /// is what keeps a size disagreement from being handed on as if it were a patch. The
    /// host-side buffers of the store step are refused by the host, against the same ROI and
    /// with the same vocabulary (`check_roi_buffer`).
    #[test]
    fn a_malformed_worker_input_is_refused_and_never_yields_a_solution() {
        assert!(run_patch_job(job_input()).solved.is_ok(), "the fixture itself must solve");

        for (name, input) in [
            ("composited destination", {
                let mut input = job_input();
                input.composite = egui::ColorImage::filled([4, 4], Color32::WHITE);
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
            let Err(message) = outcome.solved else {
                panic!("a malformed {name} must be refused, not substituted");
            };
            assert!(!message.is_empty(), "the refusal carries a user-facing sentence");
        }
    }

    /// Every ROI-sized buffer is checked ONCE, up front, and the first disagreement is named —
    /// the composite by the worker, the host's own buffers by the host through the same helper.
    #[test]
    fn the_worker_names_the_buffer_that_disagrees_with_the_roi() {
        let mut input = job_input();
        input.composite = egui::ColorImage::filled([1, 1], Color32::WHITE);
        assert!(matches!(
            validate_job_input(&input),
            Err(PatchInputError::BufferSize {
                buffer: PatchBuffer::Composite,
                got_w: 1,
                got_h: 1,
                want_w: 8,
                want_h: 8,
                ..
            })
        ));
        assert!(validate_job_input(&job_input()).is_ok());

        // The same helper, the same ROI, a host-side buffer.
        let roi = job_input().roi;
        assert!(matches!(
            check_roi_buffer(PatchBuffer::OverlayChunk, [1, 1], 1, roi),
            Err(PatchInputError::BufferSize {
                buffer: PatchBuffer::OverlayChunk,
                ..
            })
        ));
        assert!(check_roi_buffer(PatchBuffer::Page, [8, 8], 64, roi).is_ok());
    }

    /// The core asks its host for exactly the ROI its own refusal predicate measured.
    ///
    /// The loader crops against `source_size`, so the request has to carry the size the ROI was
    /// bounds-checked with; a request built from a separately fetched page size could disagree.
    #[test]
    fn the_core_asks_the_host_for_the_roi_it_measured() {
        let mut host = FakeHost {
            source_size: Some([200, 200]),
            ..FakeHost::default()
        };
        let mut tool = core();
        tool.selection = Some(selection(20.0, 30.0, 40.0, 50.0));
        tool.start_job(&mut host, (10, -10));

        let want = PatchToolCore::roi_for((20.0, 30.0, 40.0, 50.0), (10, -10), 200, 200)
            .expect("the padded union fits");
        // `OverlayRectPx` has no `PartialEq`, so the rectangle is compared field by field.
        assert_eq!(host.started.len(), 1);
        let started = host.started[0];
        assert_eq!(started.page_idx, 0);
        assert_eq!(
            (started.roi.x, started.roi.y, started.roi.w, started.roi.h),
            (want.x, want.y, want.w, want.h)
        );
        assert_eq!(started.source_size, [200, 200]);
        assert!(tool.busy(), "the job is in flight once the host accepted the load");

        // A drag the predicate refuses starts no load at all.
        let mut host = FakeHost {
            source_size: Some([200, 200]),
            ..FakeHost::default()
        };
        let mut tool = core();
        tool.selection = Some(selection(20.0, 20.0, 40.0, 40.0));
        tool.start_job(&mut host, (170, 0));
        assert!(host.started.is_empty(), "a refused drag must not reach the host");
        assert!(!tool.busy());
    }

    /// The host is polled on EVERY frame pass, not only while a load is pending.
    ///
    /// A channel-backed host (the cleaning tab's loader thread) releases an abandoned job's answer
    /// — a decoded page region of several megabytes — on a poll and on nothing else, so skipping
    /// the poll while idle would hold that memory until the next job drained the channel. What the
    /// host answers while the core awaits nothing belongs to a job that is over and is dropped.
    #[test]
    fn the_host_is_polled_on_every_frame_so_an_abandoned_answer_is_released() {
        let mut host = FakeHost::default();
        let mut tool = core();
        tool.poll_region_load(&mut host);
        assert_eq!(host.polls, 1, "an idle frame still drains the host");

        // The answer to a job the user cancelled, arriving after the core stopped waiting.
        host.region = Some(egui::ColorImage::filled([4, 4], Color32::WHITE));
        tool.poll_region_load(&mut host);
        assert_eq!(host.polls, 2);
        assert!(
            host.region.is_none(),
            "the stale answer must be drained out of the host, not left sitting there"
        );
        assert!(!tool.busy(), "a stale answer must not spawn a solve");
    }

    /// The core hands the host the rect and the pixels it solved — and nothing else.
    ///
    /// This is the seam: everything about STORAGE happens on the far side of `commit_patch`, so
    /// what crosses it is exactly `(page, rect, rgb, coverage)`, all three buffers ROI-sized.
    #[test]
    fn the_core_hands_the_host_the_rect_and_pixels_it_solved() {
        let outcome = run_patch_job(job_input());
        let roi = job_input().roi;
        let solved = outcome.solved.expect("the fixture solves");
        assert_eq!(solved.rgb.len(), roi.w * roi.h);
        assert_eq!(solved.coverage.len(), roi.w * roi.h);

        let mut tool = core();
        let (tx, rx) = mpsc::channel::<PatchOutcome>();
        tx.send(PatchOutcome {
            page_idx: 3,
            roi,
            solved: Ok(solved),
        })
        .expect("the receiver is alive");
        tool.solve_rx = Some(rx);

        let mut host = FakeHost::default();
        tool.poll_solve(&mut host);

        let commit = host.committed.pop().expect("the host is handed the finished patch");
        assert_eq!(commit.page_idx, 3);
        // `OverlayRectPx` has no `PartialEq`, so the rectangle is compared field by field.
        assert_eq!(
            (commit.roi.x, commit.roi.y, commit.roi.w, commit.roi.h),
            (roi.x, roi.y, roi.w, roi.h)
        );
        assert_eq!(commit.rgb.len(), roi.w * roi.h);
        assert_eq!(commit.coverage.len(), commit.rgb.len());
        // The interior of the selection is fully claimed; the padded border is not claimed at all.
        assert!(
            commit.coverage.iter().any(|value| *value >= 1.0),
            "the selection interior is covered"
        );
        assert_eq!(commit.coverage[0], 0.0, "the padded ROI corner is outside the selection");
        assert!(tool.solve_rx.is_none(), "the outcome is consumed, not left in the channel");
        assert_eq!(host.discards, 1, "the finished job releases what the host retained");
    }

    /// The HOST decides storage: a host that refuses stores nothing and its sentence is what the
    /// user is shown.
    #[test]
    fn the_host_decides_whether_a_patch_is_stored() {
        let roi = job_input().roi;
        let solved = run_patch_job(job_input()).solved.expect("the fixture solves");

        let mut tool = core();
        let (tx, rx) = mpsc::channel::<PatchOutcome>();
        tx.send(PatchOutcome {
            page_idx: 0,
            roi,
            solved: Ok(solved),
        })
        .expect("the receiver is alive");
        tool.solve_rx = Some(rx);

        let mut host = FakeHost {
            commit_refusal: Some("the page changed under the solve".to_string()),
            ..FakeHost::default()
        };
        tool.poll_solve(&mut host);
        assert_eq!(
            tool.status.as_deref(),
            Some("the page changed under the solve"),
            "the host's own sentence reaches the user unchanged"
        );
        assert_eq!(host.discards, 1);
    }
}
