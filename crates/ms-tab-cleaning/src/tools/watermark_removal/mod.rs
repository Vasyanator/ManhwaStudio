/*
FILE HEADER (cleaning/tools/watermark_removal/mod.rs)

Purpose:
The standalone «Удаление водяных знаков» cleaning tool. Unlike every other
region tool it needs no user-painted mask: the network predicts its own (or, in
chapter mode, the mark is solved for outright), so the tool is built on
`RegionEditToolBase` alone (not `RegionMaskInpaintToolBase`).

Three modes, all driven from the one region-editor window:
- `MaskOnly` (default): streams `watermark.detect` and draws the predicted mask
  over the region WITHOUT touching pixels. This is the honest default — the mask
  is the product, and the actual cleaning is done by the project's inpainters
  (`dev-docs/watermark_removal_plan.md` §2).
- `Clean` (explicitly experimental): streams `watermark.remove`, replaces the
  region image with the network's reconstruction and keeps its mask for preview.
  On manhwa line art this softens strokes and leaves residue (plan §1.2), which
  is why the mode carries a visible experiment marker.
- `Chapter` — «По главе (точное вычитание)»: no network, no backend, no Torch.
  The GUI-free engine in `crate::watermark_chapter` solves the compositing
  equation `I = c + s*B` from occurrences observed over different backgrounds and
  removes by division. This layer owns the catalog of marks, the calibration
  samples, the background jobs, the reports, the overlay patches and the on-disk
  library (`watermark_library.rs`); it owns no maths.

The Torch requirement lives INSIDE the tool: `ai_requirement()` is a constant
`None`, so the picker button is never AI-gated and the chapter mode stays
reachable without Torch, while the two network entries of the mode picker and the
run button carry the gate and name their own reason.

The tool owns a SECOND dock tab beside «Выбранный инструмент»: «Библиотека знаков»
(`wants_library_panel` / `draw_library_panel`, state in
`watermark_library_window.rs`), toggled by the two «Библиотека знаков…» buttons.
It also owns that panel's reach into the canvas: `library_arm` reserves the next
completed selection FOR ONE ENTRY, and it is dropped the moment the screen that draws its cancel
button is gone (`WatermarkLibraryWindow::arm_survives`, `deactivate`) — and SAYS SO on the
panel (`drop_reservation_without_a_screen`), as it does for a parked rect that arrives with no
reservation (`take_reservation_for_diverted`) and for a drag that produced no rectangle at all
(`report_selection_refusal`). A reservation
and the region base's SELECTION DIVERSION are one fact, kept in step by
`sync_selection_diversion` after every mutation of `library_arm`: while a reservation
is live the drag is cut out for the library and does NOT run the ordinary region
editor — no loader job, no loading window, no editor left standing over the canvas
with its zoom and undo shortcuts disabled. The canvas says so too: `draw_cursor`
tints the crosshair and the rubber band with the cancel button's own red
(`ARM_CANCEL_RGB`) while a reservation is in force, and the drag becomes easier to
START: while armed a plain drag selects as well as the selection-mode drag
(`cleaning.region.select_rect_armed`), which is what the panel's own
`library_arm_select_hint` tells the user, from the current bindings. Every refusal of the capture lane — its own early returns and every error of
`run_library_capture` — is reported the way a refused intake is: a red row on the armed entry
(`WatermarkLibraryWindow::reject_capture`) plus a `log_warn`, never the status line alone.
The crop the lane cuts is the selection grown by the measurement ring CLIPPED AT THE PAGE
BORDER, and the real per-side margins travel with it (`GrownCrop` -> `CapturedCrop::margins` ->
`ReferenceIntakeRequest::crop_margins`) so the intake derives the footprint that was cut: a mark
stamped flush against the image edge has no background on that side and never will, and
demanding the full ring on all four sides made such a mark unsamplable. The crops the
capture writes are session scratch in the system temp directory; the panel deletes
each one when it is committed or discarded, and `sweep_stale_scratch_crops` clears
what a killed run left behind, once per process on a worker thread.

Key items:
- `WatermarkRemovalTool`: the `CleaningTool` implementation and its wiring.
- `WatermarkMode` / `WatermarkNetworkMode`: the three-way user selection, and the
  two-way one that actually reaches the Python backend.
- `WatermarkRemovalSettings`: persisted parameters (`watermark_removal_settings.json`
  via `config::watermark_removal_settings_path`), loaded/saved on worker threads.
- `WatermarkSessionState`: per-region-editor state the tool owns itself, because
  it lives in the private `RegionInpaintEditorState` for mask tools: the run
  channel, the undo stack of the last runs, and the mask preview texture.
- `ChapterState` / `ChapterCatalog` / `ChapterMark`: the chapter mode's catalog and
  its UI state. `ChapterCatalog::kinds` is the slice the engine's
  `find_matching_kind` runs on, index-aligned with `ChapterCatalog::marks`.
- `run_watermark`: the whole neural worker-thread pass (PNG encode, streaming IPC
  call, blob split, PNG decode).
- `run_chapter_sample` / `run_chapter_scan` / `run_chapter_apply`: the chapter
  mode's worker passes. They decode ONE page at a time — a chapter is several
  strips of ~700x18000 — and stream progress back over a channel.

Files:
This root holds the header, the imports, the constants and EVERY type declaration of the
tool (including `WatermarkRemovalTool` and its `Default`); behaviour lives in the children
(see `MODULE_README.md`): `settings.rs`, `chapter_state.rs`, `library_capture.rs`,
`chapter_jobs.rs`, `describe.rs`, `tool_ops.rs`, `cleaning_tool.rs`, `editor_ui.rs`,
`backend.rs`, `tests.rs`.

Contracts:
- The GUI thread never blocks: every IPC call (`detect`/`remove`/`status`/`unload`),
  every page decode, every fit and every library write runs on an `ms_thread::spawn`
  worker and the GUI polls a channel.
- `watermark.remove` answers with `clean_png ++ mask_png` in one blob; both
  `image_len` and `mask_len` are validated with STRICT equality against the blob
  length before slicing (`split_watermark_remove_blob`).
- Model ids (`slbr`/`wdnet`/`splitnet`) and mode wire values are the persisted
  selection identity, so they stay literals; the catalog and its ✓/«скачать»
  hint are reused from `base.rs`, never duplicated here.
- Apply goes through `CanvasView` (the base's own apply, or
  `replace_overlay_region_px` per patch in chapter mode). This tool never writes
  `CleanOverlaysModel` storage itself.
- Chapter removal is licensed only for gain-verified occurrences; a
  correlation-only accept is COUNTED and reported as refused, never subtracted.
- A session without the series library (`set_session_capabilities`:
  `SessionCapabilities::series_watermark_library` false, i.e. a single-image
  session) has no chapter mode: every mode read goes through `effective_watermark_mode` (persisted «По главе» runs as
  `MaskOnly` for the session, the stored value is rewritten only by a real pick),
  the picker offers `offered_watermark_modes`, and `start_chapter_request` refuses —
  `chapter_source_ref` would otherwise key the GLOBAL library by the scratch root.
- Honest reporting (plan, "Corrections from the second implementation round"): the
  UI says the IMPRINT is measured exactly, never that «c точен»; the stated ±%
  bounds the alpha SCALE only; the exact/clipped shares are labelled as a
  quantization-and-clipping report, and model quality is reported by the detection
  gain and t-statistic instead.
*/
use super::base::RegionEditToolBase;
use super::mask_generation::{WatermarkProgress, WatermarkStatus};
use super::watermark_entry::{CropMargins, LibraryCandidate};
use super::watermark_library::{EntrySummary, LibrarySample, StoredAlphaAssumption};
use super::watermark_library_window::{
    CapturedCrop, LibraryArm, LibraryCaptureRefusal, LibraryPanelRequest, WatermarkLibraryWindow,
};
use ms_canvas::OverlayRectPx;
use crate::watermark_chapter::{
    Occurrence, PixelRect, RemovalResidual, SampleParams, SampleVerdict, WatermarkKind,
};
use eframe::egui;
use egui::TextureHandle;
use image::RgbaImage;
use ms_thread as thread;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use web_time::Duration;

mod backend;
mod chapter_jobs;
mod chapter_state;
mod cleaning_tool;
mod describe;
mod editor_ui;
mod library_capture;
mod settings;
mod tool_ops;

pub(super) use describe::describe_conditioning;
use library_capture::sweep_stale_scratch_crops;

/// Per-message timeout of the streaming `watermark.*` run. `call_streaming`
/// restarts it on every frame received, so this bounds the gap BETWEEN frames,
/// not the total duration of a first run that downloads code and weights
/// (~80-125 MiB, `dev-docs/watermark_removal_plan.md` §4.2).
const WATERMARK_RUN_CALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Timeout of the one-shot `watermark.unload` call.
const WATERMARK_UNLOAD_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Tile side bounds of the `watermark.remove` pass. Tiles are square so SLBR's
/// non-square bug cannot be hit (plan §7.3) and are snapped to a multiple of 16,
/// which SLBR and SplitNet require of their input (plan §3.7).
const WATERMARK_TILE_MIN: u32 = 256;
const WATERMARK_TILE_MAX: u32 = 1024;
const WATERMARK_TILE_MULTIPLE: u32 = 16;
/// Upper bound of the tile overlap control; the effective value is additionally
/// capped at half the tile so neighbouring tiles cannot swallow each other.
const WATERMARK_OVERLAP_MAX: u32 = 256;
const WATERMARK_THRESHOLD_MIN: f32 = 0.05;
const WATERMARK_THRESHOLD_MAX: f32 = 0.95;
const WATERMARK_DILATE_MAX: u32 = 30;
/// Yellow tint of the predicted-mask overlay: the raster form of the shared removal-mask tint
/// `ms_theme::canvas::MASK_TINT`, the mask editor's «удаление» colour, so the two surfaces read
/// the same.
const WATERMARK_MASK_PREVIEW_RGB: [u8; 3] = [ms_theme::canvas::MASK_TINT.r(), ms_theme::canvas::MASK_TINT.g(), ms_theme::canvas::MASK_TINT.b()];

/// What the tool does with the selection.
///
/// The wire strings are the persisted identity of the mode and stay literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatermarkMode {
    /// Predict the mask and show it; pixels are left untouched.
    MaskOnly,
    /// Let the network reconstruct the region (experimental, see the file header).
    Clean,
    /// Solve the mark's compositing equation over the whole chapter and subtract it
    /// exactly. Runs entirely locally — no backend, no Torch, no weights.
    Chapter,
}

/// The two modes that actually reach the Python backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatermarkNetworkMode {
    MaskOnly,
    Clean,
}

/// Persisted parameters of the tool.
///
/// `model` and `mode` are wire values, not labels. Every numeric field is a
/// REQUEST value: it is sent to the backend only after `normalized()`, so a
/// hand-edited settings file cannot push an out-of-range value onto the network.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct WatermarkRemovalSettings {
    mode: String,
    model: String,
    /// Square tile side of the `watermark.remove` pass, in pixels.
    tile: u32,
    /// Overlap between neighbouring tiles, in pixels.
    overlap: u32,
    /// Binarization threshold applied to the predicted soft mask.
    threshold: f32,
    /// Dilation radius applied to the binarized mask, in pixels.
    dilate_px: u32,
    /// Whether the predicted mask is drawn over the region preview.
    show_mask_preview: bool,
    /// Chapter mode: half-width of the anchor band an occurrence must sit in, pixels.
    chapter_anchor_tolerance_px: u32,
    /// Chapter mode: radius of the `s`-weighted background estimator, pixels. A source
    /// with FAT SOLID glyphs needs a larger radius; the gain window is never the knob to
    /// widen (`dev-docs/watermark_chapter_decomposition_plan.md`, corrections §3).
    chapter_background_blur_px: u32,
    /// Chapter mode: thickness of the ring measured around a calibration sample, pixels.
    chapter_ring_width_px: u32,
}

/// Result of one finished run.
struct WatermarkRunOutcome {
    /// The reconstructed region, or `None` in mask-only mode where pixels are
    /// deliberately left untouched.
    image: Option<egui::ColorImage>,
    /// The predicted mask in region coordinates (opaque white = watermark).
    mask: egui::ColorImage,
}

/// Message the run worker sends back. `source` is the region image the run
/// started from and becomes the undo entry when the run replaced pixels.
struct WatermarkJobResult {
    source: egui::ColorImage,
    result: Result<WatermarkRunOutcome, String>,
}

/// State scoped to ONE open region editor session.
///
/// Mask tools keep the equivalent inside the private `RegionInpaintEditorState`;
/// a `RegionEditToolBase`-only tool has to own it, so this struct holds the run
/// channel, the undo stack and the mask-preview texture, and is reset whenever
/// the editor opens a new region (`sync_session`).
#[derive(Default)]
struct WatermarkSessionState {
    /// `scroll_id` of the editor session this state belongs to.
    scroll_id: Option<u64>,
    /// Region images from before each applied run, most recent last.
    undo_stack: Vec<egui::ColorImage>,
    /// Tinted overlay built from the last predicted mask.
    mask_preview: Option<egui::ColorImage>,
    /// Texture of `mask_preview`; cleared (not patched) whenever it changes.
    mask_texture: Option<TextureHandle>,
    run_rx: Option<Receiver<WatermarkJobResult>>,
}

// ---------------------------------------------------------------------------------------
// Chapter mode: catalog, jobs and reports
// ---------------------------------------------------------------------------------------

/// Largest side of a chapter mark's template, pixels.
///
/// Detection correlates the template over anchor bands of every page of the chapter, so
/// its area is a direct multiplier on the scan cost; the measured marks are 167x135 and
/// smaller. A larger selection is refused with the limit named rather than silently
/// turning a scan into a multi-minute stall. Shared with the reference-crop intake, so the
/// two paths cannot disagree about what the detector will accept.
pub(super) const CHAPTER_MAX_TEMPLATE_SIDE: u32 = 512;
/// Most flat-ring calibration samples collected automatically per mark during a scan.
/// Beyond a handful the fit stops improving (the residuals are per-occurrence biases, not
/// noise that averages out) while every sample costs memory and refit time.
const CHAPTER_MAX_AUTO_SAMPLES: usize = 16;
/// Bins of the background-luma histogram the scan reports, each 32 LSB wide.
const CHAPTER_HISTOGRAM_BINS: usize = 8;
/// Preview thumbnail side, points.
const CHAPTER_PREVIEW_SIDE: f32 = 96.0;

/// One page as the chapter workers see it.
#[derive(Debug, Clone)]
struct ChapterPageTask {
    page_idx: usize,
    path: PathBuf,
    /// Overlay pixel size of the page when the canvas already knows it. The engine works
    /// on the DECODED page, so rects are mapped between the two spaces; they are identical
    /// whenever the overlay was created from the page's own pixel size.
    overlay_size: Option<[usize; 2]>,
}

/// The two preview images of a fitted model.
#[derive(Debug, Clone)]
struct ChapterPreview {
    /// Opacity map, `max(1 - s)` over channels, as grey.
    alpha: egui::ColorImage,
    /// The mark's own colour `W = c/alpha` where the mark is opaque enough to define it,
    /// transparent elsewhere.
    imprint: egui::ColorImage,
}

/// GPU textures of one mark's preview, rebuilt whenever its `revision` changes.
struct ChapterPreviewTextures {
    revision: u64,
    alpha: TextureHandle,
    imprint: TextureHandle,
}

/// The chapter's own calibration of one mark, parked while a library entry supplies it.
///
/// Adoption is reversible by design: the user may override an automatic match, and the
/// chapter's measurements — which cost a full scan to collect — must still be there when
/// they do.
#[derive(Debug, Clone)]
struct ParkedCalibration {
    crops: Vec<LibrarySample>,
    alpha_assumption: StoredAlphaAssumption,
}

/// One distinct mark of the open chapter, as the TOOL sees it: everything around the
/// engine's `WatermarkKind` that is presentation, provenance or persistence.
///
/// `crops` is kept in LOCKSTEP with the kind's calibration samples: the engine does not
/// hand its sample pixels back, and the library needs them, so every sample added here is
/// added together with the crop it came from.
///
/// Identity lives in the engine kind (`WatermarkKind::id`) and is NOT duplicated here: two
/// fields holding the same id would be two things to keep in agreement.
#[derive(Debug, Clone)]
struct ChapterMark {
    /// User-visible name, stored and shown VERBATIM.
    name: String,
    /// The crop the correlation template was cut from.
    template_crop: RgbaImage,
    /// One crop per calibration sample, in the order the samples were added.
    crops: Vec<LibrarySample>,
    /// Verdict of the last sample the user or the scan offered.
    last_verdict: Option<SampleVerdict>,
    /// Selections refused as calibration but kept as detection templates.
    template_only: usize,
    /// Occurrences the last scan accepted for this mark.
    occurrences: usize,
    /// Of those, the ones only correlation vouched for — never removable.
    unverified: usize,
    /// Mean detection gain and t-statistic of the last scan: the honest model-quality
    /// numbers (the recomposition residual is NOT one).
    gain_mean: f32,
    snr_mean: f32,
    /// Background luma under the accepted occurrences, binned.
    histogram: [u32; CHAPTER_HISTOGRAM_BINS],
    /// Page width the mark was measured on, for the library's search metadata.
    page_width: u32,
    /// Library entry this mark was loaded from or last saved to.
    library_entry: Option<String>,
    /// Library entries the last scan found carrying this same mark, best first. Empty until
    /// a scan has measured a signature to match on.
    matches: Vec<LibraryCandidate>,
    /// Entry currently SUPPLYING this mark's calibration, when the user (or the scan) chose
    /// one. `None` means the mark is calibrated from the chapter's own samples.
    adopted_entry: Option<String>,
    /// The user's EXPLICIT choice, which survives a rescan. `adopted_entry` is what is in
    /// effect right now and is rebuilt by every scan; this is the instruction that rebuild
    /// obeys, so an override is not quietly undone by pressing «Найти по главе» again.
    pinned_entry: Option<String>,
    /// The chapter's own calibration, parked while an entry is adopted, so dropping the
    /// entry restores what the chapter measured instead of losing it.
    parked: Option<ParkedCalibration>,
    /// What the fit was told about the peak opacity when the samples cannot pin it.
    alpha_assumption: StoredAlphaAssumption,
    preview: Option<ChapterPreview>,
    /// Bumped whenever `preview` is replaced, so the GUI knows to re-upload.
    preview_revision: u64,
}

/// The catalog of one chapter: the engine kinds plus their presentation halves.
///
/// The two vectors are INDEX-ALIGNED and must be mutated together. They are separate
/// because `find_matching_kind` — the only sanctioned way to decide whether a new sample
/// belongs to a known mark — takes a `&[WatermarkKind]` slice.
#[derive(Debug, Clone, Default)]
struct ChapterCatalog {
    kinds: Vec<WatermarkKind>,
    marks: Vec<ChapterMark>,
}

/// One accepted occurrence, attributed to a mark by its literal id (indices would rot if
/// the catalog changed between the scan and the apply).
#[derive(Debug, Clone)]
struct ChapterHitRecord {
    mark_id: String,
    occurrence: Occurrence,
}

/// Everything one page contributed to the last scan.
#[derive(Debug, Clone)]
struct ChapterPageHits {
    page_idx: usize,
    hits: Vec<ChapterHitRecord>,
}

/// Chapter-wide totals of the last scan.
#[derive(Debug, Clone, Copy, Default)]
struct ChapterScanReport {
    pages: usize,
    found: usize,
    unverified: usize,
    /// Marks whose calibration came from a library entry instead of this chapter.
    matched: usize,
}

/// Chapter-wide totals of the last apply.
#[derive(Debug, Clone, Copy, Default)]
struct ChapterApplyReport {
    pages: usize,
    removed: usize,
    /// Occurrences refused because only correlation vouched for them, or because their
    /// mark has no model. Subtracting those would INJECT an inverse mark.
    refused: usize,
    /// Patches the canvas would not accept (a page whose overlay could not be opened).
    failed_patches: usize,
    residual: RemovalResidual,
}

/// Recovered pixels of one occurrence, in DECODED page coordinates.
#[derive(Debug, Clone)]
struct ChapterPatch {
    rect: PixelRect,
    image: egui::ColorImage,
}

/// One page's worth of prepared patches, handed to the GUI thread to apply.
#[derive(Debug, Clone)]
struct ChapterApplyBatch {
    page_idx: usize,
    /// Decoded size of that page, the space `ChapterPatch::rect` lives in.
    page_size: [usize; 2],
    /// Overlay size the canvas reported for the page, when it knew one.
    overlay_size: Option<[usize; 2]>,
    patches: Vec<ChapterPatch>,
    removed: usize,
    refused: usize,
    residual: RemovalResidual,
}

/// What the chapter UI asked the tool to start.
///
/// The editor body draws inside a closure that already borrows the canvas, so anything
/// needing the project or the canvas is handed out as a request and started afterwards.
enum ChapterRequest {
    /// Add the editor's current selection as a calibration sample. `target` is `None` for
    /// «Добавить знак», which may still be absorbed by an identical known mark.
    Sample {
        page_idx: usize,
        rect: OverlayRectPx,
        target: Option<usize>,
    },
    Scan,
    Apply,
    SaveLibrary(usize),
    LoadLibrary(String),
    RefreshLibrary,
    /// Override the automatic library match of one mark: `entry` names the entry that
    /// should supply its calibration, or `None` to go back to the chapter's own samples.
    UseLibraryMatch {
        index: usize,
        entry: Option<String>,
    },
    /// Show or hide the «Библиотека знаков» dock panel.
    ToggleLibraryPanel,
}

/// Which chapter job is in flight. Used for the status line and to keep the catalog UI
/// read-only while a worker owns a copy of the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChapterJobLabel {
    Sample,
    Scan,
    Apply,
    Library,
}

/// Messages a chapter worker sends back.
enum ChapterEvent {
    Progress {
        done: usize,
        total: usize,
        label: String,
    },
    SampleDone(Box<ChapterSampleOutcome>),
    ScanDone(Box<ChapterScanOutcome>),
    ApplyBatch(Box<ChapterApplyBatch>),
    ApplyDone(Box<ChapterApplyReport>),
    LibraryList(Vec<EntrySummary>),
    LibrarySaved {
        mark_id: String,
        entry_id: String,
    },
    LibraryLoaded(Box<ChapterLoadOutcome>),
    Failed(String),
}

/// Result of adding one calibration sample (or creating a mark from one).
#[derive(Debug)]
struct ChapterSampleOutcome {
    catalog: ChapterCatalog,
    /// Index of the mark the sample landed on.
    selected: usize,
    /// Localized report of what happened, already composed by the worker.
    status: String,
}

/// Result of a chapter scan.
struct ChapterScanOutcome {
    catalog: ChapterCatalog,
    hits: Vec<ChapterPageHits>,
    report: ChapterScanReport,
}

/// A library entry turned back into a catalog mark.
struct ChapterLoadOutcome {
    kind: WatermarkKind,
    mark: ChapterMark,
    status: String,
}

/// The chapter mode's whole state.
#[derive(Default)]
struct ChapterState {
    catalog: ChapterCatalog,
    /// Index of the mark new samples are added to.
    selected: usize,
    hits: Vec<ChapterPageHits>,
    scan_report: Option<ChapterScanReport>,
    apply_report: Option<ChapterApplyReport>,
    rx: Option<Receiver<ChapterEvent>>,
    job: Option<ChapterJobLabel>,
    progress: Option<(usize, usize, String)>,
    status: Option<String>,
    library: Vec<EntrySummary>,
    /// Set when the on-disk library list is known to be stale.
    library_requested: bool,
    /// Cleared until the first listing answered, so the picker fills itself once.
    library_loaded: bool,
    /// Number handed to the next automatically named mark.
    next_mark_number: usize,
    textures: HashMap<String, ChapterPreviewTextures>,
}

/// Everything the library capture worker needs to cut one armed selection out of its page.
struct LibraryCaptureRequest {
    page: ChapterPageTask,
    /// Selection in OVERLAY coordinates, as the region editor reports it.
    rect: OverlayRectPx,
    sample_params: SampleParams,
    max_side: u32,
}

/// One cut-out crop: the rectangle to cut, and how much background it actually carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GrownCrop {
    /// The rectangle to cut out of the page, footprint plus whatever ring fitted.
    outer: PixelRect,
    /// Background present on each side of the footprint inside `outer`. Equal to the requested
    /// margin on every side unless the page border took some of it.
    margins: CropMargins,
}

/// Name prefix every session scratch crop shares, so the sweep can recognise its own files
/// and touch nothing else in the system temp directory.
const SCRATCH_CROP_PREFIX: &str = "manhwastudio-wm-capture-";

/// How long a scratch crop left by ANOTHER process is kept before the sweep removes it.
///
/// A second instance of the program may legitimately be holding crops of its own, and there
/// is no portable way to ask whether a recorded process id is still alive, so age is the
/// guard: a day is far longer than any editing session that still remembers the crop, and
/// the crop was never anything but session state.
const SCRATCH_CROP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Request of the "add mark" / "add sample" worker.
struct ChapterSampleRequest {
    catalog: ChapterCatalog,
    /// `Some(index)` adds to that mark; `None` offers the selection as a new mark, which
    /// an identical known mark may still absorb.
    target: Option<usize>,
    page: ChapterPageTask,
    /// Selection in OVERLAY coordinates, as the region editor reports it.
    rect: OverlayRectPx,
    settings: WatermarkRemovalSettings,
    /// Number for the automatic name of a newly created mark.
    next_number: usize,
}

/// Request of the chapter scan.
struct ChapterScanRequest {
    catalog: ChapterCatalog,
    pages: Vec<ChapterPageTask>,
    settings: WatermarkRemovalSettings,
}

/// Request of the chapter apply.
struct ChapterApplyRequest {
    catalog: ChapterCatalog,
    pages: Vec<ChapterPageTask>,
    hits: Vec<ChapterPageHits>,
}

/// Request of the "use this library entry for this mark" worker.
struct ChapterMatchRequest {
    catalog: ChapterCatalog,
    index: usize,
    /// `Some(entry_id)` adopts that entry; `None` restores the chapter's own calibration.
    entry: Option<String>,
}

/// The «Удаление водяных знаков» cleaning tool.
pub struct WatermarkRemovalTool {
    region_base: RegionEditToolBase,
    session: WatermarkSessionState,
    settings: WatermarkRemovalSettings,
    settings_rx: Option<Receiver<WatermarkRemovalSettings>>,
    settings_loaded: bool,
    dirty: bool,
    save_rx: Option<Receiver<()>>,
    /// Catalog download state; `None` until the first `watermark.status` answer.
    status: Option<WatermarkStatus>,
    status_rx: Option<Receiver<Result<WatermarkStatus, String>>>,
    /// Arms exactly ONE `watermark.status` query: set initially and re-armed after
    /// a run that may have downloaded code or weights, cleared when the query is
    /// spawned. Without it a failing query would spawn a thread per frame.
    status_wanted: bool,
    unload_rx: Option<Receiver<Result<(), String>>>,
    unload_status: Option<String>,
    progress: Arc<Mutex<WatermarkProgress>>,
    ai_backend_available: bool,
    /// Whether the backend reports a usable Torch runtime, pushed by the tab every frame.
    ///
    /// The tool's own Torch gate lives here rather than on the picker button: the two network
    /// modes need it, the chapter mode does not. An UNKNOWN answer arrives as `true` — the tab
    /// resolves `is_torch_available.unwrap_or(true)` — which is the same optimistic treatment
    /// the shared mask-source gating uses: a UI locked because a health snapshot has not
    /// arrived yet is worse than a named backend error at run time.
    ai_backend_torch_available: bool,
    /// Everything the local chapter mode owns. Survives closing the region editor, so a
    /// chapter-wide apply keeps running (and keeps reporting) after the window is gone.
    chapter: ChapterState,
    /// The «Библиотека знаков» dock panel's state. Tool-owned and independent of the region
    /// editor, so it stays usable while a chapter job runs.
    library_window: WatermarkLibraryWindow,
    /// What the next completed canvas selection is reserved for, when the library panel
    /// reserved it. `Option` by design: exactly one reservation can be live, so arming a
    /// second entry necessarily disarms the first.
    library_arm: Option<LibraryArm>,
    /// What the library panel asked for while it drew, raised inside `CanvasView::draw` and
    /// consumed later in the SAME frame by `draw_overlay_ui`.
    library_panel_request: Option<LibraryPanelRequest>,
    /// The armed selection being cut out of its page. Its own lane on purpose: the library
    /// panel's channel holds one job at a time and would make a capture wait behind an icon
    /// batch, while the chapter channel would make it wait behind a chapter-wide scan.
    library_capture_rx: Option<Receiver<Result<CapturedCrop, LibraryCaptureRefusal>>>,
    /// The reservation the in-flight capture was started for, so a crop that comes back after
    /// the user re-armed still lands where it was promised.
    library_capture_arm: Option<LibraryArm>,
    /// Whether the session offers the series watermark library
    /// (`SessionCapabilities::series_watermark_library`), pushed by the tab every frame before the
    /// dock bodies draw. `false` disables the chapter mode (`effective_watermark_mode`).
    series_watermark_library: bool,
}

impl Default for WatermarkRemovalTool {
    fn default() -> Self {
        let mut tool = Self {
            // The backend reflect-pads the region to a square multiple of 16
            // itself (plan §7.2), so the selection needs no forced multiple.
            region_base: RegionEditToolBase::new("watermark_removal", None),
            session: WatermarkSessionState::default(),
            settings: WatermarkRemovalSettings::default(),
            settings_rx: None,
            settings_loaded: false,
            dirty: false,
            save_rx: None,
            status: None,
            status_rx: None,
            status_wanted: true,
            unload_rx: None,
            unload_status: None,
            progress: Arc::new(Mutex::new(WatermarkProgress::default())),
            ai_backend_available: false,
            ai_backend_torch_available: false,
            chapter: ChapterState::default(),
            library_window: WatermarkLibraryWindow::default(),
            library_arm: None,
            library_panel_request: None,
            library_capture_rx: None,
            library_capture_arm: None,
            series_watermark_library: true,
        };
        tool.request_settings_load();
        // Once per process, and off the GUI thread: the scratch crops of a run that was killed
        // before the panel could clean up would otherwise accumulate in the temp directory for
        // the life of the machine.
        static SWEEP: std::sync::Once = std::sync::Once::new();
        SWEEP.call_once(|| {
            thread::spawn(sweep_stale_scratch_crops);
        });
        tool
    }
}

/// Everything the editor body may mutate, borrowed for exactly one frame.
///
/// It exists so the body can be split into small methods instead of one closure
/// with a dozen captured references.
struct WatermarkEditorCtx<'a> {
    session: &'a mut WatermarkSessionState,
    settings: &'a mut WatermarkRemovalSettings,
    status: Option<&'a WatermarkStatus>,
    unload_status: &'a mut Option<String>,
    progress: &'a Arc<Mutex<WatermarkProgress>>,
    ai_backend_available: bool,
    /// Whether the backend reports a usable Torch runtime. Gates the two network entries of
    /// the mode picker and the run button; an unknown answer arrives as `true`.
    ai_backend_torch_available: bool,
    /// Whether the «Библиотека знаков» dock panel is shown, so its button can draw pressed.
    library_panel_open: bool,
    /// Whether the session offers the series watermark library (else no chapter mode).
    series_watermark_library: bool,
    /// The chapter mode's whole state. Selection, renaming and deletion happen here
    /// directly; anything that needs a worker leaves through `chapter_request`.
    chapter: &'a mut ChapterState,
    /// Set when a control changed a persisted value.
    settings_changed: &'a mut bool,
    /// Set when the catalog download state should be re-queried.
    want_status: &'a mut bool,
    /// Set when the user asked to unload the backend model.
    unload_requested: &'a mut bool,
    /// At most one chapter job request per frame.
    chapter_request: &'a mut Option<ChapterRequest>,
}

#[cfg(test)]
mod tests;
