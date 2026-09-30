/*
File: crates/ms-tab-page-manager/src/viewer.rs

Purpose:
The page viewer of the page-manager tab: a non-modal, resizable `egui::Window` that shows ONE
page or ONE clean at full resolution, fitted to the window on open, with wheel zoom around the
cursor and drag panning. Opened by a double-click on a page card, a bound clean card or an
unassigned clean card.

Key structures:
- ViewerTarget: what the viewer shows (page / bound clean / unassigned clean), fixed per viewer.
- PageViewer: the open viewer — camera, the «поверх страницы» toggle, the clean's CPU tiles and
  textures, the request in flight, and the source page it painted this frame.
- ViewerCamera: the board's pan/zoom camera. Its own (not `PsViewport`) because its minimum zoom
  follows the fit zoom, so a very tall strip still fits a window at its minimum height.
- ViewerWorker: the tab's single clean decode/split thread (never the thumbnail FIFO), owned by
  `PageManagerTabState` and reused by every viewer.

Key functions:
- PageManagerTabState::open_viewer() / close_viewer() / draw_viewer(): entry point from the
  cards, closing, per-frame draw.
- PageManagerTabState::release_hidden_viewer_clean(): the app's every-frame hook that frees the
  clean's tiles and textures while the tab is not drawn.
- PageManagerTabState::viewer_source_page() / viewer_wants_nearest_source(): what the app's
  source-page residency window must keep for this tab (see `src/app.rs`).
- resolve_target(): the target re-resolved against this frame's links / inventory (unit-tested).
- over_page_available(), clean_request_due(), reply_is_current(), same_model_pixels(),
  nearest_sampling_for(), px_from_f32(), newest_job(), run_viewer_worker(): rules with unit
  tests.

Notes:
- PAGES are never decoded here: the app lends its resident `PageTexture` tiles (full-resolution
  RGBA kept in RAM) and the viewer draws them, re-uploading a GPU-evicted tile under the frame's
  upload budget exactly like the canvas does. That adapter is temporary (see
  `paint_source_page`).
- CLEANS are decoded (file) or taken from `CleanOverlaysModel` (an `Arc` clone under a short
  lock, never `take_delta`) and split into `egui_large_image::PreparedTiles` on the tab's viewer
  worker; the `Arc` is dropped there right after the split, because the model copy-on-writes any
  page still shared. The worker runs one job at a time and skips every queued job but the
  newest; a decode already running when it is superseded runs to completion (`image::open`
  cannot be interrupted) and its result is discarded. At most one clean is resident; closing or
  replacing the viewer, or leaving the tab, frees its CPU tiles and textures.
- Model pixels are keyed by the model's GLOBAL revision (the model has no per-page counter). A
  bump that left this page's `Arc` untouched (a `Weak` identity token, see `same_model_pixels`)
  only re-keys the entry; new pixels are re-prepared, and a re-prepared clean of the same size
  is swapped into the EXISTING `TiledTexture` and re-sent tile by tile into the same textures,
  so the old pixels keep drawing meanwhile (no blink). Only a size change builds new textures.
- The target is re-resolved every frame; a page index that no longer exists or a clean that
  went away (unlinked, bound, deleted) closes the viewer.
*/

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Weak};

use eframe::egui;
use egui_large_image::{Alpha, GridError, PORTABLE_TILE_SIDE, Placement, PreparedTiles, PxRect, SplitError, TiledTexture, UploadBudget, UploadScope};
use image::RgbaImage;

use ms_models::clean_assign::UnassignedClean;
use ms_models::page_view::{PageImageInfo, PageTexture, SourcePageLoadState};
use ms_project::ProjectData;
use ms_widgets::combo_popup_open;

use super::PageManagerTabState;
use super::clean_link::{CleanLinkProblem, CleanThumbSource, PageCleanLink};

/// Camera zoom per wheel notch is `WHEEL_ZOOM_BASE ^ WHEEL_ZOOM_STEP` (about ×1.16), the step of
/// the crop / split / stitch boards' `PsViewport`. Only the SIGN of the raw wheel delta is read
/// (its magnitude is unit-dependent, `egui-docs/03-input.md`).
const WHEEL_ZOOM_BASE: f32 = 1.0015;
const WHEEL_ZOOM_STEP: f32 = 100.0;
/// Default camera zoom limits (screen points per image pixel), `PsViewport`'s. The lower one is
/// only a default: [`ViewerCamera::fit`] lowers it below the fit zoom, so a fit is never clamped.
const BASE_MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 32.0;
/// Share of the board a fitted image fills.
const FIT_MARGIN: f32 = 0.97;
/// After a fit, the user may zoom out down to this fraction of the fit zoom (when that is below
/// [`BASE_MIN_ZOOM`]).
const FIT_MIN_ZOOM_RATIO: f32 = 0.5;
/// Per-frame upload allowance shared by the page tiles and the clean tiles: four 2048-px tiles
/// or 24 MiB, whichever comes first (the app's own source-page upload budget).
const UPLOAD_TILES_PER_FRAME: usize = 4;
const UPLOAD_BYTES_PER_FRAME: usize = 24 * 1024 * 1024;
/// Zoom (screen points per image pixel) from which pixels are sampled `NEAREST`, so individual
/// pixels stay crisp when the user zooms in to inspect them; below it they are smoothed.
const NEAREST_MIN_ZOOM: f32 = 2.0;
/// Explicit window id: the title changes with the target and the language.
const WINDOW_ID: &str = "page_manager_page_viewer";
/// Texture name prefix of the clean's tiles (debug label only).
const CLEAN_TEXTURE_NAME: &str = "pm-page-viewer-clean";
/// Full-texture UV rect.
const UV_FULL: egui::Rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

/// What a viewer shows. Fixed for the viewer's lifetime; opening another target replaces the
/// viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ViewerTarget {
    /// Source page `idx` (index into `ProjectData::pages`).
    Page(usize),
    /// The clean linked to page `idx` (OK or problem link).
    BoundClean(usize),
    /// The unassigned clean file with this name (the «Клин без страницы» section).
    Unassigned(OsString),
}

/// Where a clean's pixels come from on this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CleanSource {
    /// `CleanOverlaysModel::overlay_rgba(page_idx)` (unsaved edits included).
    Model { page_idx: usize },
    /// A clean file on disk (bound, problem or unassigned).
    File(PathBuf),
    /// The file's header could not be read; nothing is requested, the error is shown.
    Unreadable(String),
}

/// A target resolved against this frame's page list, clean links and clean inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedTarget {
    Page { page_idx: usize },
    Clean(ResolvedClean),
}

/// A resolved clean target.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedClean {
    /// The page it is bound to (`None` for an unassigned clean).
    page_idx: Option<usize>,
    /// Display name (the clean's file name).
    file_name: String,
    source: CleanSource,
    /// `[width, height]` when the link / probe knows it.
    size: Option<[u32; 2]>,
    /// The link is `Ok` (bound and loadable), the precondition of «поверх страницы».
    bound_ok: bool,
}

/// Identity + version of the clean pixels a request was made for. A shown, in-flight or failed
/// entry answers for exactly one key; a different key means the pixels must be (re)prepared
/// (or, for model pixels whose `Arc` is unchanged, only re-keyed).
#[derive(Debug, Clone, PartialEq, Eq)]
enum CleanKey {
    /// Model pixels of `page_idx` at model `revision` (the model's revision is global).
    Model { page_idx: usize, revision: u64 },
    /// A file, by path (a bind / unlink renames files, which changes the key).
    File(PathBuf),
}

/// Why the viewer cannot show the clean.
#[derive(Debug)]
enum ViewerFailure {
    /// The worker could not decode or split the image.
    Load(CleanLoadError),
    /// The tile grid could not be built on the GUI thread.
    Texture(GridError),
    /// The model page is materialized but holds no pixels.
    NoPixels,
    /// No clean-overlays model is wired (or its lock is poisoned).
    ModelUnavailable,
    /// The worker thread could not be started or stopped unexpectedly.
    WorkerUnavailable(String),
}

/// Worker-side failure of preparing a clean for display.
#[derive(Debug, thiserror::Error)]
enum CleanLoadError {
    /// `image::open` failed (missing file, unsupported or corrupt data).
    #[error("could not decode {}: {message}", path.display())]
    Decode { path: PathBuf, message: String },
    /// The image dimensions do not fit `usize` on this platform.
    #[error("image of {width}x{height} px is too large for this platform")]
    TooLarge { width: u32, height: u32 },
    /// The tile split rejected the buffer (see [`SplitError`]).
    #[error(transparent)]
    Split(#[from] SplitError),
}

/// The pixels a worker job starts from.
enum JobSource {
    /// A clone of the model's page `Arc`; dropped by the worker right after the split.
    Model(Arc<RgbaImage>),
    /// A file to decode.
    File(PathBuf),
}

/// One request to the viewer worker.
struct ViewerJob {
    epoch: u64,
    source: JobSource,
    /// Woken when the reply is sent (the worker cannot otherwise wake the GUI).
    ctx: egui::Context,
}

/// The worker's answer to the job of `epoch`.
struct ViewerReply {
    epoch: u64,
    result: Result<PreparedTiles, CleanLoadError>,
}

/// The page-manager tab's clean decode/split thread, shared by every viewer the tab opens.
///
/// It runs ONE job at a time; before starting one it drops every queued job but the newest,
/// and it neither starts nor answers a job whose epoch is no longer [`Self::epoch`]'s current
/// value. A decode already running when it is superseded runs to completion and is discarded,
/// so at most one clean decode runs per tab at any moment. The thread ends when this handle is
/// dropped (the job channel closes).
pub(crate) struct ViewerWorker {
    jobs: Sender<ViewerJob>,
    replies: Receiver<ViewerReply>,
    /// Epoch of the newest job; the worker drops work of any other epoch.
    epoch: Arc<AtomicU64>,
}

impl ViewerWorker {
    /// Starts the worker thread.
    ///
    /// # Errors
    /// The OS error when the thread cannot be created.
    fn spawn() -> std::io::Result<Self> {
        let (job_tx, job_rx) = mpsc::channel::<ViewerJob>();
        let (reply_tx, reply_rx) = mpsc::channel::<ViewerReply>();
        let epoch = Arc::new(AtomicU64::new(0));
        let worker_epoch = Arc::clone(&epoch);
        let handle = ms_thread::Builder::new().name("pm-page-viewer".to_owned()).spawn(move || run_viewer_worker(&job_rx, &reply_tx, &worker_epoch))?;
        // Detached on purpose: joining when the tab is dropped would block the GUI thread for the
        // rest of a multi-second decode. Dropping `jobs` ends the thread's loop after the job it
        // is running, whose result is then discarded (see `Drop`).
        drop(handle);
        ms_log::runtime_log::log_info("[page-manager::viewer] viewer worker started");
        Ok(Self { jobs: job_tx, replies: reply_rx, epoch })
    }

    /// Queues `source` as the newest job and returns its epoch, or `None` when the worker thread
    /// is gone (its receiver was dropped).
    fn submit(&self, source: JobSource, ctx: &egui::Context) -> Option<u64> {
        let epoch = self.epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        self.jobs.send(ViewerJob { epoch, source, ctx: ctx.clone() }).ok().map(|()| epoch)
    }

    /// Supersedes every queued and running job (none of them will be answered) and drops the
    /// replies already sent, releasing their tiles now instead of when the next viewer polls.
    ///
    /// A job whose last epoch check passed just before the bump can still send its reply after
    /// this returns; such a late reply is stale and is filtered by the viewer's epoch check or
    /// dropped by [`Self::drop_replies`].
    fn cancel(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.drop_replies();
    }

    /// Drops every reply already sent (used while no viewer can accept one).
    fn drop_replies(&self) {
        while self.replies.try_recv().is_ok() {}
    }
}

impl Drop for ViewerWorker {
    fn drop(&mut self) {
        // Invalidates the job in progress so its result is not even sent; the closed job channel
        // then ends the worker loop.
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }
}

/// `first` or the newest job queued behind it, whichever came last. The older jobs are dropped
/// here, which also releases their model `Arc`s right away.
fn newest_job(first: ViewerJob, jobs: &Receiver<ViewerJob>) -> ViewerJob {
    let mut job = first;
    while let Ok(newer) = jobs.try_recv() {
        job = newer;
    }
    job
}

/// Worker loop: takes the newest queued job, prepares it if still current, and replies if it is
/// still current afterwards. Returns when the job channel is closed and drained, or when the
/// reply channel is closed.
fn run_viewer_worker(jobs: &Receiver<ViewerJob>, replies: &Sender<ViewerReply>, epoch: &AtomicU64) {
    while let Ok(first) = jobs.recv() {
        let ViewerJob { epoch: job_epoch, source, ctx } = newest_job(first, jobs);
        if job_epoch != epoch.load(Ordering::Acquire) {
            continue;
        }
        let result = prepare_clean(source);
        if let Err(error) = &result {
            ms_log::runtime_log::log_warn(format!("[page-manager::viewer] could not prepare the clean for display: {error}"));
        }
        if job_epoch != epoch.load(Ordering::Acquire) {
            continue;
        }
        if replies.send(ViewerReply { epoch: job_epoch, result }).is_err() {
            // The tab is gone; nothing is left to prepare for.
            return;
        }
        ctx.request_repaint();
    }
}

/// Decodes (file) or takes (model) the clean and splits it into portable tiles. The model `Arc`
/// is dropped when this returns.
fn prepare_clean(source: JobSource) -> Result<PreparedTiles, CleanLoadError> {
    match source {
        JobSource::Model(rgba) => split_rgba(&rgba),
        JobSource::File(path) => {
            let image = image::open(&path).map_err(|error| CleanLoadError::Decode { path: path.clone(), message: error.to_string() })?.to_rgba8();
            split_rgba(&image)
        }
    }
}

/// Splits an unmultiplied RGBA image into [`PORTABLE_TILE_SIDE`] tiles.
fn split_rgba(image: &RgbaImage) -> Result<PreparedTiles, CleanLoadError> {
    let (width, height) = image.dimensions();
    let too_large = || CleanLoadError::TooLarge { width, height };
    let size = [usize::try_from(width).map_err(|_| too_large())?, usize::try_from(height).map_err(|_| too_large())?];
    Ok(PreparedTiles::from_rgba(image.as_raw(), size, Alpha::Unmultiplied, PORTABLE_TILE_SIDE)?)
}

/// Whether `token` (taken from the model page `Arc` a request was made from) still names the
/// model's `current` `Arc`, i.e. the page's pixels did not change since.
///
/// Sound because a live `Weak` (a) keeps the allocation, so its address is never reused, and
/// (b) makes `Arc::make_mut` on a uniquely owned `Arc` MOVE the value into a new allocation
/// instead of mutating it in place; a shared `Arc` is cloned by `make_mut` and a replaced page is
/// a new `Arc`. Every pixel change of a model page therefore yields a different `Arc`. (The model
/// mutates its RGBA only through `make_mut` or replacement; `Arc::get_mut` fails while a `Weak`
/// exists.)
fn same_model_pixels(token: &Weak<RgbaImage>, current: &Arc<RgbaImage>) -> bool {
    token.upgrade().is_some_and(|prepared| Arc::ptr_eq(&prepared, current))
}

/// The board's pan/zoom camera. World coordinates are image pixels; `zoom` is screen points per
/// image pixel, kept within `[min_zoom, MAX_ZOOM]`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ViewerCamera {
    zoom: f32,
    /// Image point shown at the board's centre.
    center_px: egui::Vec2,
    /// Lower zoom limit: [`BASE_MIN_ZOOM`], lowered by [`Self::fit`] for images that need it.
    min_zoom: f32,
}

impl Default for ViewerCamera {
    fn default() -> Self {
        Self { zoom: 1.0, center_px: egui::Vec2::ZERO, min_zoom: BASE_MIN_ZOOM }
    }
}

impl ViewerCamera {
    /// Current zoom (screen points per image pixel).
    fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Fits an image of `image_size` px into `board`, centred. The minimum zoom is lowered to a
    /// fraction of the fit zoom when needed, so the fit itself is never clamped. No-op for a
    /// degenerate board (no finite, positive fit zoom).
    fn fit(&mut self, board: egui::Rect, image_size: [usize; 2]) {
        let [width, height] = image_size.map(|side| px_to_f32(side.max(1)));
        let fit = (board.width() / width).min(board.height() / height) * FIT_MARGIN;
        if !(fit.is_finite() && fit > 0.0) {
            return;
        }
        self.min_zoom = BASE_MIN_ZOOM.min(fit * FIT_MIN_ZOOM_RATIO);
        self.zoom = fit.clamp(self.min_zoom, MAX_ZOOM);
        self.center_px = egui::vec2(width * 0.5, height * 0.5);
    }

    /// Screen position of image pixel `(0, 0)` on `board`.
    fn origin(&self, board: egui::Rect) -> egui::Pos2 {
        board.center() - self.center_px * self.zoom
    }

    /// Image point under `screen` on `board`.
    fn screen_to_px(&self, board: egui::Rect, screen: egui::Pos2) -> egui::Vec2 {
        (screen - board.center()) / self.zoom + self.center_px
    }

    /// Zooms by `notches` wheel notches (positive: in), keeping the image point under `anchor`
    /// fixed on screen.
    fn zoom_by(&mut self, board: egui::Rect, anchor: egui::Pos2, notches: f32) {
        let before = self.screen_to_px(board, anchor);
        self.zoom = (self.zoom * WHEEL_ZOOM_BASE.powf(WHEEL_ZOOM_STEP * notches)).clamp(self.min_zoom, MAX_ZOOM);
        let after = self.screen_to_px(board, anchor);
        self.center_px += before - after;
    }

    /// Moves the image by `delta` screen points.
    fn pan(&mut self, delta: egui::Vec2) {
        if self.zoom > f32::EPSILON {
            self.center_px -= delta / self.zoom;
        }
    }
}

/// An image side in pixels as `f32` for camera math. Above 2^24 px the value rounds to a
/// neighbouring float, a sub-pixel error that no fit or zoom can show.
fn px_to_f32(side: usize) -> f32 {
    // Rounding only (never truncation of magnitude): usize -> f32 cannot overflow.
    side as f32
}

/// A prepared clean on screen: the CPU tiles (the upload source, also after `set_options` and
/// `mark_all_dirty`) and their textures.
struct ShownClean {
    key: CleanKey,
    prepared: PreparedTiles,
    texture: TiledTexture,
    /// Identity token of the model page `Arc` these tiles were split from (`None` for a file).
    model_pixels: Option<Weak<RgbaImage>>,
}

/// The request whose reply is awaited.
struct CleanRequest {
    epoch: u64,
    key: CleanKey,
    /// Identity token of the model page `Arc` sent with the job (`None` for a file).
    model_pixels: Option<Weak<RgbaImage>>,
}

/// The open viewer.
pub(crate) struct PageViewer {
    target: ViewerTarget,
    camera: ViewerCamera,
    /// Fit the whole image into the board on the next frame that knows its size.
    fit_pending: bool,
    /// «Поверх страницы»: draw the clean over its page instead of over the checkerboard. Only
    /// effective while [`over_page_available`] holds.
    over_page: bool,
    in_flight: Option<CleanRequest>,
    shown: Option<ShownClean>,
    /// The last failure and the key it answers for (not retried until the key changes).
    failed: Option<(Option<CleanKey>, ViewerFailure)>,
    /// Source page painted by the last drawn frame, and whether with `NEAREST` tiles.
    source_page: Option<usize>,
    source_nearest: bool,
    /// `(page_idx, tile_idx)` of malformed source tiles already logged (logged once, not per frame).
    malformed_tiles_logged: HashSet<(usize, usize)>,
}

impl PageViewer {
    /// A viewer for `target`, fitted on its first frame.
    fn new(target: ViewerTarget) -> Self {
        Self {
            target,
            camera: ViewerCamera::default(),
            fit_pending: true,
            over_page: false,
            in_flight: None,
            shown: None,
            failed: None,
            source_page: None,
            source_nearest: false,
            malformed_tiles_logged: HashSet::new(),
        }
    }

    /// Applies the tab worker's finished replies; only the one for the request in flight is
    /// installed. A dead worker is removed from `worker` (the next request starts a new one).
    fn poll_replies(&mut self, worker: &mut Option<ViewerWorker>) {
        loop {
            let Some(live) = worker.as_ref() else { return };
            match live.replies.try_recv() {
                Ok(reply) => self.apply_reply(reply),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    ms_log::runtime_log::log_error("[page-manager::viewer] the viewer worker stopped unexpectedly");
                    *worker = None;
                    if let Some(request) = self.in_flight.take() {
                        self.failed = Some((Some(request.key), ViewerFailure::WorkerUnavailable(t!("page_manager.viewer.worker_stopped_detail").to_string())));
                    }
                    return;
                }
            }
        }
    }

    /// Installs a reply when it answers the request in flight; stale replies are dropped. New
    /// pixels of the same tile grid are swapped into the existing texture (re-sent in place, the
    /// old texels keep drawing until then); a new grid gets a new texture set.
    fn apply_reply(&mut self, reply: ViewerReply) {
        if !reply_is_current(reply.epoch, self.in_flight.as_ref().map(|request| request.epoch)) {
            return;
        }
        let Some(request) = self.in_flight.take() else { return };
        match reply.result {
            Ok(prepared) => {
                if let Some(shown) = self.shown.as_mut()
                    && shown.texture.grid() == prepared.grid()
                {
                    // Same size: keep the textures. Every tile becomes pending and `upload` re-sends
                    // it into its existing `TextureId` under the frame budget.
                    shown.prepared = prepared;
                    shown.key = request.key;
                    shown.model_pixels = request.model_pixels;
                    shown.texture.mark_all_dirty();
                    self.failed = None;
                    return;
                }
                let options = clean_texture_options(self.camera.zoom());
                match TiledTexture::new(*prepared.grid(), CLEAN_TEXTURE_NAME, options) {
                    // Replacing the old entry (another size) drops its tiles and frees its textures.
                    Ok(texture) => {
                        self.shown = Some(ShownClean { key: request.key, prepared, texture, model_pixels: request.model_pixels });
                        self.failed = None;
                    }
                    Err(error) => {
                        ms_log::runtime_log::log_warn(format!("[page-manager::viewer] clean tile grid rejected: {error}"));
                        self.shown = None;
                        self.failed = Some((Some(request.key), ViewerFailure::Texture(error)));
                    }
                }
            }
            Err(error) => {
                // Stale pixels are not kept under an error: the user would read them as current.
                self.shown = None;
                self.failed = Some((Some(request.key), ViewerFailure::Load(error)));
            }
        }
    }

    /// Queues `source` for `key` on the tab's (lazily started) worker. `model_pixels` is the
    /// identity token of a model source.
    fn request(&mut self, worker: &mut Option<ViewerWorker>, ctx: &egui::Context, key: CleanKey, source: JobSource, model_pixels: Option<Weak<RgbaImage>>) {
        if worker.is_none() {
            match ViewerWorker::spawn() {
                Ok(started) => *worker = Some(started),
                Err(error) => {
                    ms_log::runtime_log::log_error(format!("[page-manager::viewer] could not start the viewer worker thread: {error}"));
                    self.failed = Some((Some(key), ViewerFailure::WorkerUnavailable(error.to_string())));
                    return;
                }
            }
        }
        let Some(epoch) = worker.as_ref().and_then(|live| live.submit(source, ctx)) else {
            ms_log::runtime_log::log_error("[page-manager::viewer] the viewer worker is gone; clean request dropped");
            *worker = None;
            self.failed = Some((Some(key), ViewerFailure::WorkerUnavailable(t!("page_manager.viewer.worker_stopped_detail").to_string())));
            return;
        };
        self.in_flight = Some(CleanRequest { epoch, key, model_pixels });
    }
}

/// Per-frame state of the clean, for the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanStatus {
    /// Tiles are shown (possibly stale while a refresh is in flight).
    Shown,
    /// Nothing to show yet; a request is in flight.
    Loading,
    /// Nothing can be shown; the message comes from the failure / the unreadable source.
    Failed,
}

impl PageManagerTabState {
    /// Opens the viewer on `target`, replacing any open viewer (its tiles and textures are freed
    /// and its pending request is superseded).
    pub(super) fn open_viewer(&mut self, target: ViewerTarget) {
        ms_log::runtime_log::log_info(format!("[page-manager::viewer] open {target:?}"));
        self.cancel_viewer_work();
        self.viewer = Some(PageViewer::new(target));
    }

    /// Closes the viewer, if open: frees its tiles and textures and supersedes its request.
    pub(super) fn close_viewer(&mut self) {
        self.viewer = None;
        self.cancel_viewer_work();
    }

    /// Supersedes whatever the tab's viewer worker is doing and drops its unread replies.
    fn cancel_viewer_work(&self) {
        if let Some(worker) = self.viewer_worker.as_ref() {
            worker.cancel();
        }
    }

    /// Frees the open viewer's clean (CPU tiles, textures, pending request) while the tab is not
    /// drawn; the clean is prepared again when the viewer is drawn next. The app calls this every
    /// frame, whichever tab is active, before [`Self::draw`]; a tab not drawn on the previous
    /// frame counts as hidden. The camera, toggle and target are kept. While no drawn viewer can
    /// take a reply (tab hidden, or no viewer open), late worker replies are dropped here too.
    pub fn release_hidden_viewer_clean(&mut self, ctx: &egui::Context) {
        let frame_nr = ctx.cumulative_frame_nr();
        // `draw` stamps `last_drawn_frame`; drawn on the previous frame (or already on this one)
        // means the tab is active.
        let hidden = !self.last_drawn_frame.is_some_and(|last| frame_nr <= last.saturating_add(1));
        if (hidden || self.viewer.is_none())
            && let Some(worker) = self.viewer_worker.as_ref()
        {
            worker.drop_replies();
        }
        if !hidden {
            return;
        }
        let Some(viewer) = self.viewer.as_mut() else { return };
        if viewer.shown.is_none() && viewer.in_flight.is_none() {
            return;
        }
        viewer.shown = None;
        viewer.in_flight = None;
        self.cancel_viewer_work();
        ms_log::runtime_log::log_info("[page-manager::viewer] tab hidden; the viewer's clean tiles and textures were released");
    }

    /// Source page the viewer painted on the last drawn frame (the page itself, or the page under
    /// a clean shown «поверх страницы»). The app keeps that page's GPU tiles in its source-page
    /// residency window while this tab is active, so they are not trimmed and re-uploaded every
    /// frame.
    #[must_use]
    pub fn viewer_source_page(&self) -> Option<usize> {
        self.viewer.as_ref().and_then(|viewer| viewer.source_page)
    }

    /// Whether the viewer painted its source page with `NEAREST` tiles on the last drawn frame
    /// (zoomed in past [`NEAREST_MIN_ZOOM`]). The app treats that like a canvas' pixel
    /// inspection, which keeps the page's nearest-filtered textures resident.
    #[must_use]
    pub fn viewer_wants_nearest_source(&self) -> bool {
        self.viewer.as_ref().is_some_and(|viewer| viewer.source_page.is_some() && viewer.source_nearest)
    }

    /// Draws the open viewer (if any) and closes it when its target went away or the user closed
    /// the window. `textures` is the app's resident source-page tiles, lent for this frame.
    pub(super) fn draw_viewer(&mut self, ctx: &egui::Context, project: &ProjectData, page_infos: &HashMap<usize, PageImageInfo>, textures: &mut HashMap<usize, PageTexture>) {
        let Some(mut viewer) = self.viewer.take() else { return };
        let Some(resolved) = resolve_target(&viewer.target, project.pages.len(), &self.clean_links, self.unassigned_cleans()) else {
            // The page was removed, or the clean was unlinked / bound / deleted meanwhile.
            ms_log::runtime_log::log_info(format!("[page-manager::viewer] target {:?} is gone; viewer closed", viewer.target));
            self.close_viewer();
            return;
        };
        let clean_status = match &resolved {
            ResolvedTarget::Page { .. } => None,
            ResolvedTarget::Clean(clean) => Some(self.update_viewer_clean(ctx, &mut viewer, clean)),
        };
        let title = viewer_title(&resolved, project);
        let mut keep_open = true;
        egui::Window::new(title)
            .id(egui::Id::new(WINDOW_ID))
            .open(&mut keep_open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(900.0, 760.0))
            .min_width(420.0)
            .min_height(320.0)
            .show(ctx, |ui| {
                egui::Panel::top("page_manager_viewer_toolbar").show(ui, |ui| {
                    draw_viewer_toolbar(ui, &mut viewer, &resolved, page_infos);
                });
                egui::CentralPanel::default().show(ui, |ui| {
                    draw_viewer_board(ui, &mut viewer, &resolved, clean_status, page_infos, textures);
                });
            });
        if keep_open {
            self.viewer = Some(viewer);
        } else {
            self.close_viewer();
        }
    }

    /// Advances the clean's load for this frame: applies worker replies, and requests the clean's
    /// current pixels when neither the shown, the in-flight nor the failed entry answers for them.
    fn update_viewer_clean(&mut self, ctx: &egui::Context, viewer: &mut PageViewer, clean: &ResolvedClean) -> CleanStatus {
        viewer.poll_replies(&mut self.viewer_worker);
        let wanted = match &clean.source {
            CleanSource::Unreadable(_) => {
                viewer.shown = None;
                return CleanStatus::Failed;
            }
            CleanSource::File(path) => CleanKey::File(path.clone()),
            CleanSource::Model { page_idx } => match self.overlays_revision_seen {
                Some(revision) => CleanKey::Model { page_idx: *page_idx, revision },
                None => {
                    viewer.failed = Some((None, ViewerFailure::ModelUnavailable));
                    return CleanStatus::Failed;
                }
            },
        };
        let due = clean_request_due(
            &wanted,
            viewer.shown.as_ref().map(|shown| &shown.key),
            viewer.in_flight.as_ref().map(|request| &request.key),
            viewer.failed.as_ref().and_then(|(key, _)| key.as_ref()),
        );
        if due {
            match &clean.source {
                CleanSource::File(path) => viewer.request(&mut self.viewer_worker, ctx, wanted, JobSource::File(path.clone()), None),
                CleanSource::Model { page_idx } => self.request_model_clean(ctx, viewer, *page_idx),
                CleanSource::Unreadable(_) => {}
            }
        }
        if viewer.shown.is_some() {
            CleanStatus::Shown
        } else if viewer.in_flight.is_some() {
            CleanStatus::Loading
        } else {
            CleanStatus::Failed
        }
    }

    /// Reads page `page_idx`'s model pixels under one short lock (revision + an `Arc` clone).
    /// When the `Arc` is the one already shown or in flight, that entry is only re-keyed to the
    /// new (global) revision; otherwise the pixels are queued for preparation.
    fn request_model_clean(&mut self, ctx: &egui::Context, viewer: &mut PageViewer, page_idx: usize) {
        let Some(model) = self.overlays_model.as_ref() else {
            viewer.failed = Some((None, ViewerFailure::ModelUnavailable));
            return;
        };
        let (revision, rgba) = match model.lock() {
            Ok(guard) => (guard.revision(), guard.overlay_rgba(page_idx)),
            Err(_) => {
                ms_log::runtime_log::log_warn("[page-manager::viewer] clean overlays model lock poisoned; clean not shown");
                viewer.failed = Some((None, ViewerFailure::ModelUnavailable));
                return;
            }
        };
        let key = CleanKey::Model { page_idx, revision };
        let Some(rgba) = rgba else {
            viewer.shown = None;
            viewer.failed = Some((Some(key), ViewerFailure::NoPixels));
            return;
        };
        let unchanged = |token: Option<&Weak<RgbaImage>>| token.is_some_and(|token| same_model_pixels(token, &rgba));
        if let Some(shown) = viewer.shown.as_mut()
            && unchanged(shown.model_pixels.as_ref())
        {
            // The bump came from another page (or a visibility change): what is shown is current,
            // so any request in flight is obsolete.
            shown.key = key;
            viewer.in_flight = None;
            return;
        }
        if let Some(request) = viewer.in_flight.as_mut()
            && unchanged(request.model_pixels.as_ref())
        {
            request.key = key;
            return;
        }
        let token = Arc::downgrade(&rgba);
        viewer.request(&mut self.viewer_worker, ctx, key, JobSource::Model(rgba), Some(token));
    }
}

/// Resolves `target` against this frame's state; `None` when it no longer exists (page index
/// out of range, bound clean unlinked, unassigned clean bound / deleted / renamed).
fn resolve_target(target: &ViewerTarget, page_count: usize, links: &[PageCleanLink], unassigned: &[UnassignedClean]) -> Option<ResolvedTarget> {
    match target {
        ViewerTarget::Page(page_idx) => (*page_idx < page_count).then_some(ResolvedTarget::Page { page_idx: *page_idx }),
        ViewerTarget::BoundClean(page_idx) => {
            if *page_idx >= page_count {
                return None;
            }
            let resolved = match links.get(*page_idx)? {
                PageCleanLink::None => return None,
                PageCleanLink::Ok { thumb, file_name, size } => {
                    let source = match thumb {
                        CleanThumbSource::Model => CleanSource::Model { page_idx: *page_idx },
                        CleanThumbSource::File(path) => CleanSource::File(path.clone()),
                    };
                    ResolvedClean { page_idx: Some(*page_idx), file_name: file_name.clone(), source, size: *size, bound_ok: true }
                }
                PageCleanLink::Problem { problem, file, size } => {
                    let source = match problem {
                        CleanLinkProblem::CleanUnreadable(error) => CleanSource::Unreadable(error.clone()),
                        // The clean itself is readable; only its fit to the page is the problem.
                        CleanLinkProblem::SizeMismatch { .. } | CleanLinkProblem::PageUnreadable(_) => CleanSource::File(file.clone()),
                    };
                    let file_name = file.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                    ResolvedClean { page_idx: Some(*page_idx), file_name, source, size: *size, bound_ok: false }
                }
            };
            Some(ResolvedTarget::Clean(resolved))
        }
        ViewerTarget::Unassigned(file_name) => {
            let probe = unassigned.iter().find(|item| &item.file_name == file_name)?.effective()?;
            let (source, size) = match &probe.size {
                Ok(size) => (CleanSource::File(probe.path.clone()), Some(*size)),
                Err(error) => (CleanSource::Unreadable(error.clone()), None),
            };
            Some(ResolvedTarget::Clean(ResolvedClean { page_idx: None, file_name: file_name.to_string_lossy().into_owned(), source, size, bound_ok: false }))
        }
    }
}

/// «Поверх страницы» is offered only for a clean with an OK link whose known size equals its
/// page's known, non-empty size: anything else has no valid base to composite over.
fn over_page_available(bound_ok: bool, clean_size: Option<[u32; 2]>, page_size: Option<[u32; 2]>) -> bool {
    match (clean_size, page_size) {
        (Some(clean), Some(page)) => bound_ok && clean == page && page[0] > 0 && page[1] > 0,
        _ => false,
    }
}

/// Whether `wanted` must be requested: no shown, in-flight or failed entry answers for it.
fn clean_request_due(wanted: &CleanKey, shown: Option<&CleanKey>, in_flight: Option<&CleanKey>, failed: Option<&CleanKey>) -> bool {
    shown != Some(wanted) && in_flight != Some(wanted) && failed != Some(wanted)
}

/// Whether a reply of `reply_epoch` answers the request in flight (`None`: nothing awaited).
fn reply_is_current(reply_epoch: u64, in_flight_epoch: Option<u64>) -> bool {
    in_flight_epoch == Some(reply_epoch)
}

/// `NEAREST` sampling from [`NEAREST_MIN_ZOOM`] on, `LINEAR` below.
fn nearest_sampling_for(zoom: f32) -> bool {
    zoom >= NEAREST_MIN_ZOOM
}

/// Texture options of the clean's tiles at `zoom`.
fn clean_texture_options(zoom: f32) -> egui::TextureOptions {
    clean_texture_options_for(nearest_sampling_for(zoom))
}

/// `NEAREST` or `LINEAR` texture options.
fn clean_texture_options_for(nearest: bool) -> egui::TextureOptions {
    if nearest { egui::TextureOptions::NEAREST } else { egui::TextureOptions::LINEAR }
}

/// A tile coordinate that `PageTexture` stores as `f32`, back as whole pixels. `None` unless it is
/// a finite, non-negative whole number below 2^24 (where every integer is exact in `f32`).
fn px_from_f32(value: f32) -> Option<usize> {
    /// 2^24: the first integer above which `f32` skips integers.
    const EXACT_LIMIT: f32 = 16_777_216.0;
    if !(0.0..EXACT_LIMIT).contains(&value) || value.fract() != 0.0 {
        return None;
    }
    // Lossless: checked above to be a whole number in [0, 2^24), which every usize holds.
    Some(value as usize)
}

/// A `u32` pixel count as `usize` (lossless on every target this project builds for).
fn px_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Known, non-empty `[width, height]` of page `page_idx`.
fn page_size(page_infos: &HashMap<usize, PageImageInfo>, page_idx: usize) -> Option<[u32; 2]> {
    page_infos.get(&page_idx).filter(|info| info.width_px > 0 && info.height_px > 0).map(|info| [info.width_px, info.height_px])
}

/// Window title: `{number}. {page file}`, `{number}. {clean file} (clean)` or
/// `{clean file} (clean without a page)`.
fn viewer_title(resolved: &ResolvedTarget, project: &ProjectData) -> String {
    match resolved {
        ResolvedTarget::Page { page_idx } => {
            let name = project.pages.get(*page_idx).and_then(|page| page.path.file_name()).map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            tf!("page_manager.viewer.page_title", number = page_idx + 1, name = name).to_string()
        }
        ResolvedTarget::Clean(ResolvedClean { page_idx: Some(page_idx), file_name, .. }) => tf!("page_manager.viewer.clean_title", number = page_idx + 1, name = file_name).to_string(),
        ResolvedTarget::Clean(ResolvedClean { page_idx: None, file_name, .. }) => tf!("page_manager.viewer.unassigned_clean_title", name = file_name).to_string(),
    }
}

/// `[width, height]` of what the viewer shows: the page's, or the shown clean's real size, else
/// the size its link / probe reported.
fn shown_image_size(viewer: &PageViewer, resolved: &ResolvedTarget, page_infos: &HashMap<usize, PageImageInfo>) -> Option<[usize; 2]> {
    match resolved {
        ResolvedTarget::Page { page_idx } => page_size(page_infos, *page_idx).map(|size| size.map(px_usize)),
        ResolvedTarget::Clean(clean) => viewer.shown.as_ref().map(|shown| shown.prepared.grid().image_size()).or_else(|| clean.size.map(|size| size.map(px_usize))),
    }
}

/// The clean's size for the «поверх страницы» rule: the shown pixels' real size first.
fn clean_size_for_toggle(viewer: &PageViewer, clean: &ResolvedClean) -> Option<[u32; 2]> {
    viewer
        .shown
        .as_ref()
        .and_then(|shown| {
            let [width, height] = shown.prepared.grid().image_size();
            Some([u32::try_from(width).ok()?, u32::try_from(height).ok()?])
        })
        .or(clean.size)
}

/// Whether the clean is drawn over its page this frame.
fn over_page_effective(viewer: &PageViewer, clean: &ResolvedClean, page_infos: &HashMap<usize, PageImageInfo>) -> Option<usize> {
    let page_idx = clean.page_idx?;
    (viewer.over_page && over_page_available(clean.bound_ok, clean_size_for_toggle(viewer, clean), page_size(page_infos, page_idx))).then_some(page_idx)
}

/// The toolbar: fit button, zoom, pixel size and (for a clean) the «поверх страницы» toggle.
fn draw_viewer_toolbar(ui: &mut egui::Ui, viewer: &mut PageViewer, resolved: &ResolvedTarget, page_infos: &HashMap<usize, PageImageInfo>) {
    ui.horizontal_wrapped(|ui| {
        if ui.button(t!("page_manager.viewer.fit_button")).clicked() {
            viewer.fit_pending = true;
        }
        let percent = format!("{:.0}", viewer.camera.zoom() * 100.0);
        ui.label(tf!("page_manager.viewer.zoom_label", percent = percent));
        ui.separator();
        match shown_image_size(viewer, resolved, page_infos) {
            Some([width, height]) => ui.label(tf!("page_manager.viewer.size_label", width = width, height = height)),
            None => ui.weak(t!("page_manager.viewer.size_unknown_label")),
        };
        if let ResolvedTarget::Clean(clean) = resolved
            && let Some(page_idx) = clean.page_idx
        {
            ui.separator();
            let available = over_page_available(clean.bound_ok, clean_size_for_toggle(viewer, clean), page_size(page_infos, page_idx));
            ui.add_enabled(available, egui::Checkbox::new(&mut viewer.over_page, t!("page_manager.viewer.over_page_checkbox")))
                .on_disabled_hover_text(t!("page_manager.viewer.over_page_disabled_tooltip"));
        }
    });
}

/// The board: camera input, then the page tiles and / or the clean tiles, then any status text.
fn draw_viewer_board(ui: &mut egui::Ui, viewer: &mut PageViewer, resolved: &ResolvedTarget, clean_status: Option<CleanStatus>, page_infos: &HashMap<usize, PageImageInfo>, textures: &mut HashMap<usize, PageTexture>) {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::ZERO, ui.visuals().extreme_bg_color);

    // The toolbar above already printed this frame's zoom; a camera change below repaints once
    // more so the label catches up instead of lagging until the next input.
    let camera_before = viewer.camera;
    let image_size = shown_image_size(viewer, resolved, page_infos);
    if viewer.fit_pending
        && rect.width() > 1.0
        && rect.height() > 1.0
        && let Some(size) = image_size
    {
        viewer.camera.fit(rect, size);
        viewer.fit_pending = false;
    }
    handle_board_input(ui, viewer, rect, &response);
    if viewer.camera != camera_before {
        ui.ctx().request_repaint();
    }

    let zoom = viewer.camera.zoom();
    let nearest = nearest_sampling_for(zoom);
    let mut budget = UploadBudget::new(UPLOAD_TILES_PER_FRAME, UPLOAD_BYTES_PER_FRAME);
    let mut frame = BoardFrame { ui, painter: &painter, placement: Placement::scaled(viewer.camera.origin(rect), zoom), cull: rect, nearest, budget: &mut budget };
    viewer.source_page = None;
    viewer.source_nearest = nearest;

    let status = match resolved {
        ResolvedTarget::Page { page_idx } => {
            viewer.source_page = Some(*page_idx);
            draw_source_page(&mut frame, *page_idx, page_infos, textures, &mut viewer.malformed_tiles_logged)
        }
        ResolvedTarget::Clean(clean) => {
            let base_page = over_page_effective(viewer, clean, page_infos);
            let base_status = base_page.map(|page_idx| {
                viewer.source_page = Some(page_idx);
                draw_source_page(&mut frame, page_idx, page_infos, textures, &mut viewer.malformed_tiles_logged)
            });
            let clean_status = draw_clean(&mut frame, viewer, clean, clean_status, base_page.is_some());
            // A clean error outranks the page's state; otherwise report the page's loading.
            match (clean_status, base_status) {
                (BoardStatus::Ready, Some(page_status)) => page_status,
                (clean_status, _) => clean_status,
            }
        }
    };
    paint_board_status(ui, &painter, rect, &status);
}

/// Wheel zoom around the cursor (sign only, skipped under an open combo popup) and drag pan.
fn handle_board_input(ui: &egui::Ui, viewer: &mut PageViewer, rect: egui::Rect, response: &egui::Response) {
    let wheel_y = if response.hovered() && !combo_popup_open(ui.ctx()) { ui.ctx().input(|input| ms_widgets::input_util::raw_wheel_delta(input).y) } else { 0.0 };
    let notches = if wheel_y > 0.0 {
        Some(1.0)
    } else if wheel_y < 0.0 {
        Some(-1.0)
    } else {
        None
    };
    if let Some(notches) = notches {
        let anchor = response.hover_pos().filter(|pos| rect.contains(*pos)).unwrap_or_else(|| rect.center());
        viewer.camera.zoom_by(rect, anchor, notches);
    }
    // Every drag on the board is a pan (the viewer has no handles); zero unless dragged.
    viewer.camera.pan(response.drag_delta());
}

/// What the board says over the image this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BoardStatus {
    /// Drawn (tiles may still be uploading; the upload asks for the repaint).
    Ready,
    /// Waiting for pixels: a spinner.
    Loading,
    /// A localized error message.
    Error(String),
}

/// The per-frame drawing inputs every layer of the board shares: where pixels go on screen, the
/// visible rect, the sampling, and the frame's upload budget (page and clean tiles draw from ONE
/// budget).
struct BoardFrame<'a> {
    ui: &'a egui::Ui,
    painter: &'a egui::Painter,
    placement: Placement,
    cull: egui::Rect,
    nearest: bool,
    budget: &'a mut UploadBudget,
}

/// Draws source page `page_idx` from the app's resident tiles, or reports why it cannot.
/// `malformed_logged` remembers which malformed tiles were already logged (see
/// [`paint_source_page`]).
fn draw_source_page(frame: &mut BoardFrame<'_>, page_idx: usize, page_infos: &HashMap<usize, PageImageInfo>, textures: &mut HashMap<usize, PageTexture>, malformed_logged: &mut HashSet<(usize, usize)>) -> BoardStatus {
    match page_infos.get(&page_idx).map(|info| info.load_state) {
        Some(SourcePageLoadState::Failed) => return BoardStatus::Error(t!("page_manager.viewer.page_failed_error").to_string()),
        Some(SourcePageLoadState::Loading) | None => return BoardStatus::Loading,
        Some(SourcePageLoadState::Available) => {}
    }
    // Available but not in the map yet: the app's incremental upload has not reached it.
    let Some(page_texture) = textures.get_mut(&page_idx) else { return BoardStatus::Loading };
    if paint_source_page(frame, page_idx, page_texture, malformed_logged) {
        frame.ui.ctx().request_repaint();
    }
    BoardStatus::Ready
}

/// Draws the clean (checkerboard under it unless it sits over its page) and reports its state.
fn draw_clean(frame: &mut BoardFrame<'_>, viewer: &mut PageViewer, clean: &ResolvedClean, clean_status: Option<CleanStatus>, over_page: bool) -> BoardStatus {
    match clean_status {
        Some(CleanStatus::Shown) => {}
        Some(CleanStatus::Loading) | None => return BoardStatus::Loading,
        Some(CleanStatus::Failed) => return BoardStatus::Error(clean_failure_message(viewer, clean)),
    }
    let Some(shown) = viewer.shown.as_mut() else { return BoardStatus::Loading };
    if !over_page {
        let [width, height] = shown.prepared.grid().image_size();
        let image_rect = frame.placement.px_rect_to_screen(PxRect::new(0, 0, width, height));
        ms_theme::checkerboard::CANVAS.paint(frame.painter, image_rect, egui::CornerRadius::ZERO);
    }
    // Switching options marks every tile pending; the old texels keep drawing until re-sent.
    shown.texture.set_options(clean_texture_options_for(frame.nearest));
    let report = shown.texture.upload(frame.ui.ctx(), &mut shown.prepared, frame.budget, UploadScope::visible(frame.placement, frame.cull));
    shown.texture.paint(frame.painter, frame.placement, frame.cull, egui::Color32::WHITE);
    if report.wants_repaint() {
        frame.ui.ctx().request_repaint();
    }
    BoardStatus::Ready
}

/// The localized message of the clean's current failure.
fn clean_failure_message(viewer: &PageViewer, clean: &ResolvedClean) -> String {
    if let CleanSource::Unreadable(error) = &clean.source {
        return tf!("page_manager.viewer.clean_unreadable_error", error = error).to_string();
    }
    match viewer.failed.as_ref().map(|(_, failure)| failure) {
        Some(ViewerFailure::Load(error)) => tf!("page_manager.viewer.clean_load_error", error = error).to_string(),
        Some(ViewerFailure::Texture(error)) => tf!("page_manager.viewer.clean_load_error", error = error).to_string(),
        Some(ViewerFailure::NoPixels) => t!("page_manager.viewer.clean_empty_error").to_string(),
        Some(ViewerFailure::ModelUnavailable) => t!("page_manager.viewer.model_unavailable_error").to_string(),
        Some(ViewerFailure::WorkerUnavailable(detail)) => tf!("page_manager.viewer.worker_error", error = detail).to_string(),
        // `Failed` without a recorded failure cannot happen (a request either flies or failed);
        // say so generically rather than show nothing.
        None => t!("page_manager.viewer.clean_unknown_error").to_string(),
    }
}

/// Paints the status over the board: a spinner while loading, a wrapped error message.
fn paint_board_status(ui: &egui::Ui, painter: &egui::Painter, rect: egui::Rect, status: &BoardStatus) {
    match status {
        BoardStatus::Ready => {}
        BoardStatus::Loading => {
            let spinner_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(32.0, 32.0));
            egui::Spinner::new().size(32.0).paint_at(ui, spinner_rect);
        }
        BoardStatus::Error(message) => {
            let wrap = (rect.width() - 32.0).max(80.0);
            let galley = painter.layout(message.clone(), egui::FontId::proportional(15.0), ms_theme::status::ERROR, wrap);
            let pos = rect.center() - galley.size() * 0.5;
            painter.galley(pos, galley, ms_theme::status::ERROR);
        }
    }
}

/// TEMPORARY adapter: paints a source page from the app's resident `PageTexture` tiles through
/// an `egui_large_image::Placement`, re-uploading a GPU-evicted tile from its kept RGBA under
/// `budget` (the canvas' own lazy re-upload rule, `ms-canvas` `scene.rs`), and stamps the page's
/// last-used frames. Returns whether an in-view tile is still waiting for budget. A malformed
/// tile (RGBA length not `w * h * 4`) is skipped for good, logged once per viewer through
/// `malformed_logged`, since no later frame can fix it.
///
/// Removal condition: canvas source pages migrated to `egui-large-image` (plan step 5); the page
/// is then a `TiledTexture` painted with `TiledTexture::paint` and this loop goes away.
fn paint_source_page(frame: &mut BoardFrame<'_>, page_idx: usize, page_texture: &mut PageTexture, malformed_logged: &mut HashSet<(usize, usize)>) -> bool {
    let ctx = frame.ui.ctx();
    let (placement, cull, nearest) = (frame.placement, frame.cull, frame.nearest);
    let frame_nr = ctx.cumulative_frame_nr();
    let (mut linear_used, mut nearest_used, mut work_remaining) = (false, false, false);
    for (tile_idx, tile) in page_texture.tiles.iter_mut().enumerate() {
        let (Some(x), Some(y), Some(width), Some(height)) = (px_from_f32(tile.origin_px.x), px_from_f32(tile.origin_px.y), px_from_f32(tile.size_px.x), px_from_f32(tile.size_px.y)) else {
            continue;
        };
        let tile_rect = placement.px_rect_to_screen(PxRect::new(x, y, width, height));
        if !tile_rect.intersects(cull) {
            continue;
        }
        if !rgba_len_matches([width, height], tile.rgba.len()) {
            if malformed_logged.insert((page_idx, tile_idx)) {
                ms_log::runtime_log::log_warn(format!("[page-manager::viewer] source tile {tile_idx} of page {page_idx} skipped: {} RGBA bytes for {width}x{height} px", tile.rgba.len()));
            }
            continue;
        }
        if tile.linear_texture.is_none() {
            work_remaining |= !upload_page_tile(ctx, &mut tile.linear_texture, format!("page-{page_idx}-tile-{tile_idx}-linear"), [width, height], &tile.rgba, egui::TextureOptions::LINEAR, frame.budget);
        }
        if nearest && tile.nearest_texture.is_none() {
            work_remaining |= !upload_page_tile(ctx, &mut tile.nearest_texture, format!("page-{page_idx}-tile-{tile_idx}-nearest"), [width, height], &tile.rgba, egui::TextureOptions::NEAREST, frame.budget);
        }
        // A nearest tile still waiting for budget falls back to its linear texture.
        let chosen = if nearest { tile.nearest_texture.as_ref().map(|texture| (texture.id(), true)) } else { None }
            .or_else(|| tile.linear_texture.as_ref().map(|texture| (texture.id(), false)));
        if let Some((texture_id, is_nearest)) = chosen {
            frame.painter.image(texture_id, tile_rect, UV_FULL, egui::Color32::WHITE);
            if is_nearest {
                nearest_used = true;
            } else {
                linear_used = true;
            }
        }
    }
    if linear_used {
        page_texture.linear_last_used_frame = frame_nr;
    }
    if nearest_used {
        page_texture.nearest_last_used_frame = frame_nr;
    }
    work_remaining
}

/// Whether `len` RGBA bytes are exactly a `size[0]` x `size[1]` tile (overflow counts as no).
fn rgba_len_matches(size: [usize; 2], len: usize) -> bool {
    size[0].checked_mul(size[1]).and_then(|pixels| pixels.checked_mul(4)) == Some(len)
}

/// Uploads one page tile into `slot` when `budget` admits it; returns `false` when the tile must
/// wait for a later frame's budget. Precondition (checked by [`paint_source_page`] through
/// [`rgba_len_matches`]): `rgba` holds exactly `size` pixels, which `ColorImage` requires.
fn upload_page_tile(ctx: &egui::Context, slot: &mut Option<egui::TextureHandle>, name: String, size: [usize; 2], rgba: &[u8], options: egui::TextureOptions, budget: &mut UploadBudget) -> bool {
    if !budget.try_consume(rgba.len()) {
        return false;
    }
    *slot = Some(ctx.load_texture(name, egui::ColorImage::from_rgba_unmultiplied(size, rgba), options));
    true
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use ms_models::clean_assign::{CleanFileLocation, CleanFileProbe, UnassignedClean};

    use super::*;

    fn ok_model(size: Option<[u32; 2]>) -> PageCleanLink {
        PageCleanLink::Ok { thumb: CleanThumbSource::Model, file_name: "001.png".to_owned(), size }
    }

    fn unassigned(name: &str, size: Result<[u32; 2], String>) -> UnassignedClean {
        UnassignedClean {
            file_name: OsString::from(name),
            committed: Some(CleanFileProbe { path: PathBuf::from("committed").join(name), location: CleanFileLocation::Committed, size: size.clone() }),
            staged: None,
        }
    }

    #[test]
    fn page_target_resolves_while_in_range_only() {
        assert_eq!(resolve_target(&ViewerTarget::Page(2), 3, &[], &[]), Some(ResolvedTarget::Page { page_idx: 2 }));
        assert_eq!(resolve_target(&ViewerTarget::Page(3), 3, &[], &[]), None);
    }

    #[test]
    fn bound_clean_resolves_model_file_and_problem_sources() {
        let file = PathBuf::from("clean").join("002.png");
        let links = vec![
            ok_model(Some([10, 20])),
            PageCleanLink::Ok { thumb: CleanThumbSource::File(file.clone()), file_name: "002.png".to_owned(), size: Some([10, 20]) },
            PageCleanLink::Problem { problem: CleanLinkProblem::SizeMismatch { clean: [5, 5], page: [10, 20] }, file: file.clone(), size: Some([5, 5]) },
            PageCleanLink::Problem { problem: CleanLinkProblem::CleanUnreadable("bad header".to_owned()), file: file.clone(), size: None },
            PageCleanLink::None,
        ];
        let resolve = |idx| match resolve_target(&ViewerTarget::BoundClean(idx), links.len(), &links, &[]) {
            Some(ResolvedTarget::Clean(clean)) => Some(clean),
            Some(ResolvedTarget::Page { .. }) | None => None,
        };
        let model = resolve(0).expect("OK model link resolves");
        assert_eq!((model.source, model.bound_ok, model.page_idx), (CleanSource::Model { page_idx: 0 }, true, Some(0)));
        let bound_file = resolve(1).expect("OK file link resolves");
        assert_eq!((bound_file.source, bound_file.bound_ok), (CleanSource::File(file.clone()), true));
        let mismatch = resolve(2).expect("a mismatched clean is still viewable");
        assert_eq!((mismatch.source, mismatch.bound_ok, mismatch.file_name.as_str()), (CleanSource::File(file), false, "002.png"));
        let unreadable = resolve(3).expect("an unreadable clean shows its error");
        assert_eq!(unreadable.source, CleanSource::Unreadable("bad header".to_owned()));
        // Unlinked meanwhile, or the page is gone: the viewer closes.
        assert_eq!(resolve(4), None);
        assert_eq!(resolve_target(&ViewerTarget::BoundClean(9), links.len(), &links, &[]), None);
    }

    #[test]
    fn unassigned_clean_follows_its_name() {
        let items = vec![unassigned("a.png", Ok([4, 8])), unassigned("b.png", Err("truncated".to_owned()))];
        match resolve_target(&ViewerTarget::Unassigned(OsString::from("a.png")), 0, &[], &items) {
            Some(ResolvedTarget::Clean(clean)) => {
                assert_eq!(clean.source, CleanSource::File(PathBuf::from("committed").join("a.png")));
                assert_eq!((clean.size, clean.page_idx, clean.bound_ok), (Some([4, 8]), None, false));
            }
            other => panic!("unexpected resolution {other:?}"),
        }
        match resolve_target(&ViewerTarget::Unassigned(OsString::from("b.png")), 0, &[], &items) {
            Some(ResolvedTarget::Clean(clean)) => assert_eq!(clean.source, CleanSource::Unreadable("truncated".to_owned())),
            other => panic!("unexpected resolution {other:?}"),
        }
        // Bound, deleted or renamed: the name is no longer listed.
        assert_eq!(resolve_target(&ViewerTarget::Unassigned(OsString::from("c.png")), 0, &[], &items), None);
    }

    #[test]
    fn staged_copy_of_an_unassigned_clean_wins() {
        let mut item = unassigned("a.png", Ok([4, 8]));
        item.staged = Some(CleanFileProbe { path: PathBuf::from("staged").join("a.png"), location: CleanFileLocation::Unsaved, size: Ok([4, 8]) });
        match resolve_target(&ViewerTarget::Unassigned(OsString::from("a.png")), 0, &[], &[item]) {
            Some(ResolvedTarget::Clean(clean)) => assert_eq!(clean.source, CleanSource::File(PathBuf::from("staged").join("a.png"))),
            other => panic!("unexpected resolution {other:?}"),
        }
    }

    #[test]
    fn over_page_needs_an_ok_link_and_the_exact_page_size() {
        assert!(over_page_available(true, Some([10, 20]), Some([10, 20])));
        assert!(!over_page_available(false, Some([10, 20]), Some([10, 20])), "problem / unassigned cleans have no valid base");
        assert!(!over_page_available(true, Some([10, 21]), Some([10, 20])));
        assert!(!over_page_available(true, None, Some([10, 20])), "unknown clean size");
        assert!(!over_page_available(true, Some([10, 20]), None), "unknown page size");
        assert!(!over_page_available(true, Some([0, 0]), Some([0, 0])), "empty page");
    }

    #[test]
    fn a_request_is_due_only_for_a_key_nobody_answers_for() {
        let old = CleanKey::Model { page_idx: 0, revision: 1 };
        let new = CleanKey::Model { page_idx: 0, revision: 2 };
        assert!(clean_request_due(&new, None, None, None));
        // Stale-while-revalidate: the old revision is shown, the new one is still due.
        assert!(clean_request_due(&new, Some(&old), None, None));
        assert!(!clean_request_due(&new, Some(&new), None, None));
        assert!(!clean_request_due(&new, Some(&old), Some(&new), None), "already in flight");
        assert!(!clean_request_due(&new, None, None, Some(&new)), "failed keys are not retried");
        assert!(clean_request_due(&new, None, None, Some(&old)), "a newer revision retries after a failure");
        // A bind / unlink renames the file: a new path is a new request.
        let renamed = CleanKey::File(PathBuf::from("b.png"));
        assert!(clean_request_due(&renamed, Some(&CleanKey::File(PathBuf::from("a.png"))), None, None));
    }

    #[test]
    fn only_the_reply_in_flight_is_current() {
        assert!(reply_is_current(3, Some(3)));
        assert!(!reply_is_current(2, Some(3)), "superseded request");
        assert!(!reply_is_current(3, None), "nothing awaited any more");
    }

    #[test]
    fn nearest_sampling_starts_at_two_hundred_percent() {
        assert!(!nearest_sampling_for(1.99));
        assert!(nearest_sampling_for(2.0));
        assert!(nearest_sampling_for(8.0));
        assert_eq!(clean_texture_options(0.5), egui::TextureOptions::LINEAR);
        assert_eq!(clean_texture_options(4.0), egui::TextureOptions::NEAREST);
    }

    #[test]
    fn tile_coordinates_convert_only_when_exact() {
        assert_eq!(px_from_f32(0.0), Some(0));
        assert_eq!(px_from_f32(2048.0), Some(2048));
        assert_eq!(px_from_f32(16_777_215.0), Some(16_777_215));
        assert_eq!(px_from_f32(16_777_216.0), None);
        assert_eq!(px_from_f32(12.5), None);
        assert_eq!(px_from_f32(-1.0), None);
        assert_eq!(px_from_f32(f32::NAN), None);
        assert_eq!(px_from_f32(f32::INFINITY), None);
    }

    #[test]
    fn model_pixels_split_into_portable_tiles() {
        let tall = u32::try_from(PORTABLE_TILE_SIDE).expect("the portable tile side fits u32") + 1;
        let image = RgbaImage::from_pixel(3, tall, image::Rgba([1, 2, 3, 4]));
        let prepared = prepare_clean(JobSource::Model(Arc::new(image))).expect("a valid image splits");
        assert_eq!(prepared.grid().image_size(), [3, PORTABLE_TILE_SIDE + 1]);
        assert_eq!(prepared.grid().len(), 2);
    }

    #[test]
    fn a_missing_file_is_a_decode_error() {
        let path = std::env::temp_dir().join("ms_pm_viewer_missing_clean_does_not_exist.png");
        assert!(matches!(prepare_clean(JobSource::File(path)), Err(CleanLoadError::Decode { .. })));
    }

    fn model_job(epoch: u64, rgba: &Arc<RgbaImage>, ctx: &egui::Context) -> ViewerJob {
        ViewerJob { epoch, source: JobSource::Model(Arc::clone(rgba)), ctx: ctx.clone() }
    }

    #[test]
    fn only_the_newest_queued_job_survives_and_older_arcs_are_released() {
        let ctx = egui::Context::default();
        let (job_tx, job_rx) = mpsc::channel();
        let (old_a, old_b, newest) = (Arc::new(RgbaImage::new(1, 1)), Arc::new(RgbaImage::new(1, 1)), Arc::new(RgbaImage::new(1, 1)));
        job_tx.send(model_job(2, &old_b, &ctx)).expect("receiver alive");
        job_tx.send(model_job(3, &newest, &ctx)).expect("receiver alive");
        let job = newest_job(model_job(1, &old_a, &ctx), &job_rx);
        assert_eq!(job.epoch, 3);
        // The superseded jobs were dropped before any preparation: their pixels are released.
        assert_eq!((Arc::strong_count(&old_a), Arc::strong_count(&old_b)), (1, 1));
        assert_eq!(Arc::strong_count(&newest), 2, "the newest job still holds its pixels");
    }

    #[test]
    fn the_worker_answers_only_the_newest_current_job_then_exits() {
        let ctx = egui::Context::default();
        let (job_tx, job_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let epoch = AtomicU64::new(3);
        let small = Arc::new(RgbaImage::new(2, 2));
        let tall = Arc::new(RgbaImage::new(3, 5));
        for job in [model_job(1, &small, &ctx), model_job(2, &small, &ctx), model_job(3, &tall, &ctx)] {
            job_tx.send(job).expect("receiver alive");
        }
        drop(job_tx);
        // Runs on this thread; returns once the closed job channel is drained.
        run_viewer_worker(&job_rx, &reply_tx, &epoch);
        let replies: Vec<ViewerReply> = reply_rx.try_iter().collect();
        assert_eq!(replies.len(), 1, "superseded jobs are neither prepared nor answered");
        assert_eq!(replies[0].epoch, 3);
        let prepared = replies[0].result.as_ref().expect("the newest job prepares");
        assert_eq!(prepared.grid().image_size(), [3, 5]);
        assert_eq!(Arc::strong_count(&tall), 1, "the worker dropped the model pixels after the split");
    }

    #[test]
    fn a_cancelled_job_is_not_answered() {
        let ctx = egui::Context::default();
        let (job_tx, job_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        // `cancel` bumped the epoch past the queued job.
        let epoch = AtomicU64::new(2);
        job_tx.send(model_job(1, &Arc::new(RgbaImage::new(1, 1)), &ctx)).expect("receiver alive");
        drop(job_tx);
        run_viewer_worker(&job_rx, &reply_tx, &epoch);
        assert!(reply_rx.try_recv().is_err());
    }

    #[test]
    fn a_spawned_worker_answers_its_job() {
        let ctx = egui::Context::default();
        let worker = ViewerWorker::spawn().expect("the worker thread starts");
        let epoch = worker.submit(JobSource::Model(Arc::new(RgbaImage::new(4, 4))), &ctx).expect("the worker accepts jobs");
        let reply = worker.replies.recv_timeout(std::time::Duration::from_secs(10)).expect("the worker answers");
        assert_eq!(reply.epoch, epoch);
        assert!(reply.result.is_ok());
    }

    #[test]
    fn cancel_supersedes_the_epoch_and_drops_unread_replies() {
        let (job_tx, _job_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let worker = ViewerWorker { jobs: job_tx, replies: reply_rx, epoch: Arc::new(AtomicU64::new(7)) };
        reply_tx.send(ViewerReply { epoch: 7, result: Ok(prepared(2, 2)) }).expect("receiver alive");
        worker.cancel();
        assert_eq!(worker.epoch.load(Ordering::Acquire), 8, "a job of epoch 7 is no longer current");
        assert!(worker.replies.try_recv().is_err(), "the unread reply was dropped");
        reply_tx.send(ViewerReply { epoch: 7, result: Ok(prepared(2, 2)) }).expect("receiver alive");
        worker.drop_replies();
        assert!(worker.replies.try_recv().is_err());
    }

    #[test]
    fn model_pixel_identity_follows_every_change() {
        let mut pixels = Arc::new(RgbaImage::new(2, 2));
        let token = Arc::downgrade(&pixels);
        assert!(same_model_pixels(&token, &pixels));
        // An in-place edit of a uniquely owned page (the model's `make_mut`) moves the pixels to a
        // new allocation while the token is alive.
        Arc::make_mut(&mut pixels).put_pixel(0, 0, image::Rgba([9, 9, 9, 9]));
        assert!(!same_model_pixels(&token, &pixels));
        assert!(!same_model_pixels(&Arc::downgrade(&Arc::new(RgbaImage::new(2, 2))), &pixels), "another page");
    }

    fn prepared(width: u32, height: u32) -> PreparedTiles {
        split_rgba(&RgbaImage::new(width, height)).expect("a valid image splits")
    }

    fn reply_for(viewer: &mut PageViewer, epoch: u64, revision: u64, tiles: PreparedTiles) {
        viewer.in_flight = Some(CleanRequest { epoch, key: CleanKey::Model { page_idx: 0, revision }, model_pixels: None });
        viewer.apply_reply(ViewerReply { epoch, result: Ok(tiles) });
    }

    #[test]
    fn a_same_size_refresh_keeps_the_textures_and_resends_in_place() {
        let ctx = egui::Context::default();
        let mut viewer = PageViewer::new(ViewerTarget::BoundClean(0));
        reply_for(&mut viewer, 1, 1, prepared(4, 4));
        let shown = viewer.shown.as_mut().expect("the first reply is shown");
        let mut budget = UploadBudget::new(usize::MAX, usize::MAX);
        shown.texture.upload(&ctx, &mut shown.prepared, &mut budget, UploadScope::All);
        assert_eq!((shown.texture.resident_tiles(), shown.texture.pending_tiles()), (1, 0));

        reply_for(&mut viewer, 2, 2, prepared(4, 4));
        let shown = viewer.shown.as_ref().expect("the refresh is shown");
        assert_eq!(shown.key, CleanKey::Model { page_idx: 0, revision: 2 });
        // The old texture stays resident (drawn) while its tile waits to be re-sent in place.
        assert_eq!((shown.texture.resident_tiles(), shown.texture.pending_tiles()), (1, 1));

        reply_for(&mut viewer, 3, 3, prepared(4, 6));
        let shown = viewer.shown.as_ref().expect("the resized clean is shown");
        assert_eq!(shown.texture.grid().image_size(), [4, 6]);
        assert_eq!(shown.texture.resident_tiles(), 0, "a new size builds a new texture set");
    }

    #[test]
    fn a_stale_reply_is_ignored() {
        let mut viewer = PageViewer::new(ViewerTarget::BoundClean(0));
        viewer.in_flight = Some(CleanRequest { epoch: 5, key: CleanKey::Model { page_idx: 0, revision: 1 }, model_pixels: None });
        viewer.apply_reply(ViewerReply { epoch: 4, result: Ok(prepared(2, 2)) });
        assert!(viewer.shown.is_none());
        assert_eq!(viewer.in_flight.as_ref().map(|request| request.epoch), Some(5));
    }

    #[test]
    fn a_very_tall_strip_fits_the_minimum_window() {
        // 800 x 40 000 px into a board of a window at its minimum height: the fit is far below
        // the default minimum zoom and must not be clamped.
        let board = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(400.0, 250.0));
        let mut camera = ViewerCamera::default();
        camera.fit(board, [800, 40_000]);
        let expected = 250.0 / 40_000.0 * FIT_MARGIN;
        assert!((camera.zoom() - expected).abs() < 1e-7, "zoom {} != {expected}", camera.zoom());
        assert!(camera.min_zoom < camera.zoom());
        let top = camera.origin(board).y;
        let bottom = top + 40_000.0 * camera.zoom();
        assert!(top >= board.top() && bottom <= board.bottom(), "the whole strip is on the board");
        // Zooming out stops at the lowered minimum.
        for _ in 0..200 {
            camera.zoom_by(board, board.center(), -1.0);
        }
        assert!((camera.zoom() - camera.min_zoom).abs() < 1e-9);
        // An ordinary page keeps the default minimum.
        let mut page = ViewerCamera::default();
        page.fit(board, [800, 1200]);
        assert!((page.min_zoom - BASE_MIN_ZOOM).abs() < f32::EPSILON);
    }

    #[test]
    fn wheel_zoom_keeps_the_point_under_the_cursor() {
        let board = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 400.0));
        let mut camera = ViewerCamera::default();
        camera.fit(board, [300, 900]);
        let anchor = egui::pos2(123.0, 77.0);
        let before = camera.screen_to_px(board, anchor);
        camera.zoom_by(board, anchor, 1.0);
        let after = camera.screen_to_px(board, anchor);
        assert!((before - after).length() < 1e-3, "{before:?} vs {after:?}");
        camera.pan(egui::vec2(10.0, 0.0));
        assert!((camera.screen_to_px(board, anchor).x - (after.x - 10.0 / camera.zoom())).abs() < 1e-3);
    }

    #[test]
    fn tile_rgba_length_must_match_exactly() {
        assert!(rgba_len_matches([2, 3], 24));
        assert!(!rgba_len_matches([2, 3], 23));
        assert!(!rgba_len_matches([usize::MAX, 2], 0), "overflow is a mismatch");
    }
}
