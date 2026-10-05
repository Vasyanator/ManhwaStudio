/*
FILE HEADER (cleaning/tools/watermark_removal.rs)

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
  The GUI-free engine in `../watermark_chapter.rs` solves the compositing
  equation `I = c + s*B` from occurrences observed over different backgrounds and
  removes by division. This layer owns the catalog of marks, the calibration
  samples, the background jobs, the reports, the overlay patches and the on-disk
  library (`watermark_library.rs`); it owns no maths.

The Torch requirement lives INSIDE the tool: `pytorch_required()` is a constant
`false`, so the picker button is never AI-gated and the chapter mode stays
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
START: while armed a plain ЛКМ drag selects as well as Shift+ЛКМ, which is what the
panel's own `library_arm_gesture_hint` tells the user. Every refusal of the capture lane — its own early returns and every error of
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
- A single-image session (`set_single_image_session`) has no chapter mode: every
  mode read goes through `effective_watermark_mode` (persisted «По главе» runs as
  `MaskOnly` for the session, the stored value is rewritten only by a real pick),
  the picker offers `offered_watermark_modes`, and `start_chapter_request` refuses —
  `chapter_source_ref` would otherwise key the GLOBAL library by the scratch root.
- Honest reporting (plan, "Corrections from the second implementation round"): the
  UI says the IMPRINT is measured exactly, never that «c точен»; the stated ±%
  bounds the alpha SCALE only; the exact/clipped shares are labelled as a
  quantization-and-clipping report, and model quality is reported by the detection
  gain and t-statistic instead.
*/
use super::base::{
    CleaningTool, RegionEditToolBase, RegionEditorSession, StrokePoint, build_tinted_mask_preview,
};
use super::mask_generation::{
    DEFAULT_WATERMARK_MODEL, WATERMARK_DETECT_DOWNSCALE_TO, WatermarkProgress, WatermarkStatus,
    draw_watermark_model_picker_ui, draw_watermark_progress_ui, lock_watermark_progress,
    map_watermark_call_error, poll_watermark_status, spawn_watermark_status_query,
    watermark_model_spec,
};
use super::watermark_entry::{
    CropMargins, LibraryCandidate, ReferenceRefusal, StoredEntryIdentity,
    alpha_assumption_from_stored, candidate_improves, engine_background, luma_of_level,
    rank_library_candidates, rebuild_stored_kind, reference_crop_margin_px,
    save_request_from_kind, stored_flat_background,
};
use super::watermark_library::{
    EntrySummary, LibrarySample, LoadedEntry, SaveEntryRequest, StoredAlphaAssumption,
    StoredSampleOrigin, StoredSourceRef, list_entries, load_entry, save_entry,
};
use super::watermark_library_window::{
    ARM_CANCEL_RGB, CapturedCrop, LibraryArm, LibraryCaptureRefusal, LibraryPanelContext,
    LibraryPanelRequest,
    WatermarkLibraryWindow, remove_scratch_crop, thumbnail as library_window_thumbnail,
};
use ms_backend_ipc as backend_ipc;
use ms_canvas::{CanvasView, OverlayRectPx};
use ms_config as config;
use ms_project::ProjectData;
use crate::watermark_chapter::{
    AcceptanceEvidence, AlphaSource, AlphaUncertainty, CalibrationSample, DetectionParams,
    MarkSignature, MarkTemplate, ModelConditioning, ModelFitError, Occurrence, PixelRect,
    RemovalResidual, SampleParams, SampleRejection, SampleVerdict,
    SuggestedBackground, WatermarkKind, WatermarkModel, alpha_blend_operator,
    calibration_sample_from_page, discover_anchors, find_matching_kind, find_occurrences,
    remove_occurrences_on_page, scan_page, validate_calibration_sample,
};
use ms_tab_translation::backend_health::ai_backend_offline_error;
use super::region_png::{decode_mask_png, encode_color_image_png_rgba};
use ms_widgets::{WheelComboBox, WheelSlider};
use eframe::egui;
use egui::{Color32, Pos2, Rect, TextureHandle, TextureOptions};
use image::RgbaImage;
use ms_thread as thread;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use web_time::Duration;

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

impl WatermarkMode {
    /// Persisted/wire value of the mode.
    fn wire(self) -> &'static str {
        match self {
            Self::MaskOnly => "mask_only",
            Self::Clean => "clean",
            Self::Chapter => "chapter",
        }
    }

    /// Parses a persisted value, falling back to the safe mask-only default for
    /// anything unknown (a settings file from a newer build must not break).
    fn from_wire(value: &str) -> Self {
        match value.trim() {
            "clean" => Self::Clean,
            "chapter" => Self::Chapter,
            _ => Self::MaskOnly,
        }
    }

    /// Localized name of the mode as shown in the mode picker.
    fn label(self) -> &'static str {
        match self {
            Self::MaskOnly => t!("cleaning.tools.watermark.mode_mask_only"),
            Self::Clean => t!("cleaning.tools.watermark.mode_clean"),
            Self::Chapter => t!("cleaning.tools.watermark.chapter.mode_chapter"),
        }
    }

    /// The backend-facing mode, or `None` for the local chapter engine.
    ///
    /// Every path that talks to the Python backend goes through this, so "which IPC
    /// method does the chapter mode call" cannot be asked: the type says it calls none.
    fn network(self) -> Option<WatermarkNetworkMode> {
        match self {
            Self::MaskOnly => Some(WatermarkNetworkMode::MaskOnly),
            Self::Clean => Some(WatermarkNetworkMode::Clean),
            Self::Chapter => None,
        }
    }

    /// Whether selecting this mode requires a working Torch runtime.
    ///
    /// The chapter mode is deliberately AI-free. This gates the two NETWORK entries of the
    /// mode picker and the run button INSIDE the tool; it deliberately does NOT reach
    /// `CleaningTool::pytorch_required`, which is a constant `false` so the picker button —
    /// and `ensure_active_tool_available` behind it — can never make the chapter mode
    /// unreachable on a machine without Torch.
    fn requires_torch(self) -> bool {
        self.network().is_some()
    }
}

/// The mode the tool runs in this session: the persisted one, except that a single-image
/// session never runs `Chapter`.
///
/// The chapter mode keys the GLOBAL watermark library by the series folder
/// (`chapter_source_ref`: the chapter directory's parent), which in a single-image session is
/// the throwaway scratch root — every run would file its measurements under a meaningless,
/// per-session source key. It falls back to the safe default `MaskOnly` for the session only;
/// the persisted value is never rewritten by this (`draw_mode_picker` writes only a real pick).
fn effective_watermark_mode(persisted: &str, single_image: bool) -> WatermarkMode {
    match WatermarkMode::from_wire(persisted) {
        WatermarkMode::Chapter if single_image => WatermarkMode::MaskOnly,
        mode => mode,
    }
}

/// The modes the picker offers: all three in a project, the two network modes in a
/// single-image session (see `effective_watermark_mode`).
fn offered_watermark_modes(single_image: bool) -> &'static [WatermarkMode] {
    if single_image {
        &[WatermarkMode::MaskOnly, WatermarkMode::Clean]
    } else {
        &[WatermarkMode::MaskOnly, WatermarkMode::Clean, WatermarkMode::Chapter]
    }
}

/// The two modes that actually reach the Python backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatermarkNetworkMode {
    MaskOnly,
    Clean,
}

impl WatermarkNetworkMode {
    /// Localized caption of the run button, which states what the run will do.
    fn run_button_label(self) -> &'static str {
        match self {
            Self::MaskOnly => t!("cleaning.tools.watermark.detect_button"),
            Self::Clean => t!("cleaning.tools.watermark.clean_button"),
        }
    }

    /// IPC method the mode calls.
    fn ipc_method(self) -> &'static str {
        match self {
            Self::MaskOnly => backend_ipc::protocol::METHOD_WATERMARK_DETECT,
            Self::Clean => backend_ipc::protocol::METHOD_WATERMARK_REMOVE,
        }
    }
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

impl Default for WatermarkRemovalSettings {
    fn default() -> Self {
        let detection = DetectionParams::default();
        let sample = SampleParams::default();
        Self {
            mode: WatermarkMode::MaskOnly.wire().to_string(),
            model: DEFAULT_WATERMARK_MODEL.to_string(),
            tile: 512,
            overlap: 64,
            threshold: 0.5,
            dilate_px: 4,
            show_mask_preview: true,
            // The engine's own measured defaults, so the settings file starts out
            // agreeing with the constants the plan justifies.
            chapter_anchor_tolerance_px: detection.anchor_tolerance,
            chapter_background_blur_px: detection.background_blur_radius,
            chapter_ring_width_px: sample.ring_width,
        }
    }
}

impl WatermarkRemovalSettings {
    /// Returns a copy with every field forced into its supported range: the mode
    /// and model fall back to the defaults when unknown, the tile is snapped down
    /// to a multiple of 16 inside `[256, 1024]`, the overlap is capped at half the
    /// tile, a non-finite threshold falls back to the default, and the dilation is
    /// capped. This is the ONLY value ever put on the wire.
    #[must_use]
    fn normalized(&self) -> Self {
        let tile = (self.tile.clamp(WATERMARK_TILE_MIN, WATERMARK_TILE_MAX)
            / WATERMARK_TILE_MULTIPLE)
            * WATERMARK_TILE_MULTIPLE;
        let tile = tile.max(WATERMARK_TILE_MIN);
        let threshold = if self.threshold.is_finite() {
            self.threshold
                .clamp(WATERMARK_THRESHOLD_MIN, WATERMARK_THRESHOLD_MAX)
        } else {
            Self::default().threshold
        };
        // The chapter parameters are normalized by the engine itself, which is where the
        // measured hard bounds live; running them through it here keeps the settings file
        // and the values actually used identical.
        let detection = DetectionParams {
            anchor_tolerance: self.chapter_anchor_tolerance_px,
            background_blur_radius: self.chapter_background_blur_px,
            ..DetectionParams::default()
        }
        .normalized();
        let sample = SampleParams {
            ring_width: self.chapter_ring_width_px,
            ..SampleParams::default()
        }
        .normalized();
        Self {
            mode: WatermarkMode::from_wire(&self.mode).wire().to_string(),
            model: watermark_model_spec(&self.model).id.to_string(),
            tile,
            overlap: self.overlap.min(WATERMARK_OVERLAP_MAX).min(tile / 2),
            threshold,
            dilate_px: self.dilate_px.min(WATERMARK_DILATE_MAX),
            show_mask_preview: self.show_mask_preview,
            chapter_anchor_tolerance_px: detection.anchor_tolerance,
            chapter_background_blur_px: detection.background_blur_radius,
            chapter_ring_width_px: sample.ring_width,
        }
    }

    /// Detection parameters of the chapter engine, already normalized.
    fn chapter_detection_params(&self) -> DetectionParams {
        DetectionParams {
            anchor_tolerance: self.chapter_anchor_tolerance_px,
            background_blur_radius: self.chapter_background_blur_px,
            ..DetectionParams::default()
        }
        .normalized()
    }

    /// Calibration-sample parameters of the chapter engine, already normalized.
    fn chapter_sample_params(&self) -> SampleParams {
        SampleParams {
            ring_width: self.chapter_ring_width_px,
            ..SampleParams::default()
        }
        .normalized()
    }
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

impl WatermarkSessionState {
    /// Drops everything tied to a previous region when the editor opened a new one.
    fn sync_session(&mut self, scroll_id: u64) {
        if self.scroll_id == Some(scroll_id) {
            return;
        }
        self.scroll_id = Some(scroll_id);
        self.undo_stack.clear();
        self.mask_preview = None;
        self.mask_texture = None;
        // Dropping the receiver detaches the in-flight worker: its result is
        // discarded instead of landing on the new region.
        self.run_rx = None;
    }

    /// Clears the whole session (Escape, tool deactivation, editor closed).
    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Starts a run on a worker thread. A second run is refused while one is in
    /// flight; the status line explains that.
    fn start_run(
        &mut self,
        editor: &mut RegionEditorSession,
        settings: &WatermarkRemovalSettings,
        progress: &Arc<Mutex<WatermarkProgress>>,
        mode: WatermarkNetworkMode,
    ) {
        if self.run_rx.is_some() {
            editor.status =
                Some(t!("cleaning.mask_editor.processing_already_running_status").to_string());
            return;
        }
        let image = editor.image.clone();
        let settings = settings.normalized();
        let progress = Arc::clone(progress);
        let (tx, rx) = mpsc::channel::<WatermarkJobResult>();
        thread::spawn(move || {
            let result = run_watermark(&image, &settings, &progress, mode);
            let _ = tx.send(WatermarkJobResult {
                source: image,
                result,
            });
        });
        self.run_rx = Some(rx);
        editor.status = Some(t!("cleaning.mask_editor.processing_background_status").to_string());
    }

    /// Polls the run channel and applies a finished run. Returns `true` while a
    /// run is still in flight.
    fn poll_run(&mut self, editor: &mut RegionEditorSession) -> bool {
        let Some(rx) = self.run_rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok(job) => {
                self.run_rx = None;
                match job.result {
                    Ok(outcome) => {
                        if let Some(image) = outcome.image {
                            // Only a run that actually replaced pixels becomes an
                            // undo entry; a mask-only run has nothing to revert.
                            self.undo_stack.push(job.source);
                            editor.image = image;
                            editor.texture_dirty = true;
                            editor.status =
                                Some(t!("cleaning.mask_editor.processing_done_status").to_string());
                        } else {
                            editor.status = Some(
                                t!("cleaning.tools.watermark.mask_ready_status").to_string(),
                            );
                        }
                        self.set_mask_preview(&outcome.mask);
                    }
                    Err(err) => {
                        editor.status =
                            Some(tf!("cleaning.mask_editor.processing_error", err = err));
                    }
                }
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => {
                self.run_rx = None;
                editor.status =
                    Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
                false
            }
        }
    }

    /// Restores the region image from before the last applied run.
    fn undo_last_run(&mut self, editor: &mut RegionEditorSession) {
        let Some(image) = self.undo_stack.pop() else {
            editor.status = Some(t!("cleaning.mask_editor.no_state_for_undo_status").to_string());
            return;
        };
        editor.image = image;
        editor.texture_dirty = true;
        editor.status = Some(t!("cleaning.mask_editor.reverted_status").to_string());
    }

    /// Replaces the mask overlay with a tinted copy of `mask` and invalidates its
    /// texture (a new upload is cheaper and simpler than patching in place).
    fn set_mask_preview(&mut self, mask: &egui::ColorImage) {
        self.mask_preview = Some(build_tinted_mask_preview(mask, WATERMARK_MASK_PREVIEW_RGB));
        self.mask_texture = None;
    }

    /// Uploads the mask overlay texture if one is missing.
    fn ensure_mask_texture(&mut self, ctx: &egui::Context, scroll_id: u64) {
        let Some(preview) = self.mask_preview.as_ref() else {
            self.mask_texture = None;
            return;
        };
        if self.mask_texture.is_some() {
            return;
        }
        self.mask_texture = Some(ctx.load_texture(
            format!("cleaning-watermark-mask-{scroll_id}"),
            preview.clone(),
            TextureOptions::LINEAR,
        ));
    }
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

impl ChapterMark {
    /// A mark with no samples yet.
    fn new(name: String, template_crop: RgbaImage, page_width: u32) -> Self {
        Self {
            name,
            template_crop,
            crops: Vec::new(),
            last_verdict: None,
            template_only: 0,
            occurrences: 0,
            unverified: 0,
            gain_mean: 0.0,
            snr_mean: 0.0,
            histogram: [0; CHAPTER_HISTOGRAM_BINS],
            page_width,
            library_entry: None,
            matches: Vec::new(),
            adopted_entry: None,
            pinned_entry: None,
            parked: None,
            alpha_assumption: StoredAlphaAssumption::FromDeposit,
            preview: None,
            preview_revision: 0,
        }
    }

    /// Clears the per-scan counters before a new scan fills them in.
    fn reset_scan_stats(&mut self) {
        self.occurrences = 0;
        self.unverified = 0;
        self.gain_mean = 0.0;
        self.snr_mean = 0.0;
        self.histogram = [0; CHAPTER_HISTOGRAM_BINS];
    }
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

impl ChapterCatalog {
    fn len(&self) -> usize {
        self.kinds.len().min(self.marks.len())
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends one mark and its kind, keeping the alignment.
    fn push(&mut self, kind: WatermarkKind, mark: ChapterMark) {
        self.kinds.push(kind);
        self.marks.push(mark);
    }

    /// Removes one mark and its kind.
    fn remove(&mut self, index: usize) {
        if index >= self.len() {
            return;
        }
        self.kinds.remove(index);
        self.marks.remove(index);
    }

    /// Index of the mark with this literal id.
    fn index_of(&self, mark_id: &str) -> Option<usize> {
        self.kinds.iter().position(|kind| kind.id() == mark_id)
    }
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

impl ChapterState {
    /// True while a worker owns a copy of the catalog.
    fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Drops everything that describes the previous chapter scan. Called whenever the
    /// catalog changes, because hits reference marks and models that no longer exist.
    fn invalidate_scan(&mut self) {
        self.hits.clear();
        self.scan_report = None;
        self.apply_report = None;
    }

    /// Starts a worker, refusing a second job while one is in flight.
    fn start_job<F>(&mut self, label: ChapterJobLabel, work: F) -> bool
    where
        F: FnOnce(&Sender<ChapterEvent>) + Send + 'static,
    {
        if self.busy() {
            self.status =
                Some(t!("cleaning.mask_editor.processing_already_running_status").to_string());
            return false;
        }
        let (tx, rx) = mpsc::channel::<ChapterEvent>();
        self.rx = Some(rx);
        self.job = Some(label);
        self.progress = None;
        thread::spawn(move || {
            work(&tx);
        });
        true
    }
}

/// Decodes one chapter page into RGBA8.
///
/// # Errors
/// A user-facing message naming the file when the image cannot be opened or decoded.
fn decode_chapter_page(path: &Path) -> Result<RgbaImage, String> {
    image::open(path)
        .map(|image| image.to_rgba8())
        .map_err(|err| {
            tf!(
                "cleaning.region.open_page_error",
                page_path = path.display(),
                err = err
            )
        })
}

/// Wraps an engine failure into the tool's user-facing message.
fn chapter_engine_error(err: impl std::fmt::Display) -> String {
    tf!("cleaning.tools.watermark.chapter.engine_error", err = err)
}

/// Maps a value from one pixel grid onto another, proportionally.
///
/// Both grids describe the same page, so the ratio is exact whenever the two sizes are
/// equal (the normal case: an overlay is created at the page's own pixel size).
fn scale_axis(value: u64, from: u64, to: u64) -> u64 {
    if from == 0 {
        return 0;
    }
    (value * to + from / 2) / from
}

/// Maps an overlay-space rect onto the decoded page's own pixel grid.
///
/// Returns `None` for a degenerate rect or one that lands outside the page.
fn overlay_rect_to_page(
    rect: OverlayRectPx,
    overlay_size: [usize; 2],
    page_width: u32,
    page_height: u32,
) -> Option<PixelRect> {
    let (ow, oh) = (overlay_size[0] as u64, overlay_size[1] as u64);
    let (pw, ph) = (u64::from(page_width), u64::from(page_height));
    if ow == 0 || oh == 0 || pw == 0 || ph == 0 {
        return None;
    }
    let x0 = scale_axis(rect.x as u64, ow, pw).min(pw);
    let y0 = scale_axis(rect.y as u64, oh, ph).min(ph);
    let x1 = scale_axis((rect.x + rect.w) as u64, ow, pw).min(pw);
    let y1 = scale_axis((rect.y + rect.h) as u64, oh, ph).min(ph);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(PixelRect::new(
        u32::try_from(x0).ok()?,
        u32::try_from(y0).ok()?,
        u32::try_from(x1 - x0).ok()?,
        u32::try_from(y1 - y0).ok()?,
    ))
}

/// Maps a decoded-page rect back into overlay space.
///
/// Returns `None` for a degenerate result, so a patch is never applied to an empty rect.
fn page_rect_to_overlay(
    rect: PixelRect,
    page_size: [usize; 2],
    overlay_size: [usize; 2],
) -> Option<OverlayRectPx> {
    let (pw, ph) = (page_size[0] as u64, page_size[1] as u64);
    let (ow, oh) = (overlay_size[0] as u64, overlay_size[1] as u64);
    if pw == 0 || ph == 0 || ow == 0 || oh == 0 {
        return None;
    }
    let x0 = scale_axis(u64::from(rect.x), pw, ow).min(ow);
    let y0 = scale_axis(u64::from(rect.y), ph, oh).min(oh);
    let x1 = scale_axis(rect.right(), pw, ow).min(ow);
    let y1 = scale_axis(rect.bottom(), ph, oh).min(oh);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(OverlayRectPx {
        x: usize::try_from(x0).ok()?,
        y: usize::try_from(y0).ok()?,
        w: usize::try_from(x1 - x0).ok()?,
        h: usize::try_from(y1 - y0).ok()?,
    })
}

/// Cuts `rect` out of `page`. The rect must already be validated against the page.
fn crop_page(page: &RgbaImage, rect: PixelRect) -> RgbaImage {
    image::imageops::crop_imm(page, rect.x, rect.y, rect.width, rect.height).to_image()
}

/// Histogram bin of a background luma.
fn chapter_histogram_bin(luma: f32) -> usize {
    if !luma.is_finite() || luma <= 0.0 {
        return 0;
    }
    // Cast justification: the quotient is clamped into `0..CHAPTER_HISTOGRAM_BINS`, a
    // single-digit range.
    let bin = (luma / 256.0 * CHAPTER_HISTOGRAM_BINS as f32) as usize;
    bin.min(CHAPTER_HISTOGRAM_BINS - 1)
}

/// Refits a kind, treating a graded refusal as the normal state it is.
///
/// A refusal stores its verdict in the kind and drops the model, which is exactly what the
/// UI reports; only an invalid input is a real failure.
///
/// # Errors
/// A user-facing message for [`ModelFitError::Invalid`].
fn refit_chapter_kind(kind: &mut WatermarkKind) -> Result<(), String> {
    match kind.refit() {
        Ok(()) | Err(ModelFitError::Refused(_)) => Ok(()),
        Err(ModelFitError::Invalid(err)) => Err(chapter_engine_error(err)),
    }
}

/// Builds the alpha-map and imprint previews of a fitted model.
///
/// The imprint is `W = c/alpha`, defined only where the mark is opaque enough for the
/// division to mean anything; elsewhere the preview is transparent rather than showing a
/// number the data does not contain.
fn build_chapter_preview(model: &WatermarkModel) -> ChapterPreview {
    let (width, height) = (model.width() as usize, model.height() as usize);
    let mut alpha = egui::ColorImage::filled([width, height], Color32::TRANSPARENT);
    let mut imprint = egui::ColorImage::filled([width, height], Color32::TRANSPARENT);
    let (c, s) = (model.c(), model.s());
    for pixel in 0..width * height {
        let base = pixel * 3;
        let mut peak = 0.0f32;
        let mut colour = [0u8; 3];
        for channel in 0..3 {
            let opacity = (1.0 - s[base + channel]).clamp(0.0, 1.0);
            peak = peak.max(opacity);
            // Below this the division is dominated by noise, so the mark's own colour is
            // simply not measured there.
            colour[channel] = if opacity > 0.02 {
                // Cast justification: the value is clamped to 0..=255 and rounded.
                (c[base + channel] / opacity).clamp(0.0, 255.0).round() as u8
            } else {
                0
            };
        }
        // Cast justification: `peak` is clamped to 0..=1 before scaling.
        let grey = (peak * 255.0).round() as u8;
        alpha.pixels[pixel] = Color32::from_rgb(grey, grey, grey);
        if peak > 0.02 {
            imprint.pixels[pixel] =
                Color32::from_rgba_unmultiplied(colour[0], colour[1], colour[2], 255);
        }
    }
    ChapterPreview { alpha, imprint }
}

/// Refreshes one mark's preview from its kind, bumping the revision so the GUI re-uploads.
fn refresh_chapter_preview(catalog: &mut ChapterCatalog, index: usize) {
    let Some(kind) = catalog.kinds.get(index) else {
        return;
    };
    let preview = kind.model().map(build_chapter_preview);
    let Some(mark) = catalog.marks.get_mut(index) else {
        return;
    };
    mark.preview = preview;
    mark.preview_revision = mark.preview_revision.wrapping_add(1);
}

/// The reservation left after one library-panel request is applied.
///
/// Exactly one reservation exists at a time, so arming a second purpose REPLACES the first
/// instead of adding to it: that is what makes «+ новый» and «+ Выделить новый» mutually
/// exclusive without either button knowing the other exists. Choosing which entry the chapter
/// hunts says nothing about the next canvas selection and therefore leaves it alone.
fn next_library_arm(
    current: Option<LibraryArm>,
    request: &LibraryPanelRequest,
) -> Option<LibraryArm> {
    match request {
        LibraryPanelRequest::Arm(arm) => Some(arm.clone()),
        LibraryPanelRequest::Disarm => None,
        LibraryPanelRequest::SelectEntry(_) | LibraryPanelRequest::UnselectEntry(_) => current,
    }
}

/// Everything the library capture worker needs to cut one armed selection out of its page.
struct LibraryCaptureRequest {
    page: ChapterPageTask,
    /// Selection in OVERLAY coordinates, as the region editor reports it.
    rect: OverlayRectPx,
    sample_params: SampleParams,
    max_side: u32,
}

/// Cuts the armed selection out of its page and measures the background under it.
///
/// The written crop is the selection GROWN by a measurement ring, because the reference intake
/// derives the footprint by insetting the crop by that margin again: what the user dragged is
/// the mark, and the band around it is the evidence about the background. The ring is measured
/// here as well, on the page, so the panel knows straight away whether the crop can be
/// committed or has to wait for the user to state a level — the on-disk sample format has no
/// "level unknown" state.
///
/// The ring is CLIPPED at the page border rather than demanded on all four sides. A mark
/// stamped against the edge of the image — which is where the user's marks sit — has no
/// background on that side and never will, so a symmetric demand would make such a mark
/// permanently unsamplable. The engine's own measurement has always worked this way
/// (`validate_calibration_sample` clips the ring and admits it on a pixel COUNT), so nothing is
/// widened here; what is removed is a symmetry this lane invented on top of it. The real
/// margins are carried to the intake in [`CapturedCrop::margins`] so the footprint it derives is
/// the one that was cut, and the measurement's partialness travels with the level.
///
/// The three ring outcomes are NOT the same thing: a flat ring measures the level, a ring
/// refused as flat leaves the level to the user, and a ring that could not be measured at all
/// makes the crop unusable — no stated level can license it, so it is refused here instead of
/// being offered a colour control it could never satisfy.
///
/// # Errors
/// A [`LibraryCaptureRefusal`] — an already-localized message plus the tag that says what the
/// user could DO about it — for a page that cannot be decoded, a degenerate selection, a
/// footprint above `max_side`, a selection with too little background left around it to judge
/// flatness at all, and a scratch file that cannot be written.
fn run_library_capture(request: LibraryCaptureRequest) -> Result<CapturedCrop, LibraryCaptureRefusal> {
    let LibraryCaptureRequest {
        page: page_task,
        rect,
        sample_params,
        max_side,
    } = request;
    let page = decode_chapter_page(&page_task.path)
        .map_err(LibraryCaptureRefusal::untagged)?;
    let overlay_size = page_task
        .overlay_size
        .unwrap_or([page.width() as usize, page.height() as usize]);
    let footprint = overlay_rect_to_page(rect, overlay_size, page.width(), page.height())
        .ok_or_else(|| {
            LibraryCaptureRefusal::tagged(
                t!("cleaning.region.invalid_selection_size_error").to_string(),
                ReferenceRefusal::TooSmall,
            )
        })?;
    if footprint.width > max_side || footprint.height > max_side {
        return Err(LibraryCaptureRefusal::tagged(
            tf!(
                "cleaning.tools.watermark.chapter.selection_too_large_error",
                width = footprint.width,
                height = footprint.height,
                limit = max_side
            ),
            ReferenceRefusal::TooSmall,
        ));
    }
    let margin = reference_crop_margin_px(&sample_params);
    let grown = grow_page_rect(footprint, margin, page.width(), page.height()).ok_or_else(|| {
        LibraryCaptureRefusal::tagged(
            t!("cleaning.region.invalid_selection_size_error").to_string(),
            ReferenceRefusal::TooSmall,
        )
    })?;
    let GrownCrop { outer, margins } = grown;
    let measured = match validate_calibration_sample(&page, footprint, &sample_params) {
        SampleVerdict::Calibration { level, .. } => Some(level),
        // The ring was measured and REFUSED as flat: the level is the one thing only the user
        // can supply, and the intake accepts the crop once they state it.
        SampleVerdict::TemplateOnly { .. } => None,
        // No ring could be measured at all. Stating a level cannot license this crop — the
        // intake refuses it whatever is asserted (`run_reference_intake`) — so the capture is
        // refused HERE, before a scratch file is written and before the panel offers a colour
        // control that could never lead anywhere.
        SampleVerdict::Unusable {
            reason: SampleRejection::BadRect,
        } => {
            return Err(LibraryCaptureRefusal::tagged(
                t!("cleaning.tools.watermark.chapter.sample_unusable_rect").to_string(),
                ReferenceRefusal::TooSmall,
            ));
        }
        // Too little background survived to judge flatness AT ALL. This is the floor a mark at
        // the page edge actually runs into, and it is a pixel count rather than a per-side
        // margin — so the message names the count, and names the border when that is what took
        // the pixels, instead of the generic "too close to the edge" it used to give.
        SampleVerdict::Unusable {
            reason: SampleRejection::RingTooSmall { pixels, needed },
        } => {
            let message = if margins.is_partial(margin) {
                tf!(
                    "cleaning.tools.watermark.chapter.library_capture_edge_ring_error",
                    pixels = pixels,
                    needed = needed
                )
            } else {
                tf!(
                    "cleaning.tools.watermark.chapter.sample_unusable_ring",
                    pixels = pixels,
                    needed = needed
                )
            };
            return Err(LibraryCaptureRefusal::tagged(
                message,
                ReferenceRefusal::TooSmall,
            ));
        }
    };
    let crop = image::imageops::crop_imm(&page, outer.x, outer.y, outer.width, outer.height)
        .to_image();
    let path = library_scratch_crop_path();
    crop.save(&path).map_err(|err| {
        LibraryCaptureRefusal::untagged(tf!(
            "cleaning.tools.watermark.chapter.library_image_write_error",
            path = path.display(),
            err = err
        ))
    })?;
    Ok(CapturedCrop {
        preview: library_window_thumbnail(&crop),
        path,
        measured,
        margins,
    })
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

/// Grows `rect` by up to `margin` on every side, taking what the page can give.
///
/// A side the page border truncates contributes fewer pixels — or none — instead of failing the
/// whole capture. This used to refuse, which made a mark stamped flush against the image edge
/// permanently unsamplable: the ring it needs does not exist there and never will, and the user
/// has such marks. The engine's measurement has always clipped its ring at the page and judged
/// the result on a PIXEL COUNT, so admitting a truncated ring here widens nothing; it only stops
/// this lane from demanding a symmetry the measurement never asked for. The asymmetry is not
/// hidden: [`GrownCrop::margins`] travels to the intake, which derives the footprint from it
/// instead of assuming a symmetric inset, and the ring's partialness is recorded with the level.
///
/// `None` only for a rect that is not inside the page at all, which is a bad selection rather
/// than a tight one.
fn grow_page_rect(
    rect: PixelRect,
    margin: u32,
    page_width: u32,
    page_height: u32,
) -> Option<GrownCrop> {
    if u64::from(rect.x) + u64::from(rect.width) > u64::from(page_width)
        || u64::from(rect.y) + u64::from(rect.height) > u64::from(page_height)
        || rect.width == 0
        || rect.height == 0
    {
        return None;
    }
    let left = margin.min(rect.x);
    let top = margin.min(rect.y);
    // Cast-free: both sums are bounded by the page dimensions, which the check above proved.
    let right = margin.min(page_width - rect.x - rect.width);
    let bottom = margin.min(page_height - rect.y - rect.height);
    Some(GrownCrop {
        outer: PixelRect::new(
            rect.x - left,
            rect.y - top,
            rect.width + left + right,
            rect.height + top + bottom,
        ),
        margins: CropMargins {
            left,
            top,
            right,
            bottom,
        },
    })
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

/// A fresh path for one session scratch crop.
///
/// The crops are session state by construction — a crop with no stated background cannot be
/// persisted — so they live in the system temp directory, are named per process and per crop
/// so two runs cannot collide, and are deleted by the panel when the crop is committed or
/// discarded. A run that ends before the panel got to them is what `sweep_stale_scratch_crops`
/// cleans up.
fn library_scratch_crop_path() -> PathBuf {
    static NEXT_CROP: AtomicU64 = AtomicU64::new(1);
    let index = NEXT_CROP.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{SCRATCH_CROP_PREFIX}{}-{index}.png",
        std::process::id()
    ))
}

/// True when `name` is a scratch crop that no longer belongs to anybody this sweep may assume
/// is alive: it carries the scratch prefix, it was not written by `current_pid`, and `age` has
/// passed [`SCRATCH_CROP_MAX_AGE`].
///
/// Pure so the rule can be tested without touching the filesystem. A name whose process id
/// does not parse is still one of ours by prefix and is aged out like any other; a file this
/// process wrote is NEVER swept, because its crops may still be waiting in the panel.
fn scratch_crop_is_stale(name: &str, age: Duration, current_pid: u32) -> bool {
    let Some(rest) = name.strip_prefix(SCRATCH_CROP_PREFIX) else {
        return false;
    };
    if !rest.ends_with(".png") {
        return false;
    }
    let owner = rest.split_once('-').map(|(pid, _)| pid).unwrap_or(rest);
    if owner.parse::<u32>().is_ok_and(|pid| pid == current_pid) {
        return false;
    }
    age >= SCRATCH_CROP_MAX_AGE
}

/// Removes scratch crops that earlier runs left in the system temp directory.
///
/// The panel deletes a crop when it is committed or discarded, so the only crops this finds
/// are those a run was killed on top of. Without the sweep they accumulate for the life of the
/// machine; with it the directory holds at most one session's worth plus a day's grace for a
/// concurrently running instance. Runs on a worker thread: it is directory I/O.
fn sweep_stale_scratch_crops() {
    let dir = std::env::temp_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] watermark scratch sweep could not read {}: {err}",
                dir.display()
            ));
            return;
        }
    };
    let now = std::time::SystemTime::now();
    let current_pid = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // A file whose age cannot be read counts as brand new, so an unreadable timestamp
        // never costs somebody their crop.
        let age = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or(Duration::ZERO);
        if !scratch_crop_is_stale(name, age, current_pid) {
            continue;
        }
        if let Err(err) = fs::remove_file(entry.path()) {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] watermark scratch sweep could not remove {}: {err}",
                entry.path().display()
            ));
        }
    }
}

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

/// Adds one calibration sample, creating a mark when the caller asked for a new one.
///
/// Identity is the engine's `MarkSignature`: a selection offered as a new mark is matched
/// against the catalog with `find_matching_kind` first, because a colour mark and its
/// greyscale twin can be pixel-identical in shape and must NOT be merged, while a second
/// sample of the SAME mark must not become a second entry.
///
/// # Errors
/// A user-facing message for a decode failure, a selection that is degenerate, too large,
/// or does not match the target mark's footprint, and for an engine failure.
fn run_chapter_sample(request: ChapterSampleRequest) -> Result<ChapterSampleOutcome, String> {
    let ChapterSampleRequest {
        mut catalog,
        target,
        page: page_task,
        rect,
        settings,
        next_number,
    } = request;
    let page = decode_chapter_page(&page_task.path)?;
    let overlay_size = page_task
        .overlay_size
        .unwrap_or([page.width() as usize, page.height() as usize]);
    let rect = overlay_rect_to_page(rect, overlay_size, page.width(), page.height())
        .ok_or_else(|| t!("cleaning.region.invalid_selection_size_error").to_string())?;
    if rect.width > CHAPTER_MAX_TEMPLATE_SIDE || rect.height > CHAPTER_MAX_TEMPLATE_SIDE {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.selection_too_large_error",
            width = rect.width,
            height = rect.height,
            limit = CHAPTER_MAX_TEMPLATE_SIDE
        ));
    }

    let sample_params = settings.chapter_sample_params();
    let (verdict, sample) =
        calibration_sample_from_page(&page, page_task.page_idx, rect, &sample_params)
            .map_err(chapter_engine_error)?;

    // A selection that cannot even serve as a detection template must not enter the catalog
    // at all; the verdict says why in the user's own terms.
    if !verdict.usable_as_template() {
        return Err(describe_sample_verdict(&verdict));
    }

    // Resolve which mark this belongs to. A sample with a measured signature can be
    // recognised; one without (no flat ring) has a shape but no identity and therefore
    // always starts a new mark.
    let signature = sample.as_ref().and_then(MarkSignature::from_flat_sample);
    let matched = match target {
        Some(index) if index < catalog.len() => Some(index),
        // The catalog changed under the request (the target mark was removed while the
        // button was down). Refuse rather than quietly creating a mark nobody asked for.
        Some(_) => return Err(t!("cleaning.tools.watermark.chapter.no_marks_error").to_string()),
        None => signature
            .as_ref()
            .and_then(|signature| find_matching_kind(&catalog.kinds, signature)),
    };

    let mut status = Vec::new();
    let index = match matched {
        Some(index) => {
            let template = catalog.kinds[index].template();
            if template.width() != rect.width || template.height() != rect.height {
                return Err(tf!(
                    "cleaning.tools.watermark.chapter.geometry_mismatch_error",
                    width = rect.width,
                    height = rect.height,
                    expected_width = template.width(),
                    expected_height = template.height()
                ));
            }
            if target.is_none() {
                status.push(tf!(
                    "cleaning.tools.watermark.chapter.sample_matched_existing",
                    name = catalog.marks[index].name.clone()
                ));
            }
            index
        }
        None => {
            let template =
                MarkTemplate::from_page(&page, rect).map_err(chapter_engine_error)?;
            let id = format!("mark-{next_number}");
            let name = tf!(
                "cleaning.tools.watermark.chapter.mark_default_name",
                number = next_number
            );
            let mark = ChapterMark::new(name.clone(), crop_page(&page, rect), page.width());
            catalog.push(WatermarkKind::new(id, template, alpha_blend_operator()), mark);
            status.push(tf!(
                "cleaning.tools.watermark.chapter.sample_created",
                name = name
            ));
            catalog.len() - 1
        }
    };

    if let Some(sample) = sample {
        let SampleVerdict::Calibration {
            level,
            ring_std,
            ring,
        } = verdict
        else {
            return Err(chapter_engine_error("calibration sample without a flat ring"));
        };
        catalog.kinds[index]
            .add_sample(sample)
            .map_err(chapter_engine_error)?;
        catalog.marks[index].crops.push(LibrarySample {
            image: crop_page(&page, rect),
            origin: StoredSampleOrigin::Page {
                page_index: page_task.page_idx,
                x: rect.x,
                y: rect.y,
            },
            background: stored_flat_background(level, ring_std, ring),
        });
        refit_chapter_kind(&mut catalog.kinds[index])?;
    } else if verdict.usable_as_template() {
        catalog.marks[index].template_only += 1;
    }
    status.push(describe_sample_verdict(&verdict));
    catalog.marks[index].last_verdict = Some(verdict);
    catalog.marks[index].page_width = page.width();
    refresh_chapter_preview(&mut catalog, index);

    Ok(ChapterSampleOutcome {
        catalog,
        selected: index,
        status: status.join(" "),
    })
}

/// Rebuilds one kind from a template crop and a calibration crop list.
///
/// The engine deliberately offers no way to REMOVE a sample from a `WatermarkKind` — a fit
/// is only ever grown — so swapping a mark's calibration means building the kind again.
/// The DETECTION reference stays the chapter's own template crop: only the calibration
/// changes when a library entry is adopted.
///
/// # Errors
/// A user-facing message for a template with no contrast, an empty anchor set, a crop whose
/// footprint does not match the template, or an invalid fit input.
fn rebuild_chapter_kind(
    id: &str,
    template_crop: &RgbaImage,
    anchors: &[u32],
    assumption: StoredAlphaAssumption,
    crops: &[LibrarySample],
) -> Result<WatermarkKind, String> {
    let rect = PixelRect::new(0, 0, template_crop.width(), template_crop.height());
    let template = MarkTemplate::from_page(template_crop, rect).map_err(chapter_engine_error)?;
    let mut kind = WatermarkKind::new(id.to_string(), template, alpha_blend_operator());
    if !anchors.is_empty() {
        kind.template_mut()
            .set_anchors(anchors)
            .map_err(chapter_engine_error)?;
    }
    kind.set_alpha_assumption(alpha_assumption_from_stored(assumption));
    for crop in crops {
        let crop_rect = PixelRect::new(0, 0, crop.image.width(), crop.image.height());
        let sample =
            CalibrationSample::from_page(&crop.image, 0, crop_rect, engine_background(crop.background))
                .map_err(chapter_engine_error)?;
        kind.add_sample(sample).map_err(chapter_engine_error)?;
    }
    refit_chapter_kind(&mut kind)?;
    Ok(kind)
}

/// Makes a library entry supply one mark's calibration.
///
/// The chapter's own crops are parked, not dropped, so the choice can be reversed. The
/// entry's footprint must equal the mark's, because its `c`/`s` planes are per pixel: a
/// model of a different footprint is not a model of this mark whatever its signature says.
///
/// The entry is handed in already loaded: reading it is I/O, and keeping it out of here is
/// what lets the decision half of auto-match be tested without touching the installation's
/// own library.
///
/// # Errors
/// A user-facing message for a footprint mismatch or an engine failure while refitting.
fn adopt_library_entry(
    kind: &mut WatermarkKind,
    mark: &mut ChapterMark,
    entry: LoadedEntry,
) -> Result<(), String> {
    let entry_id = entry.meta.id.clone();
    let (width, height) = (kind.template().width(), kind.template().height());
    if entry.meta.width != width || entry.meta.height != height {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.geometry_mismatch_error",
            width = entry.meta.width,
            height = entry.meta.height,
            expected_width = width,
            expected_height = height
        ));
    }
    if mark.parked.is_none() {
        mark.parked = Some(ParkedCalibration {
            crops: std::mem::take(&mut mark.crops),
            alpha_assumption: mark.alpha_assumption,
        });
    }
    let rebuilt = rebuild_chapter_kind(
        kind.id(),
        &mark.template_crop,
        kind.template().anchors(),
        entry.meta.alpha_assumption,
        &entry.samples,
    )?;
    *kind = rebuilt;
    mark.crops = entry.samples;
    mark.alpha_assumption = entry.meta.alpha_assumption;
    mark.adopted_entry = Some(entry_id.clone());
    mark.library_entry = Some(entry_id);
    Ok(())
}

/// Gives one mark its own calibration back after an adopted entry is dropped.
///
/// A no-op when nothing was adopted: without the guard this would replace a mark's own
/// samples with an empty parked set and silently destroy its calibration.
///
/// # Errors
/// A user-facing message for an engine failure while refitting.
fn release_library_entry(kind: &mut WatermarkKind, mark: &mut ChapterMark) -> Result<(), String> {
    if mark.adopted_entry.is_none() {
        return Ok(());
    }
    let parked = mark.parked.take().unwrap_or(ParkedCalibration {
        crops: Vec::new(),
        alpha_assumption: StoredAlphaAssumption::FromDeposit,
    });
    let rebuilt = rebuild_chapter_kind(
        kind.id(),
        &mark.template_crop,
        kind.template().anchors(),
        parked.alpha_assumption,
        &parked.crops,
    )?;
    *kind = rebuilt;
    mark.crops = parked.crops;
    mark.alpha_assumption = parked.alpha_assumption;
    mark.adopted_entry = None;
    Ok(())
}

/// Matches every fitted mark of `catalog` against the library and adopts the entries that
/// strengthen it, returning how many marks a library entry now calibrates.
///
/// This is the step that makes a second chapter of the same source instant: a mark whose
/// signature is already in the library needs no calibration of its own at all. Adoption is
/// deliberately conservative — see `watermark_entry::candidate_improves` — and never
/// overrides a choice the user already made (`ChapterMark::adopted_entry`).
///
/// `load` reads one entry by id. It is a parameter rather than a direct `load_entry` call
/// so the decision can be exercised against an in-memory library in tests.
fn match_catalog_against_library<F>(
    catalog: &mut ChapterCatalog,
    library: &[EntrySummary],
    load: F,
) -> usize
where
    F: Fn(&str) -> Result<LoadedEntry, String>,
{
    let mut matched = 0usize;
    for index in 0..catalog.len() {
        let ChapterCatalog { kinds, marks } = &mut *catalog;
        let (Some(kind), Some(mark)) = (kinds.get_mut(index), marks.get_mut(index)) else {
            continue;
        };
        let Some(signature) = kind.signature() else {
            // No model and no flat sample: the mark has a shape but no measured identity,
            // and matching on shape is exactly what this feature must not do.
            mark.matches.clear();
            continue;
        };
        let footprint = (kind.template().width(), kind.template().height());
        mark.matches = rank_library_candidates(library, &signature, footprint);
        // The user's explicit choice wins outright; otherwise the best candidate is adopted
        // only when it is genuinely stronger than what this chapter measured itself.
        let chosen = match mark.pinned_entry.clone() {
            Some(entry_id) => Some(entry_id),
            None => mark
                .matches
                .first()
                .filter(|candidate| candidate_improves(candidate, kind.conditioning()))
                .map(|candidate| candidate.entry_id.clone()),
        };
        let Some(entry_id) = chosen else {
            continue;
        };
        match load(&entry_id).and_then(|entry| adopt_library_entry(kind, mark, entry)) {
            Ok(()) => matched += 1,
            Err(err) => ms_log::runtime_log::log_warn(format!(
                "[cleaning] watermark library entry {entry_id} does not fit mark {}: {err}",
                kind.id()
            )),
        }
    }
    matched
}

/// Request of the chapter scan.
struct ChapterScanRequest {
    catalog: ChapterCatalog,
    pages: Vec<ChapterPageTask>,
    settings: WatermarkRemovalSettings,
}

/// Scans the whole chapter: anchor discovery and flat-ring sample collection first, then
/// the fit, then the gain-verified rescan.
///
/// Detection is inherently two-pass — the gain test needs a model, so it cannot be the
/// first thing that runs. Pages are decoded ONE AT A TIME (a chapter is several strips of
/// ~700x18000 and holding them all would cost hundreds of megabytes); `rayon` inside the
/// engine parallelizes within a page.
///
/// # Errors
/// A user-facing message for an empty catalog, a page that cannot be decoded, or an engine
/// failure.
fn run_chapter_scan(
    request: ChapterScanRequest,
    tx: &Sender<ChapterEvent>,
) -> Result<ChapterScanOutcome, String> {
    let ChapterScanRequest {
        mut catalog,
        pages,
        settings,
    } = request;
    if catalog.is_empty() {
        return Err(t!("cleaning.tools.watermark.chapter.no_marks_error").to_string());
    }
    if pages.is_empty() {
        return Err(t!("cleaning.tab.no_pages_error").to_string());
    }
    let params = settings.chapter_detection_params();
    let sample_params = settings.chapter_sample_params();
    let total = pages.len();
    let kinds = catalog.len();
    let mut anchor_hits: Vec<Vec<u32>> = vec![Vec::new(); kinds];
    for mark in &mut catalog.marks {
        mark.reset_scan_stats();
    }
    // A scan re-measures the chapter, so every mark goes back to its OWN calibration before
    // pass 1 collects into it. The adopted entry is re-applied in pass 1.5 (pinned choices
    // verbatim, automatic ones only when they still improve), which keeps the parked set a
    // clean record of what this chapter measured rather than a mixture of the two.
    for index in 0..kinds {
        let ChapterCatalog { kinds, marks } = &mut catalog;
        let (Some(kind), Some(mark)) = (kinds.get_mut(index), marks.get_mut(index)) else {
            continue;
        };
        release_library_entry(kind, mark)?;
    }

    // Pass 1: correlation-only bootstrap. Discover the anchor SET the source stamps at and
    // collect every occurrence whose ring is flat as a calibration sample.
    for (done, task) in pages.iter().enumerate() {
        let page = decode_chapter_page(&task.path)?;
        for (index, hits) in anchor_hits.iter_mut().enumerate() {
            let found = discover_anchors(&[&page], catalog.kinds[index].template(), &params)
                .map_err(chapter_engine_error)?;
            if !found.is_empty() {
                hits.extend(found);
                catalog.kinds[index]
                    .template_mut()
                    .set_anchors(hits)
                    .map_err(chapter_engine_error)?;
            }
            let occurrences = find_occurrences(&page, &catalog.kinds[index], &params)
                .map_err(chapter_engine_error)?;
            for occurrence in &occurrences {
                if catalog.kinds[index].samples().len() >= CHAPTER_MAX_AUTO_SAMPLES {
                    break;
                }
                let (verdict, sample) = calibration_sample_from_page(
                    &page,
                    task.page_idx,
                    occurrence.rect,
                    &sample_params,
                )
                .map_err(chapter_engine_error)?;
                let (
                    Some(sample),
                    SampleVerdict::Calibration {
                        level,
                        ring_std,
                        ring,
                    },
                ) = (sample, verdict)
                else {
                    continue;
                };
                catalog.kinds[index]
                    .add_sample(sample)
                    .map_err(chapter_engine_error)?;
                catalog.marks[index].crops.push(LibrarySample {
                    image: crop_page(&page, occurrence.rect),
                    origin: StoredSampleOrigin::Page {
                        page_index: task.page_idx,
                        x: occurrence.rect.x,
                        y: occurrence.rect.y,
                    },
                    background: stored_flat_background(level, ring_std, ring),
                });
            }
            catalog.marks[index].page_width = page.width();
        }
        let _ = tx.send(ChapterEvent::Progress {
            done: done + 1,
            total,
            label: t!("cleaning.tools.watermark.chapter.scan_stage_detect").to_string(),
        });
    }

    for index in 0..kinds {
        refit_chapter_kind(&mut catalog.kinds[index])?;
    }

    // Pass 1.5: AUTO-MATCH. A mark whose signature is already in the library needs no
    // calibration of its own, so the library is consulted before the verified rescan and
    // its model — when it is the stronger one — is what pass 2 then verifies against.
    let matched = match_catalog_against_library(&mut catalog, &list_entries(), load_entry);
    for index in 0..kinds {
        refresh_chapter_preview(&mut catalog, index);
    }

    // Pass 2: the gain-verified rescan, which is the only evidence removal is licensed on.
    let mut hits: Vec<ChapterPageHits> = Vec::with_capacity(pages.len());
    let mut report = ChapterScanReport {
        pages: pages.len(),
        matched,
        ..ChapterScanReport::default()
    };
    let mut gain_sums = vec![(0.0f32, 0.0f32, 0usize); kinds];
    for (done, task) in pages.iter().enumerate() {
        let page = decode_chapter_page(&task.path)?;
        let page_hits = scan_page(&page, task.page_idx, &catalog.kinds, &params)
            .map_err(chapter_engine_error)?;
        let mut records = Vec::with_capacity(page_hits.len());
        for hit in page_hits {
            let Some(mark) = catalog.marks.get_mut(hit.kind_index) else {
                continue;
            };
            mark.occurrences += 1;
            report.found += 1;
            match hit.occurrence.evidence {
                AcceptanceEvidence::Correlation => {
                    mark.unverified += 1;
                    report.unverified += 1;
                }
                AcceptanceEvidence::Gain { gain, snr } => {
                    let slot = &mut gain_sums[hit.kind_index];
                    slot.0 += gain;
                    slot.1 += snr;
                    slot.2 += 1;
                }
            }
            // The ring around an accepted occurrence is what the background histogram
            // counts: it is the level the removal there is exact at.
            if let Some(level) =
                validate_calibration_sample(&page, hit.occurrence.rect, &sample_params).level()
            {
                mark.histogram[chapter_histogram_bin(luma_of_level(level))] += 1;
            }
            records.push(ChapterHitRecord {
                mark_id: catalog.kinds[hit.kind_index].id().to_string(),
                occurrence: hit.occurrence,
            });
        }
        hits.push(ChapterPageHits {
            page_idx: task.page_idx,
            hits: records,
        });
        let _ = tx.send(ChapterEvent::Progress {
            done: done + 1,
            total,
            label: t!("cleaning.tools.watermark.chapter.scan_stage_verify").to_string(),
        });
    }
    for (index, (gain, snr, count)) in gain_sums.into_iter().enumerate() {
        if count == 0 {
            continue;
        }
        // Cast justification: a count of occurrences on one chapter, far below 2^24.
        let n = count as f32;
        catalog.marks[index].gain_mean = gain / n;
        catalog.marks[index].snr_mean = snr / n;
    }

    Ok(ChapterScanOutcome {
        catalog,
        hits,
        report,
    })
}

/// Request of the chapter apply.
struct ChapterApplyRequest {
    catalog: ChapterCatalog,
    pages: Vec<ChapterPageTask>,
    hits: Vec<ChapterPageHits>,
}

/// Removes every gain-verified occurrence, page by page, and streams the prepared patches
/// to the GUI thread.
///
/// A correlation-only accept, and every occurrence of a mark that has no model, is COUNTED
/// as refused rather than removed: subtracting a mark that is not there injects an inverse
/// one, and a silent skip would hide that from the user.
///
/// # Errors
/// A user-facing message for a page that cannot be decoded or an engine failure.
fn run_chapter_apply(
    request: ChapterApplyRequest,
    tx: &Sender<ChapterEvent>,
) -> Result<ChapterApplyReport, String> {
    let ChapterApplyRequest {
        catalog,
        pages,
        hits,
    } = request;
    let mut report = ChapterApplyReport::default();
    let total = hits.iter().filter(|page| !page.hits.is_empty()).count();
    let mut done = 0usize;
    for page_hits in &hits {
        if page_hits.hits.is_empty() {
            continue;
        }
        let Some(task) = pages
            .iter()
            .find(|task| task.page_idx == page_hits.page_idx)
        else {
            continue;
        };
        let page = decode_chapter_page(&task.path)?;
        let page_size = [page.width() as usize, page.height() as usize];
        let mut patches: Vec<ChapterPatch> = Vec::new();
        let mut residual = RemovalResidual::default();
        let mut removed = 0usize;
        let mut refused = 0usize;
        for index in 0..catalog.len() {
            let mark_id = catalog.kinds[index].id();
            let occurrences: Vec<Occurrence> = page_hits
                .hits
                .iter()
                .filter(|record| record.mark_id == mark_id)
                .map(|record| record.occurrence.clone())
                .collect();
            if occurrences.is_empty() {
                continue;
            }
            let Some(model) = catalog.kinds[index].model() else {
                refused += occurrences.len();
                continue;
            };
            let safe: Vec<Occurrence> = occurrences
                .iter()
                .filter(|occurrence| occurrence.is_removal_safe())
                .cloned()
                .collect();
            refused += occurrences.len() - safe.len();
            if safe.is_empty() {
                continue;
            }
            let page_patches =
                remove_occurrences_on_page(&page, model, &safe).map_err(chapter_engine_error)?;
            for patch in page_patches {
                residual.merge(&patch.residual);
                removed += 1;
                patches.push(ChapterPatch {
                    rect: patch.rect,
                    image: egui::ColorImage::from_rgba_unmultiplied(
                        [patch.rect.width as usize, patch.rect.height as usize],
                        &patch.pixels,
                    ),
                });
            }
        }
        report.pages += 1;
        report.removed += removed;
        report.refused += refused;
        report.residual.merge(&residual);
        let _ = tx.send(ChapterEvent::ApplyBatch(Box::new(ChapterApplyBatch {
            page_idx: page_hits.page_idx,
            page_size,
            overlay_size: task.overlay_size,
            patches,
            removed,
            refused,
            residual,
        })));
        done += 1;
        let _ = tx.send(ChapterEvent::Progress {
            done,
            total,
            label: t!("cleaning.tools.watermark.chapter.apply_stage").to_string(),
        });
    }
    Ok(report)
}

/// Request of the "use this library entry for this mark" worker.
struct ChapterMatchRequest {
    catalog: ChapterCatalog,
    index: usize,
    /// `Some(entry_id)` adopts that entry; `None` restores the chapter's own calibration.
    entry: Option<String>,
}

/// Applies the user's override of an automatic library match.
///
/// The scan that produced the match is invalidated by the caller: the model changed, and
/// the gain test — the only evidence removal is licensed on — was run against the old one.
///
/// # Errors
/// A user-facing message for an unreadable entry, a footprint mismatch, or an engine
/// failure while refitting.
fn run_chapter_use_match(request: ChapterMatchRequest) -> Result<ChapterSampleOutcome, String> {
    let ChapterMatchRequest {
        mut catalog,
        index,
        entry,
    } = request;
    if index >= catalog.len() {
        return Err(t!("cleaning.tools.watermark.chapter.no_marks_error").to_string());
    }
    let status = {
        let ChapterCatalog { kinds, marks } = &mut catalog;
        let (kind, mark) = (&mut kinds[index], &mut marks[index]);
        // The choice is PINNED before it is applied, so a later rescan rebuilds the same
        // decision instead of falling back to the automatic one.
        mark.pinned_entry.clone_from(&entry);
        match entry.as_deref() {
            Some(entry_id) => {
                adopt_library_entry(kind, mark, load_entry(entry_id)?)?;
                tf!(
                    "cleaning.tools.watermark.chapter.library_match_adopted_status",
                    name = mark
                        .matches
                        .iter()
                        .find(|candidate| candidate.entry_id == entry_id)
                        .map_or_else(|| entry_id.to_string(), |candidate| candidate.name.clone())
                )
            }
            None => {
                release_library_entry(kind, mark)?;
                t!("cleaning.tools.watermark.chapter.library_match_released_status").to_string()
            }
        }
    };
    refresh_chapter_preview(&mut catalog, index);
    Ok(ChapterSampleOutcome {
        catalog,
        selected: index,
        status,
    })
}

/// Rebuilds a catalog mark from a library entry.
///
/// The calibration crops are the reconstruction source: the engine only produces a model
/// through `WatermarkKind::refit`, so a loaded entry is refitted from its own measurements
/// and cannot disagree with them.
///
/// # Errors
/// A user-facing message for an unreadable entry or an engine failure.
fn run_chapter_library_load(entry_id: &str) -> Result<ChapterLoadOutcome, String> {
    let entry = load_entry(entry_id)?;
    // The rebuild is SHARED with the reference intake and the sample editor
    // (`watermark_entry::rebuild_stored_kind`): a stored entry becomes a model in exactly one
    // place, so the three paths cannot disagree about how a kind is assembled.
    // An EMPTY entry has no mark yet: nothing to correlate against, no footprint to scan for
    // and no model to remove with. Refusing by name is the only honest answer — the alternative
    // is a chapter mark made of a template that does not exist.
    let Some(template) = entry.template.clone() else {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_entry_empty_error",
            name = entry.meta.name.clone()
        ));
    };
    let kind = rebuild_stored_kind(
        entry.meta.id.clone(),
        &template,
        (entry.meta.width, entry.meta.height),
        &entry.meta.anchors,
        entry.meta.alpha_assumption,
        &entry.samples,
    )?;
    let mut mark = ChapterMark::new(
        entry.meta.name.clone(),
        template,
        entry
            .meta
            .sources
            .first()
            .map_or(entry.meta.width, |source| source.page_width),
    );
    mark.library_entry = Some(entry.meta.id.clone());
    mark.alpha_assumption = entry.meta.alpha_assumption;
    // The crops are kept in LOCKSTEP with the kind's calibration samples: the engine never
    // hands sample pixels back, and the library needs them to write the entry again.
    mark.crops = entry.samples;
    mark.preview = kind.model().map(build_chapter_preview);
    mark.preview_revision = 1;
    let status = tf!(
        "cleaning.tools.watermark.chapter.library_loaded_status",
        name = mark.name.clone()
    );
    Ok(ChapterLoadOutcome { kind, mark, status })
}

/// Builds the whole save request for one mark.
///
/// Only the mark's flat calibration crops are handed over — an estimated per-pixel
/// background is derived from a model rather than measured, and persisting one would let a
/// later fold-in overwrite a measurement with an estimate. The request itself is assembled by
/// the SHARED `watermark_entry::save_request_from_kind`, so a chapter write and a library-side
/// rewrite cannot describe the same kind differently.
///
/// A mark with NO flat crops is written DELIBERATELY, not by omission. Such a mark was
/// discovered by correlation and never measured against a background the engine would call
/// flat, so the entry it produces carries its template and its footprint but no samples and no
/// model — a legitimate state the library expresses (`NotEnoughSamples`) and the one the user
/// then adds crops to from the panel. It is logged as such, because "the entry I saved has no
/// samples" is otherwise indistinguishable from a crop list that was lost on the way here.
fn build_library_request(
    mark: &ChapterMark,
    kind: &WatermarkKind,
    source: Option<StoredSourceRef>,
) -> SaveEntryRequest {
    if mark.crops.is_empty() {
        ms_log::runtime_log::log_info(format!(
            "[cleaning] watermark library: saving mark {:?} with NO calibration crops. \
             The entry keeps its template and footprint and carries no model; samples can be \
             added to it from the library panel.",
            mark.name
        ));
    }
    save_request_from_kind(
        StoredEntryIdentity {
            entry_id: mark.library_entry.clone(),
            name: mark.name.clone(),
            alpha_assumption: mark.alpha_assumption,
            source,
        },
        kind,
        // A chapter mark ALWAYS has its template crop — it is what detection ran on — so this
        // path never writes a template-less entry. That state belongs to «+ новый» alone.
        Some(mark.template_crop.clone()),
        mark.crops.clone(),
    )
}

/// Search metadata of the open project: where this measurement came from.
///
/// The source key is the SERIES folder (the chapter's parent), because a mark belongs to a
/// publisher rather than to one chapter; the chapter folder is recorded next to it for the
/// user's own bookkeeping. Both are literal, never localized.
fn chapter_source_ref(
    project: &ProjectData,
    page_width: u32,
    variant_id: &str,
    anchor_key: String,
) -> StoredSourceRef {
    let source_key = project
        .project_dir
        .parent()
        .and_then(|parent| parent.file_name())
        .or_else(|| project.project_dir.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    StoredSourceRef {
        source_key,
        page_width,
        anchor_key,
        variant_id: variant_id.to_string(),
        chapter: project
            .project_dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string()),
    }
}

// ---------------------------------------------------------------------------------------
// Chapter mode: report text
// ---------------------------------------------------------------------------------------

/// Localized report of one sample verdict, including the measured reason a selection was
/// refused as a calibration target.
fn describe_sample_verdict(verdict: &SampleVerdict) -> String {
    match verdict {
        // A partial ring gets its OWN wording rather than the same sentence with a footnote:
        // "measured against the page edge" is what the user can act on, and hiding it inside
        // the ordinary line would let a one-sided measurement read as a complete one.
        SampleVerdict::Calibration {
            level,
            ring_std,
            ring,
        } if ring.is_partial() => tf!(
            "cleaning.tools.watermark.chapter.sample_calibration_partial",
            level = format_levels(&[luma_of_level(*level)]),
            std = format!("{:.1}", ring_std.iter().copied().fold(0.0f32, f32::max)),
            pixels = ring.pixels,
            full = ring.full_pixels
        ),
        SampleVerdict::Calibration {
            level, ring_std, ..
        } => tf!(
            "cleaning.tools.watermark.chapter.sample_calibration",
            level = format_levels(&[luma_of_level(*level)]),
            std = format!("{:.1}", ring_std.iter().copied().fold(0.0f32, f32::max))
        ),
        SampleVerdict::TemplateOnly {
            ring_std,
            ring_max_dev,
            std_limit,
            max_dev_limit,
            ..
        } => tf!(
            "cleaning.tools.watermark.chapter.sample_template_only",
            std = format!("{:.1}", ring_std.iter().copied().fold(0.0f32, f32::max)),
            std_limit = format!("{std_limit:.1}"),
            max_dev = format!("{:.1}", ring_max_dev.iter().copied().fold(0.0f32, f32::max)),
            max_dev_limit = format!("{max_dev_limit:.1}")
        ),
        SampleVerdict::Unusable {
            reason: SampleRejection::BadRect,
        } => t!("cleaning.tools.watermark.chapter.sample_unusable_rect").to_string(),
        SampleVerdict::Unusable {
            reason: SampleRejection::RingTooSmall { pixels, needed },
        } => tf!(
            "cleaning.tools.watermark.chapter.sample_unusable_ring",
            pixels = pixels,
            needed = needed
        ),
    }
}

/// Background levels as a compact list.
fn format_levels(levels: &[f32]) -> String {
    levels
        .iter()
        .map(|level| format!("{level:.0}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The GRADED conditioning verdict in the user's words.
///
/// Wording is load-bearing (`dev-docs/watermark_chapter_decomposition_plan.md`,
/// "Corrections from the second implementation round"): what is measured exactly is the
/// IMPRINT, never «c»; the stated percentage bounds the alpha SCALE only.
///
/// `rests_on_assertion` is `true` when the model behind `conditioning` was fitted on at
/// least one HAND-STATED background level (`ModelProvenance::manual_backgrounds` is
/// non-zero). It is a CORRECTNESS input, not a decoration: such a model may never be
/// described as measured, whatever the verdict tag says, so the two verdicts that claim a
/// measurement switch to their `_asserted` wording. The library card enforces the same rule
/// through `entry_verdict_line`, and the two must not be able to disagree about one entry.
/// `pub(super)` for one reason only: the library panel's own wording test asserts that this
/// function and `entry_verdict_line` cannot disagree about the SAME entry, and the obligation
/// is only worth testing when both halves are checked together.
pub(super) fn describe_conditioning(
    conditioning: &ModelConditioning,
    rests_on_assertion: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(match conditioning {
        ModelConditioning::Separable { .. } if rests_on_assertion => {
            t!("cleaning.tools.watermark.chapter.verdict_separable_asserted").to_string()
        }
        ModelConditioning::Separable { .. } => {
            t!("cleaning.tools.watermark.chapter.verdict_separable").to_string()
        }
        ModelConditioning::DepositExact { .. } if rests_on_assertion => {
            t!("cleaning.tools.watermark.chapter.verdict_deposit_exact_asserted").to_string()
        }
        ModelConditioning::DepositExact { .. } => {
            t!("cleaning.tools.watermark.chapter.verdict_deposit_exact").to_string()
        }
        ModelConditioning::NotEnoughSamples { have, need } => tf!(
            "cleaning.tools.watermark.chapter.verdict_not_enough",
            have = have,
            need = need
        ),
        ModelConditioning::DepositUnavailable { samples, spread } => tf!(
            "cleaning.tools.watermark.chapter.verdict_deposit_unavailable",
            samples = samples,
            spread = format!("{spread:.0}")
        ),
        ModelConditioning::Underdetermined {
            underdetermined_pixels,
            total_pixels,
            worst_pixel_spread,
            required,
            ..
        } => tf!(
            "cleaning.tools.watermark.chapter.verdict_underdetermined",
            pixels = underdetermined_pixels,
            total = total_pixels,
            spread = format!("{worst_pixel_spread:.0}"),
            required = format!("{required:.0}")
        ),
    });
    let levels = conditioning.levels();
    lines.push(if levels.is_empty() {
        t!("cleaning.tools.watermark.chapter.levels_none").to_string()
    } else {
        tf!(
            "cleaning.tools.watermark.chapter.levels_line",
            levels = format_levels(levels),
            spread = format!("{:.0}", conditioning.spread())
        )
    });
    if let Some(alpha) = conditioning.alpha_uncertainty() {
        lines.push(describe_alpha_uncertainty(&alpha));
    }
    if let Some(suggestion) = conditioning.suggested_background() {
        lines.push(match suggestion {
            SuggestedBackground::Darker { at_most } => tf!(
                "cleaning.tools.watermark.chapter.suggest_darker",
                level = format!("{at_most:.0}")
            ),
            SuggestedBackground::Brighter { at_least } => tf!(
                "cleaning.tools.watermark.chapter.suggest_brighter",
                level = format!("{at_least:.0}")
            ),
        });
    }
    lines
}

/// How well the alpha SCALE is pinned, and what that costs in LSB.
fn describe_alpha_uncertainty(alpha: &AlphaUncertainty) -> String {
    let source = match alpha.source {
        AlphaSource::SeparatedBackgrounds => {
            t!("cleaning.tools.watermark.chapter.alpha_source_separated")
        }
        AlphaSource::EstimatedBackgrounds => {
            t!("cleaning.tools.watermark.chapter.alpha_source_estimated")
        }
        AlphaSource::ManualBackgrounds => {
            t!("cleaning.tools.watermark.chapter.alpha_source_manual")
        }
        AlphaSource::Assumed => t!("cleaning.tools.watermark.chapter.alpha_source_assumed"),
    };
    tf!(
        "cleaning.tools.watermark.chapter.alpha_line",
        percent = format!("{:.0}", alpha.percent),
        source = source,
        rms = format!("{:.1}", alpha.rms_lsb),
        dark_luma = format!("{:.0}", alpha.dark_luma),
        dark_rms = format!("{:.1}", alpha.dark_rms_lsb),
        dark_max = format!("{:.0}", alpha.dark_max_lsb)
    )
}

/// The quantization-and-clipping report of an apply.
///
/// Deliberately NOT a quality score: by construction the recomposition error is bounded by
/// `s*0.5` unless the recovery was clipped, so a badly fitted model produces a small
/// residual just as happily as a good one. Model quality is the detection gain and the
/// t-statistic, reported per mark.
fn describe_residual(residual: &RemovalResidual) -> String {
    tf!(
        "cleaning.tools.watermark.chapter.quantization_line",
        exact = format!("{:.1}", residual.exact_share() * 100.0),
        clipped = format!("{:.1}", residual.clipped_share() * 100.0),
        uncertainty = format!("{:.2}", residual.max_uncertainty_lsb)
    )
}

/// One word for a stored entry's calibration quality, for the match picker.
///
/// Only two stored verdicts carry a model at all; anything else is reported as "no model"
/// rather than dressed up as a grade.
fn library_quality_label(verdict: &str) -> &'static str {
    match verdict {
        "separable" => t!("cleaning.tools.watermark.chapter.library_quality_exact"),
        "deposit_exact" => t!("cleaning.tools.watermark.chapter.library_quality_graded"),
        _ => t!("cleaning.tools.watermark.chapter.library_quality_none"),
    }
}

/// The background histogram as a compact `bin: count` list, empty bins dropped.
fn describe_histogram(histogram: &[u32; CHAPTER_HISTOGRAM_BINS]) -> String {
    let step = 256 / CHAPTER_HISTOGRAM_BINS;
    histogram
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(bin, count)| format!("{}-{}: {count}", bin * step, (bin + 1) * step - 1))
        .collect::<Vec<_>>()
        .join("; ")
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
    /// Whether the open project is a single-image session, pushed by the tab every frame
    /// before the dock bodies draw. Disables the chapter mode (`effective_watermark_mode`).
    single_image_session: bool,
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
            single_image_session: false,
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

impl WatermarkRemovalTool {
    /// The mode this session runs in; see `effective_watermark_mode`.
    fn effective_mode(&self) -> WatermarkMode {
        effective_watermark_mode(&self.settings.mode, self.single_image_session)
    }

    /// Reads the settings file on a worker thread (never on the GUI thread).
    fn request_settings_load(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.settings_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(load_watermark_settings());
        });
    }

    /// Applies a finished settings load. A disconnected channel keeps the
    /// in-memory defaults and unblocks saving.
    fn poll_settings_load(&mut self) {
        let Some(rx) = self.settings_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(settings) => {
                self.settings = settings;
                self.settings_loaded = true;
                self.settings_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.settings_loaded = true;
                self.settings_rx = None;
            }
        }
    }

    /// Writes dirty settings on a worker thread, at most one save in flight, and
    /// never before the initial load finished (which would clobber the file).
    fn poll_and_maybe_save(&mut self) {
        if let Some(rx) = self.save_rx.as_ref() {
            match rx.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => self.save_rx = None,
                Err(TryRecvError::Empty) => return,
            }
        }
        if !self.dirty || !self.settings_loaded {
            return;
        }
        self.dirty = false;
        let settings = self.settings.clone();
        let (tx, rx) = mpsc::channel();
        self.save_rx = Some(rx);
        thread::spawn(move || {
            if let Err(err) = save_watermark_settings(&settings) {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] failed to save watermark removal settings: {err}"
                ));
            }
            let _ = tx.send(());
        });
    }

    /// Polls the catalog query and arms a new one when it is wanted and nothing
    /// else is running. The query is never issued while a run is in flight, so
    /// the backend is not asked mid-download.
    fn poll_and_maybe_query_status(&mut self) {
        poll_watermark_status(&mut self.status, &mut self.status_rx);
        if self.status_wanted
            && self.ai_backend_available
            && self.status_rx.is_none()
            && self.session.run_rx.is_none()
        {
            self.status_wanted = false;
            self.status_rx = Some(spawn_watermark_status_query());
        }
    }

    /// Snapshot of every page of the chapter, taken on the GUI thread before the editor
    /// borrows the canvas: path, index and the overlay size the canvas already knows.
    fn chapter_page_tasks(&self, canvas: &CanvasView, project: &ProjectData) -> Vec<ChapterPageTask> {
        project
            .pages
            .iter()
            .map(|page| ChapterPageTask {
                page_idx: page.idx,
                path: page.path.clone(),
                overlay_size: canvas.overlay_size(page.idx),
            })
            .collect()
    }

    /// Ids of the library entries the open chapter is currently searching for.
    ///
    /// Membership is the CATALOG, not `pinned_entry`: an entry is being hunted exactly when a
    /// mark of its own was loaded from it. `ChapterCatalog::index_of` keys on the kind id, and
    /// a kind loaded from the library carries the entry id, but a chapter-DISCOVERED mark is
    /// `mark-{n}`; `library_entry` is what tells the two apart, so it is what is checked here
    /// and in `unload_library_entry`.
    fn chapter_library_selection(&self) -> Vec<String> {
        self.chapter
            .catalog
            .kinds
            .iter()
            .zip(self.chapter.catalog.marks.iter())
            .filter(|(kind, mark)| mark.library_entry.as_deref() == Some(kind.id()))
            .map(|(kind, _)| kind.id().to_string())
            .collect()
    }

    /// Ids of the library entries a chapter-DISCOVERED mark was written to.
    ///
    /// «В библиотеку» stamps the new entry id onto a mark whose kind id stays `mark-{n}`, so
    /// such an entry is already in the chapter under a different name: loading it would give
    /// the catalog a SECOND kind for one physical mark, and «Убрать» would then drop only the
    /// loaded copy. It is therefore neither selectable nor unselectable — the card says it is
    /// already here, and `ChapterRequest::LoadLibrary` refuses it for the same reason.
    /// Index of the catalog mark that ALREADY represents `entry_id`, however it got there.
    ///
    /// Two things can put an entry in the chapter: loading it (the kind takes the entry's id)
    /// and saving a discovered mark to it (the kind keeps `mark-{n}` and only the mark names
    /// the entry). Both count, because either way a second load would give the catalog two
    /// kinds for one physical mark and everything keyed on that id would resolve to whichever
    /// came first.
    fn chapter_entry_index(&self, entry_id: &str) -> Option<usize> {
        self.chapter.catalog.index_of(entry_id).or_else(|| {
            self.chapter
                .catalog
                .marks
                .iter()
                .position(|mark| mark.library_entry.as_deref() == Some(entry_id))
        })
    }

    fn chapter_saved_entries(&self) -> Vec<String> {
        self.chapter
            .catalog
            .kinds
            .iter()
            .zip(self.chapter.catalog.marks.iter())
            .filter_map(|(kind, mark)| {
                mark.library_entry
                    .as_deref()
                    .filter(|entry_id| *entry_id != kind.id())
                    .map(str::to_string)
            })
            .collect()
    }

    /// Runs what the library panel asked for while it drew.
    ///
    /// Reached from `draw_overlay_ui`, i.e. after the dock body of the same frame, because
    /// every branch here touches state the dock body may not: the chapter catalog, the
    /// canvas, or a worker.
    fn run_library_panel_request(
        &mut self,
        request: LibraryPanelRequest,
        canvas: &CanvasView,
        project: &ProjectData,
    ) {
        self.library_arm = next_library_arm(self.library_arm.take(), &request);
        self.sync_selection_diversion();
        match request {
            LibraryPanelRequest::SelectEntry(entry_id) => {
                self.start_chapter_request(ChapterRequest::LoadLibrary(entry_id), canvas, project);
            }
            LibraryPanelRequest::UnselectEntry(entry_id) => self.unload_library_entry(&entry_id),
            // The reservation is the WHOLE effect of these two; it was applied above.
            LibraryPanelRequest::Arm(_) | LibraryPanelRequest::Disarm => {}
        }
    }

    /// Drops the chapter mark that was loaded from `entry_id`, and only that one.
    ///
    /// A mark the chapter DISCOVERED is never touched, even when it sits under a kind id that
    /// happens to match: removing it would throw away measurements that cost a scan, and it
    /// was never "the library entry the user selected" in the first place.
    fn unload_library_entry(&mut self, entry_id: &str) {
        let Some(index) = self
            .chapter
            .catalog
            .index_of(entry_id)
            .filter(|index| {
                self.chapter
                    .catalog
                    .marks
                    .get(*index)
                    .and_then(|mark| mark.library_entry.as_deref())
                    == Some(entry_id)
            })
        else {
            return;
        };
        // Drop the mark's preview textures with it, or the map would keep one entry per mark
        // ever created in this session.
        self.chapter.textures.remove(entry_id);
        self.chapter.catalog.remove(index);
        self.chapter.selected = self
            .chapter
            .selected
            .min(self.chapter.catalog.len().saturating_sub(1));
        self.chapter.invalidate_scan();
        self.chapter.status = Some(tf!(
            "cleaning.tools.watermark.chapter.library_unloaded_status",
            id = entry_id
        ));
    }

    /// Keeps the region base's selection DIVERSION in step with the live reservation.
    ///
    /// The two are one fact: while a reservation is in force the next completed selection is a
    /// library crop, and must not run the ordinary region editor — no loader job, no modal
    /// spinner, no editor window left standing over the canvas with its zoom and undo shortcuts
    /// disabled. Call this after every mutation of `library_arm`, including the ones that DROP
    /// it, because dropping the diversion is what discards an unclaimed diverted rect.
    fn sync_selection_diversion(&mut self) {
        self.region_base
            .set_selection_diverted(self.library_arm.is_some());
    }

    /// Drops a reservation whose screen is no longer drawn, SAYING SO.
    ///
    /// A reservation may only outlive the surface that made it for as long as that surface is
    /// the one the user is looking at: the red «Отменить выделение» is the ONLY way to call it
    /// off, so a reservation drawn on no screen is one the user cannot cancel. The panel
    /// decides (it owns the screen); this owns the reservation and drops it.
    ///
    /// Dropping it in silence was the defect: the next drag would then open the ordinary
    /// region editor instead of adding a sample, with nothing anywhere having said the
    /// reservation was gone. The reason lands on the panel — the surface that drew the
    /// reservation, and the one the user comes back to — and in the log, because a reservation
    /// dying for a reason the user did not intend is a diagnosable event.
    fn drop_reservation_without_a_screen(&mut self) {
        let Some(arm) = self.library_arm.as_ref() else {
            return;
        };
        if self.library_window.arm_survives(arm) {
            return;
        }
        ms_log::runtime_log::log_warn(format!(
            "[cleaning] watermark library reservation {arm:?} dropped: its screen is no longer drawn"
        ));
        self.library_arm = None;
        self.sync_selection_diversion();
        self.library_window
            .set_status(t!("cleaning.tools.watermark.chapter.library_arm_lost_status").to_string());
    }

    /// Takes the reservation a parked rect belongs to, REPORTING the rect when none is live.
    ///
    /// `None` is the defensive branch: every path that clears `library_arm` also calls
    /// `sync_selection_diversion`, which drops an unclaimed parked rect with it, so a rect
    /// reaching here unclaimed means one of those paths stopped keeping that promise. It costs
    /// a line to say so instead of dropping the user's gesture without a word — which on
    /// screen is indistinguishable from a drag that did nothing at all.
    #[must_use]
    fn take_reservation_for_diverted(
        &mut self,
        page_idx: usize,
        rect: OverlayRectPx,
    ) -> Option<LibraryArm> {
        let arm = self.library_arm.take();
        if arm.is_none() {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] diverted selection on page {page_idx} ({}x{} at {},{}) arrived with no reservation",
                rect.w, rect.h, rect.x, rect.y
            ));
            self.library_window.set_status(
                t!("cleaning.tools.watermark.chapter.library_arm_lost_status").to_string(),
            );
        }
        arm
    }

    /// Repeats the region base's "that drag produced no rectangle" reason on the library panel.
    ///
    /// The base keeps the same reason in its own dock body, but a user who armed a reservation
    /// is reading the LIBRARY panel, two tabs away. Drained unconditionally so the one-shot can
    /// never go stale; shown only while a reservation is live, because otherwise the base's own
    /// body is already the surface the user is looking at.
    fn report_selection_refusal(&mut self) {
        let refusal = self.region_base.take_selection_refusal();
        if let Some(refusal) = refusal
            && self.library_arm.is_some()
        {
            self.library_window.set_status(refusal);
        }
    }

    /// Cuts the armed canvas selection out of its page on a worker thread.
    ///
    /// The reservation is consumed by the caller before this runs, so a capture that fails
    /// does not leave the next selection silently spoken for.
    ///
    /// EVERY early return here is answered the same way a refused intake is: a red row on the
    /// entry the drag was aimed at (`WatermarkLibraryWindow::reject_capture`) plus a line in the
    /// runtime log. Neither is optional. The row is what the user sees — the panel's status line
    /// is nowhere near the list they are watching — and the log line is what makes the failure
    /// diagnosable from outside, which it was not: this lane used to report two of its refusals
    /// to the status line alone and nothing at all to the log.
    fn start_library_capture(
        &mut self,
        arm: LibraryArm,
        page_idx: usize,
        rect: OverlayRectPx,
        canvas: &CanvasView,
        project: &ProjectData,
    ) {
        if self.library_capture_rx.is_some() {
            self.library_window.reject_capture(
                &arm,
                &LibraryCaptureRefusal::untagged(
                    t!("cleaning.mask_editor.processing_already_running_status").to_string(),
                ),
            );
            return;
        }
        let Some(page) = self
            .chapter_page_tasks(canvas, project)
            .into_iter()
            .find(|task| task.page_idx == page_idx)
        else {
            self.library_window.reject_capture(
                &arm,
                &LibraryCaptureRefusal::untagged(tf!(
                    "cleaning.tools.watermark.chapter.library_capture_page_error",
                    page = page_idx + 1
                )),
            );
            return;
        };
        let request = LibraryCaptureRequest {
            page,
            rect,
            sample_params: self.settings.normalized().chapter_sample_params(),
            max_side: CHAPTER_MAX_TEMPLATE_SIDE,
        };
        let (tx, rx) = mpsc::channel();
        self.library_capture_rx = Some(rx);
        self.library_capture_arm = Some(arm);
        thread::spawn(move || {
            let _ = tx.send(run_library_capture(request));
        });
    }

    /// Folds a finished canvas capture into the library panel.
    ///
    /// A refusal lands on the entry the drag was aimed at, exactly as an accepted crop does —
    /// see `start_library_capture` for why the status line alone was not enough.
    fn poll_library_capture(&mut self) {
        let Some(rx) = self.library_capture_rx.as_ref() else {
            return;
        };
        let captured = match rx.try_recv() {
            Ok(captured) => captured,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(LibraryCaptureRefusal::untagged(
                t!("cleaning.mask_editor.processing_thread_crashed_error").to_string(),
            )),
        };
        self.library_capture_rx = None;
        let arm = self.library_capture_arm.take();
        match (captured, arm) {
            (Ok(crop), Some(arm)) => {
                let sample_params = self.settings.normalized().chapter_sample_params();
                self.library_window.accept_captured_crop(
                    crop,
                    &arm,
                    sample_params,
                    CHAPTER_MAX_TEMPLATE_SIDE,
                );
            }
            // The crop is on disk but nothing claims it any more: no screen can show it and
            // no screen can discard it, so the scratch file goes with the reservation that
            // was its only owner.
            (Ok(crop), None) => {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] watermark library capture {} arrived with no reservation",
                    crop.path.display()
                ));
                remove_scratch_crop(&crop.path);
            }
            (Err(refusal), Some(arm)) => self.library_window.reject_capture(&arm, &refusal),
            // No reservation left to hang the row on, so the panel-wide line is the only
            // surface there is — and it is still said out loud rather than dropped.
            (Err(refusal), None) => {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] watermark library capture refused with no reservation ({:?}): {}",
                    refusal.refusal, refusal.message
                ));
                self.library_window.set_status(refusal.message);
            }
        }
    }

    /// Drains the chapter worker channel and folds every finished step into the state.
    ///
    /// `canvas` is needed because a finished removal batch is applied here, on the GUI
    /// thread, through `CanvasView` — the tool never writes the overlay model itself.
    fn poll_chapter_job(&mut self, canvas: &mut CanvasView) {
        loop {
            let event = {
                let Some(rx) = self.chapter.rx.as_ref() else {
                    return;
                };
                rx.try_recv()
            };
            match event {
                Ok(ChapterEvent::Progress { done, total, label }) => {
                    self.chapter.progress = Some((done, total, label));
                }
                Ok(ChapterEvent::SampleDone(outcome)) => {
                    let outcome = *outcome;
                    self.chapter.catalog = outcome.catalog;
                    self.chapter.selected = outcome.selected;
                    self.chapter.next_mark_number = self
                        .chapter
                        .next_mark_number
                        .max(self.chapter.catalog.len())
                        .saturating_add(1);
                    self.chapter.status = Some(outcome.status);
                    // The catalog changed, so last scan's hits no longer describe it.
                    self.chapter.invalidate_scan();
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::ScanDone(outcome)) => {
                    let outcome = *outcome;
                    self.chapter.catalog = outcome.catalog;
                    self.chapter.hits = outcome.hits;
                    self.chapter.scan_report = Some(outcome.report);
                    self.chapter.apply_report = None;
                    self.chapter.status = Some(tf!(
                        "cleaning.tools.watermark.chapter.scan_done_status",
                        found = outcome.report.found,
                        pages = outcome.report.pages,
                        unverified = outcome.report.unverified
                    ));
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::ApplyBatch(batch)) => {
                    let failed = self.apply_chapter_batch(canvas, &batch);
                    // Accumulate as the pages land so the report grows live; the final
                    // event replaces the totals with the worker's own count.
                    let report = self
                        .chapter
                        .apply_report
                        .get_or_insert_with(ChapterApplyReport::default);
                    report.pages += 1;
                    report.removed += batch.removed;
                    report.refused += batch.refused;
                    report.residual.merge(&batch.residual);
                    report.failed_patches += failed;
                }
                Ok(ChapterEvent::ApplyDone(report)) => {
                    let failed = self
                        .chapter
                        .apply_report
                        .map_or(0, |partial| partial.failed_patches);
                    let mut report = *report;
                    report.failed_patches = failed;
                    self.chapter.status = Some(tf!(
                        "cleaning.tools.watermark.chapter.apply_done_status",
                        pages = report.pages,
                        removed = report.removed,
                        refused = report.refused
                    ));
                    self.chapter.apply_report = Some(report);
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::LibraryList(entries)) => {
                    self.chapter.library = entries;
                    self.chapter.library_loaded = true;
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::LibrarySaved { mark_id, entry_id }) => {
                    if let Some(index) = self.chapter.catalog.index_of(&mark_id) {
                        self.chapter.catalog.marks[index].library_entry = Some(entry_id.clone());
                    }
                    self.chapter.status = Some(tf!(
                        "cleaning.tools.watermark.chapter.library_saved_status",
                        id = entry_id
                    ));
                    // The library gained (or changed) an entry, so the picker is stale.
                    self.chapter.library_requested = true;
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::LibraryLoaded(outcome)) => {
                    let outcome = *outcome;
                    self.chapter.catalog.push(outcome.kind, outcome.mark);
                    self.chapter.selected = self.chapter.catalog.len().saturating_sub(1);
                    self.chapter.status = Some(outcome.status);
                    self.chapter.invalidate_scan();
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::Failed(err)) => {
                    self.chapter.status = Some(tf!("cleaning.mask_editor.processing_error", err = err));
                    self.finish_chapter_job();
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.chapter.status =
                        Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
                    self.finish_chapter_job();
                    return;
                }
            }
        }
    }

    /// Clears the in-flight markers once a job reported its last event.
    fn finish_chapter_job(&mut self) {
        self.chapter.rx = None;
        self.chapter.job = None;
        self.chapter.progress = None;
    }

    /// Writes one page's recovered patches into the clean overlay, returning how many the
    /// canvas refused (a page whose overlay could not be opened at all).
    fn apply_chapter_batch(&mut self, canvas: &mut CanvasView, batch: &ChapterApplyBatch) -> usize {
        let overlay_size = batch
            .overlay_size
            .or_else(|| canvas.overlay_size(batch.page_idx))
            .unwrap_or(batch.page_size);
        let mut failed = 0usize;
        for patch in &batch.patches {
            let Some(target) = page_rect_to_overlay(patch.rect, batch.page_size, overlay_size)
            else {
                failed += 1;
                continue;
            };
            if !canvas.replace_overlay_region_px(batch.page_idx, target, &patch.image) {
                failed += 1;
            }
        }
        if failed > 0 {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] chapter watermark removal: {failed} patch(es) of page {} were refused by the canvas",
                batch.page_idx
            ));
        }
        failed
    }

    /// Starts the worker one chapter button asked for.
    ///
    /// Every job takes a COPY of the catalog: the worker owns it for the duration and hands
    /// back the updated one, so the GUI keeps a consistent catalog to draw and the engine
    /// never runs under a lock.
    fn start_chapter_request(
        &mut self,
        request: ChapterRequest,
        canvas: &CanvasView,
        project: &ProjectData,
    ) {
        // The chapter mode is not offered in a single-image session (`effective_watermark_mode`);
        // this refuses a request that reached here anyway, because the sample path would file
        // a library entry under the scratch directory's name.
        if project.is_single_image() {
            ms_log::runtime_log::log_warn(
                "[cleaning.watermark] refused a chapter-mode request in a single-image session; \
                 the chapter mode is not offered there",
            );
            return;
        }
        // The page snapshot is built HERE rather than once per frame: it clones a path per
        // page of the chapter and only a job start ever needs it.
        let pages = self.chapter_page_tasks(canvas, project);
        match request {
            ChapterRequest::Sample {
                page_idx,
                rect,
                target,
            } => {
                let Some(page) = pages.iter().find(|task| task.page_idx == page_idx).cloned()
                else {
                    self.chapter.status = Some(t!("cleaning.tab.no_pages_error").to_string());
                    return;
                };
                let sample_request = ChapterSampleRequest {
                    catalog: self.chapter.catalog.clone(),
                    target,
                    page,
                    rect,
                    settings: self.settings.normalized(),
                    next_number: self.chapter.next_mark_number.max(1),
                };
                self.chapter.start_job(ChapterJobLabel::Sample, move |tx| {
                    let _ = tx.send(match run_chapter_sample(sample_request) {
                        Ok(outcome) => ChapterEvent::SampleDone(Box::new(outcome)),
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::Scan => {
                let scan_request = ChapterScanRequest {
                    catalog: self.chapter.catalog.clone(),
                    pages,
                    settings: self.settings.normalized(),
                };
                self.chapter.start_job(ChapterJobLabel::Scan, move |tx| {
                    let _ = tx.send(match run_chapter_scan(scan_request, tx) {
                        Ok(outcome) => ChapterEvent::ScanDone(Box::new(outcome)),
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::Apply => {
                if self.chapter.hits.is_empty() {
                    self.chapter.status =
                        Some(t!("cleaning.tools.watermark.chapter.no_hits_error").to_string());
                    return;
                }
                let apply_request = ChapterApplyRequest {
                    catalog: self.chapter.catalog.clone(),
                    pages,
                    hits: self.chapter.hits.clone(),
                };
                self.chapter.apply_report = None;
                self.chapter.start_job(ChapterJobLabel::Apply, move |tx| {
                    let _ = tx.send(match run_chapter_apply(apply_request, tx) {
                        Ok(report) => ChapterEvent::ApplyDone(Box::new(report)),
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::SaveLibrary(index) => {
                let (Some(mark), Some(kind)) = (
                    self.chapter.catalog.marks.get(index),
                    self.chapter.catalog.kinds.get(index),
                ) else {
                    return;
                };
                let mark_id = kind.id().to_string();
                let source = chapter_source_ref(
                    project,
                    mark.page_width,
                    kind.id(),
                    kind.template().anchor_key(),
                );
                let save_request = build_library_request(mark, kind, Some(source));
                self.chapter.start_job(ChapterJobLabel::Library, move |tx| {
                    let _ = tx.send(match save_entry(&save_request) {
                        Ok(entry_id) => ChapterEvent::LibrarySaved { mark_id, entry_id },
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::LoadLibrary(entry_id) => {
                // Loading the same entry twice would give the catalog two kinds sharing one
                // id, and everything keyed on that id (patches, previews, the save target)
                // would resolve to whichever came first. A mark the chapter DISCOVERED and
                // then saved to the library is the same physical mark under a `mark-{n}` kind
                // id, so it has to be recognised by `library_entry` as well.
                if let Some(index) = self.chapter_entry_index(&entry_id) {
                    self.chapter.selected = index;
                    self.chapter.status = Some(
                        t!("cleaning.tools.watermark.chapter.library_already_loaded_status")
                            .to_string(),
                    );
                    return;
                }
                self.chapter.start_job(ChapterJobLabel::Library, move |tx| {
                    let _ = tx.send(match run_chapter_library_load(&entry_id) {
                        Ok(outcome) => ChapterEvent::LibraryLoaded(Box::new(outcome)),
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::RefreshLibrary => self.request_chapter_library(),
            ChapterRequest::UseLibraryMatch { index, entry } => {
                let match_request = ChapterMatchRequest {
                    catalog: self.chapter.catalog.clone(),
                    index,
                    entry,
                };
                self.chapter.start_job(ChapterJobLabel::Library, move |tx| {
                    let _ = tx.send(match run_chapter_use_match(match_request) {
                        Ok(outcome) => ChapterEvent::SampleDone(Box::new(outcome)),
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::ToggleLibraryPanel => self.library_window.toggle(),
        }
    }

    /// Asks the library for its entry list, at most one query at a time.
    fn request_chapter_library(&mut self) {
        self.chapter.library_requested = false;
        self.chapter.start_job(ChapterJobLabel::Library, |tx| {
            let _ = tx.send(ChapterEvent::LibraryList(list_entries()));
        });
    }

    /// Polls the background unload call and reports its outcome in the params
    /// section.
    fn poll_unload(&mut self) {
        let Some(rx) = self.unload_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.unload_rx = None;
                self.unload_status =
                    Some(t!("cleaning.tools.watermark.unload_done_status").to_string());
            }
            Ok(Err(err)) => {
                self.unload_rx = None;
                self.unload_status = Some(tf!("cleaning.inpaint.unload_error", err = err));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.unload_rx = None;
            }
        }
    }
}

impl CleaningTool for WatermarkRemovalTool {
    fn tool_id(&self) -> &'static str {
        "watermark_removal"
    }

    fn title(&self) -> &'static str {
        t!("cleaning.tools.watermark.title")
    }

    /// Always `false`: the Torch requirement of this tool lives INSIDE it.
    ///
    /// Only the two network modes need Torch, and «По главе (точное вычитание)» needs neither
    /// backend nor Torch nor weights — so the picker BUTTON must never be AI-gated. Following
    /// the mode here made the persisted default (`mask_only`) disable the button on a fresh
    /// Torch-less install, hiding the one mode that would have worked; and because the same
    /// answer feeds `ensure_active_tool_available` every frame, picking a network mode
    /// force-deselected the tool. The gate is therefore drawn where it belongs: the two
    /// network entries of the mode picker and the run button (`draw_mode_picker`,
    /// `draw_actions`), both explaining themselves on hover.
    fn pytorch_required(&self) -> bool {
        false
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.region_base.cancel_selection();
        self.session.clear();
        // A canvas reservation belongs to the tool that is drawing: nothing cancels it while
        // this tool is not active, and a selection made after coming back would otherwise be
        // cut out as a library crop the user no longer expects.
        self.library_arm = None;
        // ORDER MATTERS: the tab calls `finish_stroke()` before `deactivate()`, so switching
        // tools mid-drag completes a DIVERTED selection one statement before the reservation
        // dies. Syncing here is what drops that orphan rect — without it the next activation
        // would find a rect nothing armed and cut it out for a reservation that is gone.
        self.sync_selection_diversion();
        // The chapter catalog and any job in flight deliberately survive: a chapter-wide
        // pass takes minutes and must not be thrown away by a stray tool switch.
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.region_base.draw_ui_hint(ui);
        if self.effective_mode() == WatermarkMode::Chapter {
            ui.small(t!("cleaning.tools.watermark.chapter.description_hint"));
            // Reachable without an open region editor: an entry can be built from reference
            // crops alone, with no chapter and no selection involved. A TOGGLE drawn pressed
            // while the panel is open — a dock tab has no close affordance by design, so this
            // button and its twin in the region editor are the whole story.
            if ui
                .add(
                    egui::Button::new(t!("cleaning.tools.watermark.chapter.library_manage_button"))
                        .selected(self.library_window.is_open()),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.library_manage_hint"))
                .clicked()
            {
                self.library_window.toggle();
            }
        } else {
            ui.small(t!("cleaning.tools.watermark.description_hint"));
            ui.small(t!("cleaning.tools.watermark.download_hint"));
            ui.small(t!("cleaning.tools.watermark.experimental_warning"));
        }
        // A chapter job outlives its editor window, so its progress belongs here too.
        if let Some((done, total, label)) = self.chapter.progress.clone() {
            ui.small(tf!(
                "cleaning.tools.watermark.chapter.job_progress",
                label = label,
                done = done,
                total = total
            ));
        }
        if let Some(status) = self.chapter.status.as_ref() {
            ui.small(status);
        }
    }

    fn on_key_event(&mut self, ctx: &egui::Context) -> bool {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.region_base.cancel_selection();
            self.session.clear();
            return true;
        }
        false
    }

    fn set_ai_backend_available(&mut self, available: bool) {
        self.ai_backend_available = available;
    }

    fn set_ai_backend_torch_available(&mut self, available: bool) {
        self.ai_backend_torch_available = available;
    }

    fn set_single_image_session(&mut self, single_image: bool) {
        self.single_image_session = single_image;
    }

    /// The «Библиотека знаков» panel follows the toggle the two library buttons drive. Both
    /// buttons belong to the chapter mode, which a single-image session does not offer, so the
    /// panel is never shown there.
    fn wants_library_panel(&self) -> bool {
        self.library_window.is_open() && !self.single_image_session
    }

    /// Draws the library screen into its dock panel.
    ///
    /// The intake shares the tool's OWN ring measurement and footprint limit, so the library
    /// and the chapter mode cannot disagree about either. The jobs behind it are polled from
    /// `draw_overlay_ui`, not from here: this body does not run while the panel is hidden.
    fn draw_library_panel(&mut self, ui: &mut egui::Ui) {
        let sample_params = self.settings.normalized().chapter_sample_params();
        // Rebuilt every frame rather than cached: the catalog holds a handful of marks, and a
        // cached copy would be a second answer to "which entries is this chapter hunting"
        // that could disagree with the catalog after a scan rebuilt it.
        let selected_entries = self.chapter_library_selection();
        let saved_entries = self.chapter_saved_entries();
        let context = LibraryPanelContext {
            sample_params,
            max_side: CHAPTER_MAX_TEMPLATE_SIDE,
            selected_entries: &selected_entries,
            saved_entries: &saved_entries,
            armed: self.library_arm.as_deref(),
            chapter_busy: self.chapter.busy(),
        };
        // The dock body may not touch the catalog or the canvas itself; what it asks for is
        // consumed by `draw_overlay_ui` later in this same frame.
        if let Some(request) = self.library_window.draw_panel_body(ui, &context) {
            self.library_panel_request = Some(request);
        }
    }

    fn wants_primary_stroke(&self, point: StrokePoint) -> bool {
        // A click that ends an eyedropper sampling belongs to the colour control, not to the
        // canvas: letting it through would start a selection drag under the picked pixel.
        // BOTH predicates are needed. The colour widget clears `eyedropper_active` on the very
        // frame the terminating click lands, and the dock body (where it is drawn) runs before
        // this hook, so on that frame the first flag already reads false — only
        // `primary_click_consumed_this_frame` still names the click. The typing tab guards the
        // same pair for the same reason (`ms-tab-typing/src/panel/create_state.rs`).
        if self.library_window.eyedropper_owns_primary_click() {
            return false;
        }
        self.region_base.wants_primary_stroke(point)
    }

    fn stroke_begin(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        self.region_base.begin_selection(canvas, point);
    }

    fn stroke_update(&mut self, canvas: &mut CanvasView, _from: StrokePoint, to: StrokePoint) {
        self.region_base.update_selection(canvas, to);
    }

    fn stroke_end(&mut self, canvas: &mut CanvasView) {
        self.region_base.end_selection(canvas);
    }

    fn draw_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        self.poll_settings_load();
        self.poll_and_maybe_query_status();
        self.poll_unload();
        self.poll_chapter_job(canvas);
        // The library panel's workers are polled HERE, not from its dock body: the body does
        // not run while the panel is hidden, so a job finishing after the user toggled it shut
        // would never be collected. This hook runs every frame the tool is active.
        self.poll_library_capture();
        // The capture lane has its OWN channel, so `WatermarkLibraryWindow::busy` cannot see
        // it and cannot ask for the repaint it needs. Without this the worker's answer — and
        // the row or the status it produces — waits for whatever input happens to draw the
        // next frame, which after a finished drag may be nothing at all: the user lets go of
        // the mouse and the application goes quiet holding an answer it has not painted.
        if self.library_capture_rx.is_some() {
            ctx.request_repaint();
        }
        self.library_window.poll(ctx);
        // A reservation may only outlive the surface that made it for as long as that surface
        // is the one the user is looking at: the red «Отменить выделение» is the ONLY way to
        // call it off, so a reservation drawn on no screen is one the user cannot cancel. The
        // panel decides (it owns the screen); the tool owns the reservation and drops it. This
        // runs BEFORE `run_library_panel_request` below, so a reservation the body asked for
        // this very frame is not judged against the screen it was made on.
        self.drop_reservation_without_a_screen();
        if self.library_window.take_changed() {
            // The panel wrote to disk, so the chapter mode's own copy of the entry list is
            // stale. The panel body already drew this frame (the dock runs inside
            // `CanvasView::draw`), so its flag is consumed in the same frame it was raised.
            self.chapter.library_requested = true;
        }
        // The library picker fills itself the first time the chapter mode is drawn and
        // whenever an entry was written, never on a frame that already has a job running.
        let chapter_mode = self.effective_mode() == WatermarkMode::Chapter;
        if chapter_mode
            && !self.chapter.busy()
            && (self.chapter.library_requested || !self.chapter.library_loaded)
        {
            self.request_chapter_library();
        }

        let mut settings_changed = false;
        let mut want_status = false;
        let mut unload_requested = false;
        let mut chapter_request = None;
        {
            let Self {
                region_base,
                session,
                settings,
                status,
                unload_status,
                progress,
                ai_backend_available,
                ai_backend_torch_available,
                chapter,
                library_window,
                single_image_session,
                ..
            } = self;
            let mut editor_ctx = WatermarkEditorCtx {
                session,
                settings,
                status: status.as_ref(),
                unload_status,
                progress,
                ai_backend_available: *ai_backend_available,
                ai_backend_torch_available: *ai_backend_torch_available,
                library_panel_open: library_window.is_open(),
                single_image_session: *single_image_session,
                chapter,
                settings_changed: &mut settings_changed,
                want_status: &mut want_status,
                unload_requested: &mut unload_requested,
                chapter_request: &mut chapter_request,
            };
            region_base.draw_overlay_ui(
                ctx,
                canvas,
                project,
                t!("cleaning.tools.watermark.title"),
                |editor| {
                    if editor.status.is_none() {
                        editor.status =
                            Some(t!("cleaning.tools.watermark.editor_hint_status").to_string());
                    }
                },
                |ui, editor| editor_ctx.draw_body(ui, editor),
            );
        }

        if settings_changed {
            self.dirty = true;
        }
        if want_status {
            self.status_wanted = true;
        }
        if let Some(request) = chapter_request {
            self.start_chapter_request(request, canvas, project);
        }
        // The dock body drew earlier in this same frame (it runs inside `CanvasView::draw`),
        // so whatever it asked for is acted on here, where the canvas and the project exist.
        if let Some(request) = self.library_panel_request.take() {
            self.run_library_panel_request(request, canvas, project);
        }
        // An armed drag never reached the region editor: `RegionEditToolBase` parked its rect
        // instead of loading it (`set_selection_diverted`), and this is where the tool collects
        // it. Taking the reservation before the capture starts means a capture that fails does
        // not leave the next selection silently spoken for; taking the RECT first means a rect
        // whose reservation died in the meantime is discarded here rather than stranded.
        if let Some((page_idx, rect)) = self.region_base.take_diverted_selection()
            && let Some(arm) = self.take_reservation_for_diverted(page_idx, rect)
        {
            self.start_library_capture(arm, page_idx, rect, canvas, project);
        }
        self.report_selection_refusal();
        // Last word on the diversion: whatever this frame did to the reservation — armed it,
        // cancelled it, consumed it, watched it expire — the flag now says the same thing.
        self.sync_selection_diversion();
        if unload_requested && self.unload_rx.is_none() {
            let (tx, rx) = mpsc::channel();
            self.unload_rx = Some(rx);
            thread::spawn(move || {
                let _ = tx.send(unload_watermark_model());
            });
            self.unload_status =
                Some(t!("cleaning.tools.watermark.unload_requested_status").to_string());
        }
        // The editor is gone (applied or cancelled): drop its per-session state so
        // the next region starts with an empty undo stack and no stale mask.
        if !self.region_base.has_open_editor() && self.session.scroll_id.is_some() {
            self.session.clear();
        }
        self.poll_and_maybe_save();
        // LAST, so every mutation this frame made to what the panel shows is covered: the dock
        // body already drew (it runs inside `CanvasView::draw`), so a status or a refusal row
        // recorded anywhere above needs a frame of its own to be seen at all.
        self.library_window.request_repaint_if_dirty(ctx);
    }

    /// Paints the selection cursor, in the reservation's own colour while one is live.
    ///
    /// An armed drag does something completely different from an ordinary one — it is cut out
    /// for the library and never opens the region editor — and until this existed the canvas
    /// looked identical either way: same crosshair, same black-and-white band. The only signal
    /// was the red «Отменить выделение» button, which lives on the dock panel, i.e. not where
    /// the user is looking while dragging. The accent is the CANCEL BUTTON'S own colour, so the
    /// canvas and the panel say the same thing in the same colour.
    fn draw_cursor(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<egui::Pos2>,
    ) {
        if self.library_arm.is_some() {
            self.region_base.draw_cursor_tinted(
                ui,
                canvas,
                pointer_scene_pos,
                Color32::from_rgb(ARM_CANCEL_RGB[0], ARM_CANCEL_RGB[1], ARM_CANCEL_RGB[2]),
            );
            return;
        }
        self.region_base.draw_cursor(ui, canvas, pointer_scene_pos);
    }

    /// Only the region-editor window is reported here.
    ///
    /// The library screen is a DOCK PANEL now, and a dock panel's rect reaches the tab's
    /// occlusion gate on its own: `PanelDockOutput::drawn_panels` -> `dock_panel_rects` ->
    /// `panel_rects` -> `CleaningTabState::canvas_pointer_occluded`. Reporting it a second
    /// time here would be a duplicate the tool cannot keep in sync with the dock.
    fn captures_canvas_pointer(&self, pointer_pos: egui::Pos2) -> bool {
        self.region_base.editor_window_contains(pointer_pos)
    }

    /// Only the modal region editor blocks zoom.
    ///
    /// The library panel deliberately does NOT: it is a session-long surface, and
    /// `block_canvas_zoom` also disables the clean-overlay undo shortcuts, which a panel that
    /// may stay open for a whole chapter must not do (`tools/MODULE_README.md`).
    fn block_canvas_zoom(&self) -> bool {
        self.region_base.has_open_editor()
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
    /// Whether the open project is a single-image session (no chapter mode).
    single_image_session: bool,
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

/// Draws one entry of the mode dropdown and applies a click to `selected`.
///
/// A Torch-backed mode is disabled while the backend reports no Torch and explains that on
/// hover; the AI-free chapter mode is always selectable. Deliberately the same shape as the
/// shared `mask_generation::draw_mask_source_entry`, down to the message and its colour, so
/// the two Torch gates of this tab cannot word the same refusal differently.
fn draw_mode_entry(
    ui: &mut egui::Ui,
    selected: &mut WatermarkMode,
    candidate: WatermarkMode,
    torch_available: bool,
) {
    let enabled = torch_available || !candidate.requires_torch();
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(candidate.label()).selected(*selected == candidate),
    );
    let response = if enabled {
        response
    } else {
        response.on_disabled_hover_text(
            egui::RichText::new(t!("cleaning.common.pytorch_not_installed_status"))
                .color(ms_theme::status::ERROR),
        )
    };
    if response.clicked() {
        *selected = candidate;
    }
}

impl WatermarkEditorCtx<'_> {
    /// The mode this session runs in; see `effective_watermark_mode`.
    fn effective_mode(&self) -> WatermarkMode {
        effective_watermark_mode(&self.settings.mode, self.single_image_session)
    }

    /// Draws the whole region-editor body: scrollable controls plus preview, then
    /// the run/undo action row. The status line and Отмена/Применить are appended
    /// by `RegionEditToolBase::draw_overlay_ui`.
    fn draw_body(&mut self, ui: &mut egui::Ui, editor: &mut RegionEditorSession) {
        self.session.sync_session(editor.scroll_id);
        let running = self.session.poll_run(editor);
        let mode = self.effective_mode();

        let scroll_id = editor.scroll_id;
        // Keep the action row fixed while a long parameter list scrolls, and keep
        // mouse-drag out of the scroll sources so dragging the preview does not
        // scroll the panel.
        let scroll_max_h = (ui.ctx().content_rect().height() - 200.0).max(240.0);
        egui::ScrollArea::vertical()
            .id_salt(("cleaning_watermark_body_scroll", scroll_id))
            .max_height(scroll_max_h)
            .auto_shrink([false, true])
            .scroll_source(
                egui::scroll_area::ScrollSource::SCROLL_BAR
                    | egui::scroll_area::ScrollSource::MOUSE_WHEEL,
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if RegionEditToolBase::draw_region_editor_zoom_controls(ui, editor) {
                        ui.ctx().request_repaint();
                    }
                });
                // The mode picker stays outside the collapsible sections: it decides which
                // of them is drawn at all.
                self.draw_mode_picker(ui);
                match mode {
                    WatermarkMode::MaskOnly | WatermarkMode::Clean => {
                        draw_watermark_progress_ui(ui, self.progress);
                        self.draw_params(ui, scroll_id);
                    }
                    WatermarkMode::Chapter => {
                        self.draw_chapter_progress(ui);
                        self.draw_chapter_params(ui, scroll_id);
                        self.draw_chapter_catalog(ui, editor, scroll_id);
                        self.draw_chapter_reports(ui);
                        self.draw_chapter_library(ui, scroll_id);
                    }
                }
                self.draw_preview(ui, editor);
            });

        ui.separator();
        match mode {
            WatermarkMode::MaskOnly | WatermarkMode::Clean => {
                self.draw_actions(ui, editor, running)
            }
            WatermarkMode::Chapter => self.draw_chapter_actions(ui, editor),
        }
        if running || self.chapter.busy() {
            ui.ctx().request_repaint();
        }
    }

    /// Draws the three-way mode picker. The chapter mode is local, so it also states that
    /// it needs no backend at all.
    ///
    /// The two NETWORK modes are disabled while the backend reports no Torch and say so on
    /// hover; «По главе (точное вычитание)» is always selectable. A persisted selection is
    /// never forced to change by this — a settings file naming a network mode still opens,
    /// with the run button explaining why it cannot run.
    fn draw_mode_picker(&mut self, ui: &mut egui::Ui) {
        let effective = self.effective_mode();
        let mut mode = effective;
        let torch_available = self.ai_backend_torch_available;
        let offered = offered_watermark_modes(self.single_image_session);
        ui.horizontal(|ui| {
            ui.label(t!("cleaning.common.mode_label"));
            WheelComboBox::from_id_salt("cleaning_watermark_mode_picker")
                .selected_text(mode.label())
                .show_ui(ui, |ui| {
                    for &candidate in offered {
                        draw_mode_entry(ui, &mut mode, candidate, torch_available);
                    }
                });
        });
        // Compared with the EFFECTIVE mode, not the persisted string: a single-image session
        // shows a persisted «По главе» as «Только маска», and only a real pick may overwrite it.
        if mode != effective {
            self.settings.mode = mode.wire().to_string();
            *self.settings_changed = true;
        }
        match mode {
            WatermarkMode::MaskOnly => {}
            WatermarkMode::Clean => {
                ui.colored_label(
                    ms_theme::status::WARNING,
                    t!("cleaning.tools.watermark.experimental_warning"),
                );
            }
            WatermarkMode::Chapter => {
                ui.small(t!("cleaning.tools.watermark.chapter.description_hint"));
            }
        }
    }

    /// Draws the collapsed-by-default «Параметры (удаление водяных знаков)»
    /// section: model, mode, tiling, mask parameters, preview toggle and the
    /// backend catalog/unload controls.
    fn draw_params(&mut self, ui: &mut egui::Ui, scroll_id: u64) {
        let settings = &mut *self.settings;
        let unload_status = &mut *self.unload_status;
        let changed = &mut *self.settings_changed;
        let want_status = &mut *self.want_status;
        let unload_requested = &mut *self.unload_requested;
        let status = self.status;
        RegionEditToolBase::draw_region_editor_collapsible_section(
            ui,
            ("cleaning_watermark_params", scroll_id),
            t!("cleaning.tools.watermark.params_heading"),
            false,
            |ui| {
                // The model id is the persisted identity, so the picker works on
                // the catalog's `&'static str` and the settings string follows it.
                let mut selected_model = watermark_model_spec(&settings.model).id;
                draw_watermark_model_picker_ui(ui, &mut selected_model, status);
                if selected_model != settings.model {
                    settings.model = selected_model.to_string();
                    *changed = true;
                }

                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.tile, WATERMARK_TILE_MIN..=WATERMARK_TILE_MAX)
                            .text(t!("cleaning.tools.watermark.tile_label")),
                    )
                    .on_hover_text(t!("cleaning.tools.watermark.tile_hint"))
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.overlap, 0..=WATERMARK_OVERLAP_MAX)
                            .text(t!("cleaning.tools.watermark.overlap_label")),
                    )
                    .on_hover_text(t!("cleaning.tools.watermark.overlap_hint"))
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(
                            &mut settings.threshold,
                            WATERMARK_THRESHOLD_MIN..=WATERMARK_THRESHOLD_MAX,
                        )
                        .text(t!("cleaning.tools.watermark.threshold_label")),
                    )
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.dilate_px, 0..=WATERMARK_DILATE_MAX)
                            .text(t!("cleaning.common.mask_expand_label")),
                    )
                    .changed();
                *changed |= ui
                    .checkbox(
                        &mut settings.show_mask_preview,
                        t!("cleaning.tools.watermark.show_mask_label"),
                    )
                    .changed();

                if ui
                    .small_button(t!("cleaning.tools.watermark.refresh_status_button"))
                    .clicked()
                {
                    *want_status = true;
                }
                if ui
                    .small_button(t!("cleaning.tools.watermark.unload_button"))
                    .clicked()
                {
                    *unload_requested = true;
                }
                if let Some(status) = unload_status.as_ref() {
                    ui.small(status);
                }
            },
        );
    }

    /// Draws the region image with the predicted mask painted over it when the
    /// preview toggle is on.
    fn draw_preview(&mut self, ui: &mut egui::Ui, editor: &mut RegionEditorSession) {
        RegionEditToolBase::ensure_region_editor_texture(editor, ui.ctx());
        self.session.ensure_mask_texture(ui.ctx(), editor.scroll_id);
        // Cloned so the draw closure does not borrow `self` while `editor` is
        // borrowed; a `TextureHandle` clone is a refcount bump.
        let mask_texture = self
            .settings
            .show_mask_preview
            .then(|| self.session.mask_texture.clone())
            .flatten();
        let preview_size = editor.zoomed_image_size();
        let scroll_id = editor.scroll_id;
        RegionEditToolBase::draw_region_editor_scroll_area(ui, scroll_id, preview_size, |ui| {
            let Some(texture) = editor.texture.as_ref() else {
                return;
            };
            let response = ui.add(egui::Image::new((texture.id(), preview_size)));
            if let Some(mask_texture) = mask_texture.as_ref() {
                ui.painter().image(
                    mask_texture.id(),
                    response.rect,
                    Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        });
    }

    /// Draws the chapter job's progress bar, if one is running.
    fn draw_chapter_progress(&mut self, ui: &mut egui::Ui) {
        let Some((done, total, label)) = self.chapter.progress.clone() else {
            return;
        };
        // Cast justification: both are page counts of one chapter, far below 2^24.
        let fraction = if total > 0 {
            (done as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        ui.add(egui::ProgressBar::new(fraction).text(tf!(
            "cleaning.tools.watermark.chapter.job_progress",
            label = label,
            done = done,
            total = total
        )));
        ui.ctx().request_repaint();
    }

    /// Draws the collapsed-by-default «Параметры (по главе)» section.
    ///
    /// Only the three parameters a real source can legitimately need are exposed. The gain
    /// window is NOT among them: the plan's measurement is that a source with fat solid
    /// glyphs needs a larger background radius, never a looser gain window, and the engine
    /// clamps every value handed to it to its own measured bounds anyway.
    fn draw_chapter_params(&mut self, ui: &mut egui::Ui, scroll_id: u64) {
        let settings = &mut *self.settings;
        let changed = &mut *self.settings_changed;
        RegionEditToolBase::draw_region_editor_collapsible_section(
            ui,
            ("cleaning_watermark_chapter_params", scroll_id),
            t!("cleaning.tools.watermark.chapter.params_heading"),
            false,
            |ui| {
                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.chapter_anchor_tolerance_px, 0..=16)
                            .text(t!("cleaning.tools.watermark.chapter.anchor_tolerance_label")),
                    )
                    .on_hover_text(t!("cleaning.tools.watermark.chapter.anchor_tolerance_hint"))
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.chapter_background_blur_px, 1..=32)
                            .text(t!("cleaning.tools.watermark.chapter.blur_radius_label")),
                    )
                    .on_hover_text(t!("cleaning.tools.watermark.chapter.blur_radius_hint"))
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.chapter_ring_width_px, 1..=16)
                            .text(t!("cleaning.tools.watermark.chapter.ring_width_label")),
                    )
                    .on_hover_text(t!("cleaning.tools.watermark.chapter.ring_width_hint"))
                    .changed();
            },
        );
    }

    /// Draws the catalog of marks: selection, name, per-mark measurements, verdict and the
    /// model preview.
    ///
    /// The whole section is read-only while a job runs, because a worker owns a copy of the
    /// catalog and will hand back its own version when it finishes.
    fn draw_chapter_catalog(
        &mut self,
        ui: &mut egui::Ui,
        editor: &mut RegionEditorSession,
        scroll_id: u64,
    ) {
        let busy = self.chapter.busy();
        let chapter = &mut *self.chapter;
        let request = &mut *self.chapter_request;
        RegionEditToolBase::draw_region_editor_collapsible_section(
            ui,
            ("cleaning_watermark_chapter_marks", scroll_id),
            t!("cleaning.tools.watermark.chapter.marks_heading"),
            true,
            |ui| {
                if chapter.catalog.is_empty() {
                    ui.small(t!("cleaning.tools.watermark.chapter.no_marks_hint"));
                    return;
                }
                let mut remove: Option<usize> = None;
                for index in 0..chapter.catalog.len() {
                    ui.push_id(index, |ui| {
                        ui.separator();
                        ui.horizontal(|ui| {
                            let selected = chapter.selected == index;
                            if ui
                                .add_enabled(!busy, egui::RadioButton::new(selected, ""))
                                .clicked()
                            {
                                chapter.selected = index;
                            }
                            ui.add_enabled_ui(!busy, |ui| {
                                // The name is user data: whatever is typed is kept verbatim.
                                ui.add(
                                    egui::TextEdit::singleline(
                                        &mut chapter.catalog.marks[index].name,
                                    )
                                    .id_salt(("cleaning_watermark_chapter_mark_name", index))
                                    .desired_width(200.0),
                                );
                            });
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(t!(
                                        "cleaning.tools.watermark.chapter.library_save_button"
                                    )),
                                )
                                .clicked()
                            {
                                *request = Some(ChapterRequest::SaveLibrary(index));
                            }
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(t!(
                                        "cleaning.tools.watermark.chapter.remove_mark_button"
                                    )),
                                )
                                .clicked()
                            {
                                remove = Some(index);
                            }
                        });
                        Self::draw_chapter_mark_body(ui, chapter, index, busy, request);
                    });
                }
                if let Some(index) = remove {
                    // Drop the mark's preview textures with it, or the map would keep one
                    // entry per mark ever created in this session.
                    if let Some(kind) = chapter.catalog.kinds.get(index) {
                        let id = kind.id().to_string();
                        chapter.textures.remove(&id);
                    }
                    chapter.catalog.remove(index);
                    chapter.selected = chapter.selected.min(chapter.catalog.len().saturating_sub(1));
                    chapter.invalidate_scan();
                }
            },
        );
        // The editor's status line doubles as the chapter status line while this mode is
        // active, so a finished job is visible without hunting for the tool panel.
        if let Some(status) = chapter.status.as_ref() {
            editor.status = Some(status.clone());
        }
    }

    /// Draws one mark's measurements, verdict, suggested fix, library match and preview.
    fn draw_chapter_mark_body(
        ui: &mut egui::Ui,
        chapter: &mut ChapterState,
        index: usize,
        busy: bool,
        request: &mut Option<ChapterRequest>,
    ) {
        let Some(kind) = chapter.catalog.kinds.get(index) else {
            return;
        };
        // A mark rebuilt from a library entry whose background was hand-stated keeps the
        // verdict the engine gave it, so the assertion has to be read off the model's
        // provenance: without it this editor would claim a measurement the card warns about.
        let rests_on_assertion = kind
            .model()
            .is_some_and(|model| model.provenance().manual_backgrounds > 0);
        let conditioning_lines = describe_conditioning(kind.conditioning(), rests_on_assertion);
        let noise_gain = kind.model().map(WatermarkModel::max_noise_gain);
        let template_size = (kind.template().width(), kind.template().height());
        let anchors = kind.template().anchor_key();
        let calibration_samples = kind.samples().len();
        let Some(mark) = chapter.catalog.marks.get(index) else {
            return;
        };
        ui.small(tf!(
            "cleaning.tools.watermark.chapter.mark_stats",
            width = template_size.0,
            height = template_size.1,
            calibration = calibration_samples,
            template_only = mark.template_only,
            occurrences = mark.occurrences
        ));
        ui.small(tf!(
            "cleaning.tools.watermark.chapter.anchors_line",
            anchors = anchors
        ));
        for line in conditioning_lines {
            ui.small(line);
        }
        if let Some(gain) = noise_gain {
            ui.small(tf!(
                "cleaning.tools.watermark.chapter.noise_gain_line",
                gain = format!("{gain:.2}")
            ));
        }
        if mark.occurrences > 0 {
            // Model quality is the DETECTION evidence — gain and t — never the
            // recomposition residual, which only measures quantization and clipping.
            ui.small(tf!(
                "cleaning.tools.watermark.chapter.quality_line",
                gain = format!("{:.2}", mark.gain_mean),
                snr = format!("{:.0}", mark.snr_mean),
                count = mark.occurrences - mark.unverified
            ));
            if mark.unverified > 0 {
                ui.small(tf!(
                    "cleaning.tools.watermark.chapter.unverified_line",
                    count = mark.unverified
                ));
            }
            let histogram = describe_histogram(&mark.histogram);
            if !histogram.is_empty() {
                ui.small(tf!(
                    "cleaning.tools.watermark.chapter.histogram_line",
                    bins = histogram
                ));
            }
        }
        if let Some(verdict) = mark.last_verdict.as_ref() {
            ui.small(describe_sample_verdict(verdict));
        }
        Self::draw_chapter_match_picker(ui, mark, index, busy, request);
        Self::draw_chapter_preview(ui, chapter, index);
    }

    /// Draws the library auto-match row of one mark: which entry supplies its calibration,
    /// and the override that lets the user pick another one or none at all.
    ///
    /// Matching is shape-independent by design, so the row states the EVIDENCE — the entry's
    /// calibration quality and the measured opacity gain against this mark's own deposit —
    /// rather than just a name.
    fn draw_chapter_match_picker(
        ui: &mut egui::Ui,
        mark: &ChapterMark,
        index: usize,
        busy: bool,
        request: &mut Option<ChapterRequest>,
    ) {
        if mark.matches.is_empty() {
            return;
        }
        let current = mark.adopted_entry.clone();
        let selected_text = match current.as_ref() {
            Some(entry_id) => mark
                .matches
                .iter()
                .find(|candidate| &candidate.entry_id == entry_id)
                .map_or_else(|| entry_id.clone(), |candidate| candidate.name.clone()),
            None => t!("cleaning.tools.watermark.chapter.library_match_none").to_string(),
        };
        let mut chosen: Option<Option<String>> = None;
        ui.horizontal(|ui| {
            ui.small(t!("cleaning.tools.watermark.chapter.library_match_label"));
            // Read-only while a worker owns a copy of the catalog: it will hand back its own
            // version, and a choice made against this one would be thrown away.
            ui.add_enabled_ui(!busy, |ui| {
                WheelComboBox::from_id_salt(("cleaning_watermark_chapter_match", index))
                    .selected_text(selected_text)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(
                                current.is_none(),
                                t!("cleaning.tools.watermark.chapter.library_match_none"),
                            )
                            .clicked()
                        {
                            chosen = Some(None);
                        }
                        for candidate in &mark.matches {
                            let label = tf!(
                                "cleaning.tools.watermark.chapter.library_match_option",
                                name = candidate.name.clone(),
                                quality = library_quality_label(&candidate.verdict),
                                gain = format!("{:.2}", candidate.gain)
                            );
                            if ui
                                .selectable_label(
                                    current.as_deref() == Some(candidate.entry_id.as_str()),
                                    label,
                                )
                                .clicked()
                            {
                                chosen = Some(Some(candidate.entry_id.clone()));
                            }
                        }
                    });
            });
        });
        if let Some(entry) = chosen
            && entry != current
        {
            *request = Some(ChapterRequest::UseLibraryMatch { index, entry });
        }
    }

    /// Draws the alpha-map and imprint thumbnails of a fitted model, uploading their
    /// textures on first use and whenever the model was refitted.
    fn draw_chapter_preview(ui: &mut egui::Ui, chapter: &mut ChapterState, index: usize) {
        let Some(id) = chapter.catalog.kinds.get(index).map(|kind| kind.id().to_string()) else {
            return;
        };
        let Some(mark) = chapter.catalog.marks.get(index) else {
            return;
        };
        let Some(preview) = mark.preview.as_ref() else {
            return;
        };
        let revision = mark.preview_revision;
        let stale = !matches!(chapter.textures.get(&id), Some(textures) if textures.revision == revision);
        if stale {
            let alpha = preview.alpha.clone();
            let imprint = preview.imprint.clone();
            let alpha = ui.ctx().load_texture(
                format!("cleaning-watermark-chapter-alpha-{id}"),
                alpha,
                TextureOptions::NEAREST,
            );
            let imprint = ui.ctx().load_texture(
                format!("cleaning-watermark-chapter-imprint-{id}"),
                imprint,
                TextureOptions::NEAREST,
            );
            chapter.textures.insert(
                id.clone(),
                ChapterPreviewTextures {
                    revision,
                    alpha,
                    imprint,
                },
            );
        }
        let Some(textures) = chapter.textures.get(&id) else {
            return;
        };
        let size = egui::vec2(CHAPTER_PREVIEW_SIDE, CHAPTER_PREVIEW_SIDE);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.small(t!("cleaning.tools.watermark.chapter.preview_alpha_label"));
                ui.add(egui::Image::new((textures.alpha.id(), size)));
            });
            ui.vertical(|ui| {
                ui.small(t!("cleaning.tools.watermark.chapter.preview_imprint_label"));
                ui.add(egui::Image::new((textures.imprint.id(), size)));
            });
        });
    }

    /// Draws the totals of the last scan and the last apply.
    fn draw_chapter_reports(&mut self, ui: &mut egui::Ui) {
        if let Some(report) = self.chapter.scan_report {
            ui.separator();
            ui.small(tf!(
                "cleaning.tools.watermark.chapter.scan_done_status",
                found = report.found,
                pages = report.pages,
                unverified = report.unverified
            ));
            if report.matched > 0 {
                ui.small(tf!(
                    "cleaning.tools.watermark.chapter.library_matched_line",
                    count = report.matched
                ));
            }
        }
        if let Some(report) = self.chapter.apply_report {
            ui.small(tf!(
                "cleaning.tools.watermark.chapter.apply_done_status",
                pages = report.pages,
                removed = report.removed,
                refused = report.refused
            ));
            ui.small(describe_residual(&report.residual));
            if report.failed_patches > 0 {
                ui.colored_label(
                    ms_theme::status::WARNING,
                    tf!(
                        "cleaning.tools.watermark.chapter.apply_failed_patches",
                        count = report.failed_patches
                    ),
                );
            }
        }
    }

    /// Draws the library picker: refresh, the entry list and a load button per entry.
    fn draw_chapter_library(&mut self, ui: &mut egui::Ui, scroll_id: u64) {
        let busy = self.chapter.busy();
        let library_panel_open = self.library_panel_open;
        let chapter = &mut *self.chapter;
        let request = &mut *self.chapter_request;
        RegionEditToolBase::draw_region_editor_collapsible_section(
            ui,
            ("cleaning_watermark_chapter_library", scroll_id),
            t!("cleaning.tools.watermark.chapter.library_heading"),
            false,
            |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(t!(
                                "cleaning.tools.watermark.chapter.library_refresh_button"
                            )),
                        )
                        .clicked()
                    {
                        *request = Some(ChapterRequest::RefreshLibrary);
                    }
                    // A TOGGLE drawn pressed while the panel is open: a dock tab has no
                    // close affordance of its own, so this button is the whole story.
                    if ui
                        .add(
                            egui::Button::new(t!(
                                "cleaning.tools.watermark.chapter.library_manage_button"
                            ))
                            .selected(library_panel_open),
                        )
                        .on_hover_text(t!(
                            "cleaning.tools.watermark.chapter.library_manage_hint"
                        ))
                        .clicked()
                    {
                        *request = Some(ChapterRequest::ToggleLibraryPanel);
                    }
                });
                if chapter.library.is_empty() {
                    ui.small(t!("cleaning.tools.watermark.chapter.library_empty_hint"));
                    return;
                }
                for entry in &chapter.library {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(t!(
                                    "cleaning.tools.watermark.chapter.library_load_button"
                                )),
                            )
                            .clicked()
                        {
                            *request = Some(ChapterRequest::LoadLibrary(entry.id.clone()));
                        }
                        ui.small(tf!(
                            "cleaning.tools.watermark.chapter.library_entry_line",
                            name = entry.name.clone(),
                            width = entry.width,
                            height = entry.height,
                            anchors = entry.anchor_key.clone(),
                            samples = entry.samples
                        ));
                    });
                    if !entry.sources.is_empty() {
                        ui.small(tf!(
                            "cleaning.tools.watermark.chapter.library_sources_line",
                            sources = entry
                                .sources
                                .iter()
                                .map(|source| format!(
                                    "{} ({} px, {})",
                                    source.source_key, source.page_width, source.anchor_key
                                ))
                                .collect::<Vec<_>>()
                                .join("; ")
                        ));
                    }
                }
            },
        );
    }

    /// Draws the chapter action row: add a mark, add a sample, scan, apply.
    fn draw_chapter_actions(&mut self, ui: &mut egui::Ui, editor: &mut RegionEditorSession) {
        let busy = self.chapter.busy();
        let has_marks = !self.chapter.catalog.is_empty();
        let has_hits = !self.chapter.hits.is_empty();
        let selected = self.chapter.selected;
        let page_idx = editor.page_idx;
        let rect = editor.target_rect_px;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!("cleaning.tools.watermark.chapter.add_mark_button")),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.add_mark_hint"))
                .on_disabled_hover_text(t!(
                    "cleaning.mask_editor.processing_already_running_status"
                ))
                .clicked()
            {
                *self.chapter_request = Some(ChapterRequest::Sample {
                    page_idx,
                    rect,
                    target: None,
                });
            }
            if ui
                .add_enabled(
                    !busy && has_marks,
                    egui::Button::new(t!("cleaning.tools.watermark.chapter.add_sample_button")),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.add_sample_hint"))
                .clicked()
            {
                *self.chapter_request = Some(ChapterRequest::Sample {
                    page_idx,
                    rect,
                    target: Some(selected),
                });
            }
            if ui
                .add_enabled(
                    !busy && has_marks,
                    egui::Button::new(t!("cleaning.tools.watermark.chapter.scan_button")),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.scan_hint"))
                .clicked()
            {
                *self.chapter_request = Some(ChapterRequest::Scan);
            }
            if ui
                .add_enabled(
                    !busy && has_hits,
                    egui::Button::new(t!("cleaning.tools.watermark.chapter.apply_button")),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.apply_hint"))
                .clicked()
            {
                *self.chapter_request = Some(ChapterRequest::Apply);
            }
            if busy {
                ui.spinner();
            }
        });
    }

    /// Draws the run / undo / cancel row. The run button names what the current
    /// mode will do and explains on hover why it is disabled.
    ///
    /// Both network modes need a reachable backend AND a Torch runtime. Since
    /// `CleaningTool::pytorch_required` is a constant `false` — the tool must stay selectable
    /// for its AI-free chapter mode — this is the gate that keeps a Torch-less machine from
    /// firing a request that can only fail. The hint is three-way in the same order as the
    /// shared `mask_generation::generate_button_hover_text`: backend first, Torch second,
    /// what the run does last.
    fn draw_actions(&mut self, ui: &mut egui::Ui, editor: &mut RegionEditorSession, running: bool) {
        let Some(mode) = self.effective_mode().network() else {
            return;
        };
        let backend_available = self.ai_backend_available;
        let torch_available = self.ai_backend_torch_available;
        let can_run = !running && backend_available && torch_available;
        let can_undo = !running && !self.session.undo_stack.is_empty();
        ui.horizontal(|ui| {
            let run_hint = if !backend_available {
                t!("cleaning.mask_editor.backend_unavailable_status")
            } else if !torch_available {
                t!("cleaning.common.pytorch_not_installed_status")
            } else {
                t!("cleaning.tools.watermark.run_hint")
            };
            if ui
                .add_enabled(can_run, egui::Button::new(mode.run_button_label()))
                .on_hover_text(run_hint)
                // `on_hover_text` is enabled-only, so the reason the button is
                // greyed out needs the disabled variant too.
                .on_disabled_hover_text(run_hint)
                .clicked()
            {
                // A run may download code or weights; re-check the catalog after it.
                *self.want_status = true;
                self.session
                    .start_run(editor, self.settings, self.progress, mode);
            }
            if ui
                .add_enabled(
                    can_undo,
                    egui::Button::new(t!("cleaning.mask_editor.revert_button")),
                )
                .clicked()
            {
                self.session.undo_last_run(editor);
            }
            if running {
                ui.spinner();
                if ui
                    .button(t!("cleaning.mask_editor.cancel_processing_button"))
                    .on_hover_text(t!("cleaning.mask_editor.cancel_processing_tooltip"))
                    .clicked()
                {
                    self.session.run_rx = None;
                    editor.status =
                        Some(t!("cleaning.mask_editor.processing_cancelled_status").to_string());
                }
            }
        });
    }
}

/// Runs one watermark pass on `image` and returns the outcome.
///
/// `settings` must already be `normalized()`. Mask-only mode streams
/// `watermark.detect` and returns the mask alone; clean mode streams
/// `watermark.remove` and returns the reconstruction plus its mask. `progress` is
/// updated from the `progress` frames and is always cleared before returning,
/// including on failure, so the editor never keeps a stuck progress bar.
///
/// # Errors
/// Returns a user-facing message when the region is empty, when the backend
/// fails or is unreachable, when the response blob does not match its declared
/// lengths, or when a returned PNG has the wrong size.
fn run_watermark(
    image: &egui::ColorImage,
    settings: &WatermarkRemovalSettings,
    progress: &Arc<Mutex<WatermarkProgress>>,
    mode: WatermarkNetworkMode,
) -> Result<WatermarkRunOutcome, String> {
    if image.size[0] == 0 || image.size[1] == 0 {
        return Err(t!("cleaning.region.invalid_selection_size_error").to_string());
    }
    let image_png = encode_color_image_png_rgba(image)?;
    let header = watermark_request_header(mode, settings);

    {
        let mut guard = lock_watermark_progress(progress);
        guard.active = true;
        guard.phase = "generate".to_string();
        guard.step = 0;
        guard.total = 0;
        guard.label = t!("cleaning.tools.watermark.preparing_status").to_string();
    }
    let stream_result = watermark_stream_call(
        mode.ipc_method(),
        header,
        &image_png,
        |phase, step, total, label| {
            let mut guard = lock_watermark_progress(progress);
            guard.phase = phase;
            guard.step = step;
            guard.total = total;
            guard.label = label;
        },
    );
    {
        let mut guard = lock_watermark_progress(progress);
        guard.active = false;
    }

    let (response_header, blob) = stream_result?;
    match mode {
        WatermarkNetworkMode::MaskOnly => Ok(WatermarkRunOutcome {
            image: None,
            mask: decode_region_mask(&blob, image.size)?,
        }),
        WatermarkNetworkMode::Clean => {
            let (clean_png, mask_png) = split_watermark_remove_blob(&response_header, &blob)?;
            Ok(WatermarkRunOutcome {
                image: Some(decode_region_rgba(clean_png, image.size)?),
                mask: decode_region_mask(mask_png, image.size)?,
            })
        }
    }
}

/// Builds the request header of a run. `model` is the wire id of the catalog
/// entry; the detect pass is not tiled (plan §3.3) and takes the shared downscale
/// target instead of the tile geometry.
fn watermark_request_header(
    mode: WatermarkNetworkMode,
    settings: &WatermarkRemovalSettings,
) -> Value {
    match mode {
        WatermarkNetworkMode::MaskOnly => json!({
            "params": {
                "model": settings.model,
                "downscale_to": WATERMARK_DETECT_DOWNSCALE_TO,
                "threshold": settings.threshold,
                "dilate_px": settings.dilate_px,
            }
        }),
        WatermarkNetworkMode::Clean => json!({
            "params": {
                "model": settings.model,
                "tile": settings.tile,
                "overlap": settings.overlap,
                "threshold": settings.threshold,
                "dilate_px": settings.dilate_px,
            }
        }),
    }
}

/// Issues a streaming `watermark.*` request. Each `progress` frame carries
/// `phase`/`step`/`total`/`label` in its header and no blob.
///
/// # Errors
/// Returns the backend error message, the abort notice for an interrupted call,
/// or the unified offline message when the transport fails.
fn watermark_stream_call<F>(
    method: &str,
    header: Value,
    blob: &[u8],
    mut on_progress: F,
) -> Result<(Value, Vec<u8>), String>
where
    F: FnMut(String, u64, u64, String),
{
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call_streaming(
            method,
            header,
            blob,
            |progress_header, _preview_blob| {
                let phase = progress_header
                    .get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or("generate")
                    .to_string();
                let step = progress_header
                    .get("step")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let total = progress_header
                    .get("total")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let label = progress_header
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                on_progress(phase, step, total, label);
            },
            WATERMARK_RUN_CALL_TIMEOUT,
        )
        .map_err(map_watermark_call_error)
}

/// Splits the `watermark.remove` response blob into `(clean_png, mask_png)`.
///
/// The blob is `clean_png ++ mask_png` and the header declares both lengths. They
/// are validated with STRICT equality against the blob length, so a truncated or
/// padded frame is rejected instead of being sliced into garbage.
///
/// # Errors
/// Returns a user-facing message when `image_len`/`mask_len` are missing, are not
/// non-negative integers, are zero, or do not add up to exactly `blob.len()`.
fn split_watermark_remove_blob<'a>(
    header: &Value,
    blob: &'a [u8],
) -> Result<(&'a [u8], &'a [u8]), String> {
    let image_len = header_len_field(header, "image_len")?;
    let mask_len = header_len_field(header, "mask_len")?;
    if image_len == 0 || mask_len == 0 {
        return Err(t!("cleaning.tools.watermark.empty_result_error").to_string());
    }
    let expected = image_len
        .checked_add(mask_len)
        .ok_or_else(|| t!("cleaning.tools.watermark.blob_header_error").to_string())?;
    if expected != blob.len() {
        return Err(tf!(
            "cleaning.tools.watermark.blob_length_error",
            actual = blob.len(),
            expected = expected
        ));
    }
    Ok((&blob[..image_len], &blob[image_len..]))
}

/// Reads a `*_len` response-header field as a `usize`.
///
/// # Errors
/// Returns the malformed-header message when the field is missing, is not an
/// unsigned integer, or does not fit into `usize`.
fn header_len_field(header: &Value, field: &str) -> Result<usize, String> {
    header
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| t!("cleaning.tools.watermark.blob_header_error").to_string())
}

/// Decodes the reconstructed region PNG and checks it against the region size.
///
/// # Errors
/// Returns a user-facing message for an empty payload, a corrupt PNG, or a size
/// that does not match `expected`.
fn decode_region_rgba(bytes: &[u8], expected: [usize; 2]) -> Result<egui::ColorImage, String> {
    if bytes.is_empty() {
        return Err(t!("cleaning.inpaint.no_png_result_error").to_string());
    }
    let rgba = image::load_from_memory(bytes)
        .map_err(|err| tf!("cleaning.inpaint.corrupt_png_error", err = err))?
        .to_rgba8();
    let width = usize::try_from(rgba.width())
        .map_err(|_| t!("cleaning.png.image_width_too_large_error").to_string())?;
    let height = usize::try_from(rgba.height())
        .map_err(|_| t!("cleaning.png.image_height_too_large_error").to_string())?;
    if [width, height] != expected {
        return Err(tf!(
            "cleaning.inpaint.unexpected_size_error",
            out_w = width,
            out_h = height,
            width = expected[0],
            height = expected[1]
        ));
    }
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width, height],
        rgba.as_raw(),
    ))
}

/// Decodes the predicted L8 mask PNG into a binary mask in region coordinates
/// (opaque white = watermark, transparent = untouched).
///
/// # Errors
/// Returns a user-facing message for an empty payload, a mask whose size does not
/// match `expected`, or a buffer whose length does not match its own size.
fn decode_region_mask(bytes: &[u8], expected: [usize; 2]) -> Result<egui::ColorImage, String> {
    if bytes.is_empty() {
        return Err(t!("cleaning.tools.watermark.no_mask_result_error").to_string());
    }
    let method_label = t!("cleaning.mask_editor.source.watermark");
    let (mask_size, mask_alpha) = decode_mask_png(bytes)?;
    let width = usize::try_from(mask_size[0]).map_err(|_| {
        tf!(
            "cleaning.mask_editor.mask_width_too_large_error",
            method_label = method_label
        )
    })?;
    let height = usize::try_from(mask_size[1]).map_err(|_| {
        tf!(
            "cleaning.mask_editor.mask_height_too_large_error",
            method_label = method_label
        )
    })?;
    if [width, height] != expected {
        return Err(tf!(
            "cleaning.mask_editor.mask_size_error",
            method_label = method_label,
            mask_w = width,
            mask_h = height,
            image = expected[0],
            image_2 = expected[1]
        ));
    }
    let expected_len = width.saturating_mul(height);
    if expected_len != mask_alpha.len() {
        return Err(tf!(
            "cleaning.mask_editor.mask_length_error",
            method_label = method_label,
            actual_len = mask_alpha.len(),
            expected_len = expected_len
        ));
    }
    let mut mask = egui::ColorImage::filled([width, height], Color32::TRANSPARENT);
    for (dst, alpha) in mask.pixels.iter_mut().zip(mask_alpha) {
        if alpha != 0 {
            *dst = Color32::from_rgba_unmultiplied(255, 255, 255, 255);
        }
    }
    Ok(mask)
}

/// Asks the backend to drop the resident watermark model.
///
/// # Errors
/// Returns the backend error message, or the unified offline message when the
/// backend cannot be reached.
fn unload_watermark_model() -> Result<(), String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call(
            backend_ipc::protocol::METHOD_WATERMARK_UNLOAD,
            json!({}),
            &[],
            WATERMARK_UNLOAD_CALL_TIMEOUT,
        )
        .map_err(map_watermark_call_error)?;
    Ok(())
}

/// Loads the settings file, falling back to the defaults for a missing or
/// unreadable file and for JSON that does not parse.
fn load_watermark_settings() -> WatermarkRemovalSettings {
    let path = config::watermark_removal_settings_path();
    let Ok(raw) = fs::read_to_string(&path) else {
        return WatermarkRemovalSettings::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Writes the settings file, creating its parent directory if needed.
///
/// # Errors
/// Returns a user-facing message when the directory cannot be created, the value
/// cannot be serialized, or the file cannot be written.
fn save_watermark_settings(settings: &WatermarkRemovalSettings) -> Result<(), String> {
    let path = config::watermark_removal_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    }
    let raw = serde_json::to_string_pretty(settings)
        .map_err(|err| tf!("cleaning.tools.watermark.serialize_settings_error", err = err))?;
    fs::write(&path, raw)
        .map_err(|err| tf!("cleaning.tools.watermark.write_settings_error", err = err))
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the tests build library records by hand; the product code reaches them through
    // `watermark_entry`, which is why this is not in the module's own import list.
    use super::super::watermark_library::StoredSignature;
    // The intake itself is driven only here, to prove the CAPTURE lane's own output is
    // acceptable to it; the product code runs it from the library panel's worker.
    use super::super::watermark_entry::{ReferenceIntakeRequest, run_reference_intake};

    #[test]
    fn mode_wire_roundtrip() {
        assert_eq!(WatermarkMode::from_wire("clean"), WatermarkMode::Clean);
        assert_eq!(
            WatermarkMode::from_wire("mask_only"),
            WatermarkMode::MaskOnly
        );
        // An unknown persisted value must fall back to the SAFE mode, never to
        // the experimental one.
        assert_eq!(WatermarkMode::from_wire("bogus"), WatermarkMode::MaskOnly);
        assert_eq!(WatermarkMode::from_wire("chapter"), WatermarkMode::Chapter);
        assert_eq!(WatermarkMode::MaskOnly.wire(), "mask_only");
        assert_eq!(WatermarkMode::Clean.wire(), "clean");
        assert_eq!(WatermarkMode::Chapter.wire(), "chapter");
        assert_eq!(
            WatermarkNetworkMode::MaskOnly.ipc_method(),
            backend_ipc::protocol::METHOD_WATERMARK_DETECT
        );
        assert_eq!(
            WatermarkNetworkMode::Clean.ipc_method(),
            backend_ipc::protocol::METHOD_WATERMARK_REMOVE
        );
        // The chapter mode is local: it has no backend method and needs no Torch. This
        // mapping now gates the mode picker entries and the run button INSIDE the tool,
        // not the tool's selectability — see `the_tool_is_never_torch_gated`.
        assert_eq!(WatermarkMode::Chapter.network(), None);
        assert!(!WatermarkMode::Chapter.requires_torch());
        assert!(WatermarkMode::MaskOnly.requires_torch());
        assert!(WatermarkMode::Clean.requires_torch());
    }

    /// The picker BUTTON must never be Torch-gated, whatever mode is persisted.
    ///
    /// `pytorch_required` feeds both the button's `AiRequirement::Torch` badge and
    /// `CleaningTabState::ensure_active_tool_available`, which runs every frame. While it
    /// followed the mode, the persisted default (`mask_only`) disabled the button on a
    /// Torch-less machine — hiding the one mode that needs nothing — and selecting a network
    /// mode force-deselected the tool on the next frame. The gate lives inside the tool now
    /// (`draw_mode_picker`, `draw_actions`), so this answer is a constant.
    #[test]
    fn the_tool_is_never_torch_gated() {
        let mut tool = WatermarkRemovalTool::default();
        for mode in [
            WatermarkMode::MaskOnly,
            WatermarkMode::Clean,
            WatermarkMode::Chapter,
        ] {
            tool.settings.mode = mode.wire().to_string();
            assert!(
                !tool.pytorch_required(),
                "{mode:?} must not make the tool button Torch-gated"
            );
        }
        // Including an unknown persisted value, which falls back to `mask_only`.
        tool.settings.mode = "bogus".to_string();
        assert!(!tool.pytorch_required());
    }

    /// A single-image session never runs the chapter mode — it would key the global library by
    /// the scratch directory — and never offers it, while a project keeps every mode.
    #[test]
    fn a_single_image_session_never_runs_or_offers_the_chapter_mode() {
        assert_eq!(effective_watermark_mode("chapter", true), WatermarkMode::MaskOnly);
        assert_eq!(effective_watermark_mode("chapter", false), WatermarkMode::Chapter);
        assert_eq!(effective_watermark_mode("clean", true), WatermarkMode::Clean);
        assert_eq!(effective_watermark_mode("mask_only", true), WatermarkMode::MaskOnly);
        assert!(!offered_watermark_modes(true).contains(&WatermarkMode::Chapter));
        assert!(offered_watermark_modes(false).contains(&WatermarkMode::Chapter));
    }

    /// The session fallback leaves the persisted choice alone: the tool reads «По главе» as
    /// «Только маска» in a single-image session without rewriting `settings.mode`, and the
    /// chapter library panel stays hidden even when its toggle was on.
    #[test]
    fn a_single_image_session_keeps_the_persisted_chapter_mode() {
        let mut tool = WatermarkRemovalTool::default();
        tool.settings.mode = WatermarkMode::Chapter.wire().to_string();
        tool.library_window.toggle();
        tool.set_single_image_session(true);
        assert_eq!(tool.effective_mode(), WatermarkMode::MaskOnly);
        assert_eq!(tool.settings.mode, WatermarkMode::Chapter.wire());
        assert!(!tool.wants_library_panel());
        tool.set_single_image_session(false);
        assert_eq!(tool.effective_mode(), WatermarkMode::Chapter);
        assert!(tool.wants_library_panel());
    }

    /// The «Библиотека знаков» dock tab follows the library state and nothing else, and it
    /// starts hidden: a dock tab has no close affordance, so the two «Библиотека знаков…»
    /// buttons toggling this ONE flag are the whole story.
    #[test]
    fn the_library_panel_follows_the_library_toggle() {
        let mut tool = WatermarkRemovalTool::default();
        assert!(!tool.wants_library_panel());
        tool.library_window.toggle();
        assert!(tool.wants_library_panel());
        tool.library_window.toggle();
        assert!(!tool.wants_library_panel());
    }

    /// The library panel is a DOCK panel, so its rect reaches the tab's occlusion gate
    /// through `PanelDockOutput::drawn_panels` and must not be reported a second time here —
    /// and it must not block canvas zoom, which would also kill the clean-overlay undo
    /// shortcuts for as long as the panel stays open.
    #[test]
    fn the_library_panel_neither_captures_the_pointer_nor_blocks_zoom() {
        let mut tool = WatermarkRemovalTool::default();
        tool.library_window.toggle();
        assert!(tool.wants_library_panel());
        assert!(!tool.block_canvas_zoom());
        assert!(!tool.captures_canvas_pointer(egui::pos2(10.0, 10.0)));
    }

    #[test]
    fn defaults_are_mask_only_slbr() {
        let settings = WatermarkRemovalSettings::default();
        assert_eq!(settings.mode, "mask_only");
        assert_eq!(settings.model, DEFAULT_WATERMARK_MODEL);
        assert!(settings.show_mask_preview);
        assert_eq!(settings, settings.normalized());
    }

    #[test]
    fn settings_roundtrip_through_json() {
        let settings = WatermarkRemovalSettings {
            mode: WatermarkMode::Clean.wire().to_string(),
            model: "splitnet".to_string(),
            tile: 256,
            overlap: 32,
            threshold: 0.35,
            dilate_px: 9,
            show_mask_preview: false,
            chapter_anchor_tolerance_px: 4,
            chapter_background_blur_px: 8,
            chapter_ring_width_px: 5,
        };
        let raw = serde_json::to_string(&settings).expect("serialize settings");
        let parsed: WatermarkRemovalSettings =
            serde_json::from_str(&raw).expect("deserialize settings");
        assert_eq!(parsed, settings);
    }

    #[test]
    fn partial_json_uses_defaults() {
        let empty: WatermarkRemovalSettings =
            serde_json::from_str("{}").expect("deserialize empty object");
        assert_eq!(empty, WatermarkRemovalSettings::default());

        let partial: WatermarkRemovalSettings =
            serde_json::from_str(r#"{"tile": 256, "mode": "clean"}"#).expect("deserialize partial");
        assert_eq!(partial.tile, 256);
        assert_eq!(partial.mode, "clean");
        assert_eq!(partial.model, WatermarkRemovalSettings::default().model);
        assert_eq!(
            partial.dilate_px,
            WatermarkRemovalSettings::default().dilate_px
        );
    }

    #[test]
    fn normalize_clamps_every_parameter() {
        let wild = WatermarkRemovalSettings {
            mode: "nonsense".to_string(),
            model: "not-a-model".to_string(),
            tile: 9999,
            overlap: 9999,
            threshold: 12.0,
            dilate_px: 999,
            show_mask_preview: true,
            chapter_anchor_tolerance_px: 9999,
            chapter_background_blur_px: 9999,
            chapter_ring_width_px: 9999,
        };
        let normalized = wild.normalized();
        assert_eq!(normalized.mode, WatermarkMode::MaskOnly.wire());
        assert_eq!(normalized.model, DEFAULT_WATERMARK_MODEL);
        assert_eq!(normalized.tile, WATERMARK_TILE_MAX);
        assert_eq!(normalized.tile % WATERMARK_TILE_MULTIPLE, 0);
        assert_eq!(normalized.overlap, WATERMARK_OVERLAP_MAX);
        assert!((normalized.threshold - WATERMARK_THRESHOLD_MAX).abs() < f32::EPSILON);
        assert_eq!(normalized.dilate_px, WATERMARK_DILATE_MAX);
        // The chapter parameters are clamped by the ENGINE's own measured bounds, so a
        // hand-edited settings file cannot widen the anchor band into "anywhere".
        let engine_detection = DetectionParams {
            anchor_tolerance: u32::MAX,
            background_blur_radius: u32::MAX,
            ..DetectionParams::default()
        }
        .normalized();
        assert_eq!(
            normalized.chapter_anchor_tolerance_px,
            engine_detection.anchor_tolerance
        );
        assert_eq!(
            normalized.chapter_background_blur_px,
            engine_detection.background_blur_radius
        );
        assert_eq!(
            normalized.chapter_ring_width_px,
            SampleParams {
                ring_width: u32::MAX,
                ..SampleParams::default()
            }
            .normalized()
            .ring_width
        );

        // With a small tile it is half the tile, not the slider maximum, that caps
        // the overlap — otherwise neighbouring tiles would swallow each other.
        let narrow = WatermarkRemovalSettings {
            tile: WATERMARK_TILE_MIN,
            overlap: WATERMARK_OVERLAP_MAX,
            ..WatermarkRemovalSettings::default()
        };
        assert_eq!(narrow.normalized().overlap, WATERMARK_TILE_MIN / 2);

        let tiny = WatermarkRemovalSettings {
            tile: 10,
            overlap: 4,
            threshold: f32::NAN,
            dilate_px: 0,
            ..WatermarkRemovalSettings::default()
        };
        let normalized = tiny.normalized();
        assert_eq!(normalized.tile, WATERMARK_TILE_MIN);
        assert_eq!(normalized.overlap, 4);
        assert!(
            (normalized.threshold - WatermarkRemovalSettings::default().threshold).abs()
                < f32::EPSILON
        );

        // A tile that is not a multiple of 16 is snapped DOWN, never up: the
        // networks reject any other size.
        let odd = WatermarkRemovalSettings {
            tile: 519,
            ..WatermarkRemovalSettings::default()
        };
        assert_eq!(odd.normalized().tile, 512);
    }

    #[test]
    fn blob_split_accepts_exact_lengths() {
        let blob = b"CLEANMASK".to_vec();
        let header = json!({ "image_len": 5, "mask_len": 4 });
        let (clean, mask) =
            split_watermark_remove_blob(&header, &blob).expect("exact lengths must split");
        assert_eq!(clean, b"CLEAN");
        assert_eq!(mask, b"MASK");
    }

    #[test]
    fn blob_split_rejects_short_padded_and_malformed() {
        let blob = b"CLEANMASK".to_vec();

        // Declared more than the blob carries.
        let short = json!({ "image_len": 5, "mask_len": 5 });
        assert!(split_watermark_remove_blob(&short, &blob).is_err());

        // Declared less than the blob carries (trailing padding).
        let padded = json!({ "image_len": 5, "mask_len": 3 });
        assert!(split_watermark_remove_blob(&padded, &blob).is_err());

        // Missing / non-integer / negative fields.
        assert!(split_watermark_remove_blob(&json!({ "mask_len": 4 }), &blob).is_err());
        assert!(
            split_watermark_remove_blob(&json!({ "image_len": "5", "mask_len": 4 }), &blob).is_err()
        );
        assert!(
            split_watermark_remove_blob(&json!({ "image_len": -5, "mask_len": 4 }), &blob).is_err()
        );

        // A zero-length part is an empty result, not a valid split.
        assert!(
            split_watermark_remove_blob(&json!({ "image_len": 9, "mask_len": 0 }), &blob).is_err()
        );
        assert!(
            split_watermark_remove_blob(&json!({ "image_len": 0, "mask_len": 9 }), &blob).is_err()
        );
    }

    #[test]
    fn request_header_carries_wire_params_per_mode() {
        let settings = WatermarkRemovalSettings {
            mode: WatermarkMode::Clean.wire().to_string(),
            model: "wdnet".to_string(),
            tile: 256,
            overlap: 64,
            threshold: 0.4,
            dilate_px: 6,
            show_mask_preview: true,
            ..WatermarkRemovalSettings::default()
        }
        .normalized();

        let clean = watermark_request_header(WatermarkNetworkMode::Clean, &settings);
        let params = clean.get("params").expect("clean header has params");
        assert_eq!(params.get("model").and_then(Value::as_str), Some("wdnet"));
        assert_eq!(params.get("tile").and_then(Value::as_u64), Some(256));
        assert_eq!(params.get("overlap").and_then(Value::as_u64), Some(64));
        assert_eq!(params.get("dilate_px").and_then(Value::as_u64), Some(6));
        assert!(params.get("downscale_to").is_none());

        let detect = watermark_request_header(WatermarkNetworkMode::MaskOnly, &settings);
        let params = detect.get("params").expect("detect header has params");
        assert_eq!(
            params.get("downscale_to").and_then(Value::as_u64),
            Some(u64::from(WATERMARK_DETECT_DOWNSCALE_TO))
        );
        // The detect pass is not tiled, so the tile geometry must not leak into it.
        assert!(params.get("tile").is_none());
        assert!(params.get("overlap").is_none());
    }

    // -----------------------------------------------------------------------------------
    // Chapter mode
    // -----------------------------------------------------------------------------------

    /// Synthetic mark: a cross at opacity 0.4 inside a border at 0.2, colour (30, 200, 30).
    ///
    /// Every product below is an exact byte at both background levels used here, so the
    /// test measures the pipeline rather than rounding.
    const SYNTHETIC_MARK_SIDE: u32 = 16;
    const SYNTHETIC_MARK_COLOUR: [f32; 3] = [30.0, 200.0, 30.0];

    fn synthetic_alpha(x: u32, y: u32) -> f32 {
        let inside_cross = (6..10).contains(&x) || (6..10).contains(&y);
        let on_border = x == 0 || y == 0 || x + 1 == SYNTHETIC_MARK_SIDE || y + 1 == SYNTHETIC_MARK_SIDE;
        if inside_cross {
            0.4
        } else if on_border {
            0.2
        } else {
            0.0
        }
    }

    /// Stamps the synthetic mark over a uniform background at `(ox, oy)`.
    fn stamp_synthetic_mark(page: &mut RgbaImage, ox: u32, oy: u32) {
        for y in 0..SYNTHETIC_MARK_SIDE {
            for x in 0..SYNTHETIC_MARK_SIDE {
                let alpha = synthetic_alpha(x, y);
                let pixel = page.get_pixel_mut(ox + x, oy + y);
                for channel in 0..3 {
                    let background = f32::from(pixel[channel]);
                    let value = alpha * SYNTHETIC_MARK_COLOUR[channel] + (1.0 - alpha) * background;
                    // Cast justification: every product is exact for the alphas and levels
                    // used here, and the value is clamped into 0..=255 anyway.
                    pixel[channel] = value.clamp(0.0, 255.0).round() as u8;
                }
            }
        }
    }

    /// Writes a 200x600 page holding the mark twice on white and once on black, and
    /// returns its path plus the three occurrence rects.
    fn write_synthetic_page(name: &str) -> (PathBuf, [OverlayRectPx; 3]) {
        let dir = std::env::temp_dir().join(format!("manhwastudio-wm-chapter-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        let mut page = RgbaImage::from_pixel(200, 600, image::Rgba([255, 255, 255, 255]));
        for y in 350..500 {
            for x in 0..200 {
                page.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
            }
        }
        let rects = [
            OverlayRectPx {
                x: 100,
                y: 100,
                w: SYNTHETIC_MARK_SIDE as usize,
                h: SYNTHETIC_MARK_SIDE as usize,
            },
            OverlayRectPx {
                x: 100,
                y: 200,
                w: SYNTHETIC_MARK_SIDE as usize,
                h: SYNTHETIC_MARK_SIDE as usize,
            },
            OverlayRectPx {
                x: 100,
                y: 400,
                w: SYNTHETIC_MARK_SIDE as usize,
                h: SYNTHETIC_MARK_SIDE as usize,
            },
        ];
        for rect in &rects {
            // Cast justification: the rects above are literals far below u32::MAX.
            stamp_synthetic_mark(&mut page, rect.x as u32, rect.y as u32);
        }
        let path = dir.join("page.png");
        page.save(&path).expect("write synthetic page");
        (path, rects)
    }

    /// A catalog holding one mark LOADED from a library entry and one the chapter discovered
    /// on its own, with a kind id that would fool a plain `index_of` lookup.
    fn catalog_with_library_and_discovered(entry_id: &str, discovered_id: &str) -> ChapterCatalog {
        let mut catalog = ChapterCatalog::default();
        for (id, library_entry) in [
            (entry_id, Some(entry_id.to_string())),
            (discovered_id, None),
        ] {
            let mut crop = RgbaImage::from_pixel(16, 16, image::Rgba([255, 255, 255, 255]));
            stamp_synthetic_mark(&mut crop, 0, 0);
            let template = MarkTemplate::from_page(&crop, PixelRect::new(0, 0, 16, 16))
                .expect("template from the synthetic crop");
            let kind = WatermarkKind::new(id.to_string(), template, alpha_blend_operator());
            let mut mark = ChapterMark::new(id.to_string(), crop, 200);
            mark.library_entry = library_entry;
            catalog.push(kind, mark);
        }
        catalog
    }

    /// The card's «Выбрать»/«Убрать» reads chapter MEMBERSHIP, and «Убрать» drops only the
    /// mark that came from that entry: a mark the chapter discovered for itself cost a scan
    /// to collect and must survive a click on a library card that happens to sit next to it.
    #[test]
    fn the_card_toggle_reflects_catalog_membership_and_spares_discovered_marks() {
        let mut tool = WatermarkRemovalTool::default();
        tool.chapter.catalog = catalog_with_library_and_discovered("entry-a", "mark-1");
        assert_eq!(tool.chapter_library_selection(), vec!["entry-a".to_string()]);

        tool.unload_library_entry("mark-1");
        assert_eq!(
            tool.chapter.catalog.len(),
            2,
            "a chapter-discovered mark is not the library card's to remove"
        );

        tool.unload_library_entry("entry-a");
        assert_eq!(tool.chapter.catalog.len(), 1);
        assert!(tool.chapter_library_selection().is_empty());
        assert!(
            tool.chapter.catalog.index_of("mark-1").is_some(),
            "the discovered mark is still the chapter's"
        );
    }

    /// An entry a chapter-DISCOVERED mark was saved to is already in the chapter under a
    /// `mark-{n}` kind id. It is therefore not selectable — a second load would give the
    /// catalog two kinds for one physical mark — and not unselectable either, because the
    /// mark it names cost a scan to measure.
    #[test]
    fn a_mark_saved_to_the_library_is_already_in_the_chapter() {
        let mut tool = WatermarkRemovalTool::default();
        tool.chapter.catalog = catalog_with_library_and_discovered("entry-a", "mark-1");
        // «В библиотеку» on the discovered mark: the kind id stays `mark-1`.
        tool.chapter.catalog.marks[1].library_entry = Some("entry-b".to_string());

        assert_eq!(tool.chapter_library_selection(), vec!["entry-a".to_string()]);
        assert_eq!(tool.chapter_saved_entries(), vec!["entry-b".to_string()]);
        assert_eq!(
            tool.chapter_entry_index("entry-b"),
            Some(1),
            "the entry is already represented, so a load must be refused"
        );
        assert_eq!(tool.chapter_entry_index("entry-a"), Some(0));
        assert_eq!(tool.chapter_entry_index("entry-c"), None);

        // And the mark itself is not the library card's to remove.
        tool.unload_library_entry("entry-b");
        assert_eq!(
            tool.chapter.catalog.len(),
            2,
            "a mark the chapter discovered survives a click on the card it was saved to"
        );
    }

    /// Exactly one canvas reservation is live at a time, so «+ новый» and «+ Выделить новый»
    /// are mutually exclusive without either knowing the other exists — and cancelling clears
    /// it, while choosing which entry the chapter hunts leaves it alone.
    #[test]
    fn exactly_one_canvas_reservation_is_live() {
        let mut arm = next_library_arm(None, &LibraryPanelRequest::Arm("entry-b".to_string()));
        assert_eq!(arm, Some("entry-b".to_string()));

        arm = next_library_arm(
            arm,
            &LibraryPanelRequest::Arm("entry-a".to_string()),
        );
        assert_eq!(
            arm,
            Some("entry-a".to_string()),
            "arming a second purpose replaces the first instead of adding to it"
        );

        arm = next_library_arm(
            arm,
            &LibraryPanelRequest::SelectEntry("entry-b".to_string()),
        );
        assert_eq!(arm, Some("entry-a".to_string()));

        arm = next_library_arm(arm, &LibraryPanelRequest::Disarm);
        assert_eq!(arm, None);
        assert_eq!(next_library_arm(None, &LibraryPanelRequest::Disarm), None);
    }

    /// A reservation and the region base's DIVERSION are one fact: while a crop is reserved,
    /// the next completed drag belongs to the library and must not run the region editor.
    #[test]
    fn a_reservation_diverts_the_next_selection_and_letting_it_go_undoes_that() {
        let mut tool = WatermarkRemovalTool {
            library_arm: Some("entry-b".to_string()),
            ..Default::default()
        };
        tool.sync_selection_diversion();
        tool.region_base.park_diverted_selection(
            3,
            OverlayRectPx {
                x: 8,
                y: 9,
                w: 40,
                h: 50,
            },
        );
        let (page_idx, rect) = tool
            .region_base
            .take_diverted_selection()
            .expect("an armed drag must be parked for the tool, not loaded into the editor");
        assert_eq!(page_idx, 3);
        assert_eq!((rect.x, rect.y, rect.w, rect.h), (8, 9, 40, 50));
        assert!(
            !tool.region_base.has_open_editor(),
            "an armed drag must not open the region editor"
        );

        tool.library_arm = None;
        tool.sync_selection_diversion();
        tool.region_base.set_selection_diverted(true);
        tool.region_base.park_diverted_selection(
            3,
            OverlayRectPx {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
        );
        tool.sync_selection_diversion();
        assert!(
            tool.region_base.take_diverted_selection().is_none(),
            "with no reservation live the diversion is off and any parked rect is dropped"
        );
    }

    /// THE FIRST HALF OF THE BUG: while a reservation is armed the tool must accept a PLAIN
    /// ЛКМ drag, and the hint the user is reading must say so.
    ///
    /// The Shift-only gate made an armed drag a complete no-op — no rubber band, no pan (the
    /// canvas pans only with Space held), no message — and the one string that named Shift was
    /// painted in a different dock tab from the library panel the user was reading.
    #[test]
    fn an_armed_tool_accepts_a_plain_drag_and_says_which_gesture_it_wants() {
        let _guard = locale_guard();
        let plain = super::super::base::StrokePoint {
            page_idx: 0,
            scene_pos: egui::pos2(12.0, 12.0),
            modifiers: super::super::base::StrokeModifiers::default(),
        };
        let mut tool = WatermarkRemovalTool {
            library_arm: Some("entry-b".to_string()),
            ..Default::default()
        };
        tool.sync_selection_diversion();
        assert!(
            tool.wants_primary_stroke(plain),
            "the button the user just pressed IS the statement of intent"
        );
        assert_eq!(
            tool.region_base.selection_hint(),
            t!("cleaning.region.selection_hint_armed"),
            "the tool's own hint must name the gesture accepted in this state"
        );
        let gesture = t!("cleaning.tools.watermark.chapter.library_arm_gesture_hint");
        assert_ne!(
            gesture, "cleaning.tools.watermark.chapter.library_arm_gesture_hint",
            "the panel's gesture line must exist in the catalog, not fall back to its own key"
        );

        tool.library_arm = None;
        tool.sync_selection_diversion();
        assert!(
            !tool.wants_primary_stroke(plain),
            "with nothing armed the Shift-only gate stands, as it does for every other tool"
        );
        assert_eq!(
            tool.region_base.selection_hint(),
            t!("cleaning.region.selection_hint")
        );
    }

    /// A parked rect that reaches the tool with NO reservation must be REPORTED, not dropped.
    ///
    /// Defensive: every path that clears `library_arm` also calls `sync_selection_diversion`,
    /// which drops the parked rect with it, so this state means one of them stopped keeping
    /// that promise. Dropping the user's gesture without a word looks exactly like a drag that
    /// did nothing at all — which is the whole class of defect this change closes.
    #[test]
    fn a_parked_rect_with_no_reservation_is_reported_instead_of_dropped() {
        let _guard = locale_guard();
        let mut tool = WatermarkRemovalTool {
            library_arm: Some("entry-a".to_string()),
            ..Default::default()
        };
        tool.sync_selection_diversion();
        let rect = OverlayRectPx {
            x: 5,
            y: 6,
            w: 32,
            h: 24,
        };
        tool.region_base.park_diverted_selection(2, rect);
        // The reservation dies without the diversion being re-synced — the state this branch
        // exists to catch.
        tool.library_arm = None;

        assert!(
            tool.take_reservation_for_diverted(2, rect).is_none(),
            "there is no reservation to take"
        );
        assert!(
            tool.library_window.status().is_some(),
            "an orphan rect must reach the user, not the void"
        );
    }

    /// A reservation whose screen stopped being drawn is dropped — and SAYS SO.
    ///
    /// The red «Отменить выделение» is the only way to call a reservation off, so one drawn on
    /// no screen is one the user cannot cancel. Dropping it silently left the next drag opening
    /// the ordinary region editor with nothing having explained why.
    #[test]
    fn a_reservation_whose_screen_is_gone_is_dropped_with_a_reason() {
        let _guard = locale_guard();
        let mut tool = WatermarkRemovalTool {
            library_arm: Some("entry-a".to_string()),
            ..Default::default()
        };
        tool.sync_selection_diversion();
        // The panel is closed, so `arm_survives` is false: no screen draws the cancel button.
        assert!(!tool.library_window.is_open());

        tool.drop_reservation_without_a_screen();

        assert!(tool.library_arm.is_none());
        assert!(
            tool.library_window.status().is_some(),
            "a reservation that dies for a reason the user did not choose must be reported"
        );
    }

    /// The ORDERING TRAP: `CleaningTabState::activate_tool` calls `finish_stroke()` before
    /// `deactivate()`, so switching tools in the middle of an ARMED drag completes a diverted
    /// selection one statement before the reservation dies. Neither the rect nor a capture may
    /// survive that — a rect kept here would be cut out the next time the tool is armed.
    #[test]
    fn a_tool_switch_mid_armed_drag_strands_neither_the_rect_nor_a_capture() {
        let mut tool = WatermarkRemovalTool {
            library_arm: Some("entry-a".to_string()),
            ..Default::default()
        };
        let mut canvas = CanvasView::default();
        tool.sync_selection_diversion();
        // `finish_stroke` -> `stroke_end` -> the base parks the rect, all BEFORE `deactivate`.
        tool.region_base.park_diverted_selection(
            1,
            OverlayRectPx {
                x: 2,
                y: 2,
                w: 16,
                h: 16,
            },
        );

        tool.deactivate(&mut canvas);

        assert!(tool.library_arm.is_none());
        assert!(
            tool.region_base.take_diverted_selection().is_none(),
            "the rect must die with the reservation that armed it"
        );
        assert!(
            tool.library_capture_rx.is_none() && tool.library_capture_arm.is_none(),
            "no capture may be left in flight by a tool switch"
        );
    }

    /// A selection in the middle of the page gets its full ring on all four sides, and the
    /// margins it reports say so — that is the case every existing entry was cut under, and it
    /// must not move because the edge case was added.
    #[test]
    fn a_capture_away_from_the_edge_keeps_its_full_ring() {
        assert_eq!(
            grow_page_rect(PixelRect::new(10, 10, 20, 20), 4, 100, 100),
            Some(GrownCrop {
                outer: PixelRect::new(6, 6, 28, 28),
                margins: CropMargins::uniform(4),
            })
        );
    }

    /// A selection with less than a full ring available takes what the page can give instead
    /// of being refused. This is the user's real case: a mark stamped flush against the image
    /// border has no background on that side and never will, so a symmetric demand made such a
    /// mark permanently unsamplable. The per-side margins come back with the crop so the intake
    /// derives the footprint that was cut.
    #[test]
    fn a_capture_at_the_page_edge_takes_the_ring_the_page_can_give() {
        // Tight on the left: 1 px available of the 4 asked for.
        assert_eq!(
            grow_page_rect(PixelRect::new(1, 10, 20, 20), 4, 100, 100),
            Some(GrownCrop {
                outer: PixelRect::new(0, 6, 25, 28),
                margins: CropMargins {
                    left: 1,
                    top: 4,
                    right: 4,
                    bottom: 4,
                },
            })
        );
        // The user's own geometry: footprint 319 wide at x=480 on an 800-px page leaves one
        // pixel on the right. It used to be refused outright.
        assert_eq!(
            grow_page_rect(PixelRect::new(480, 200, 319, 236), 4, 800, 1200),
            Some(GrownCrop {
                outer: PixelRect::new(476, 196, 324, 244),
                margins: CropMargins {
                    left: 4,
                    top: 4,
                    right: 1,
                    bottom: 4,
                },
            })
        );
        // Flush against the top: nothing above it at all, and that is still a crop.
        assert_eq!(
            grow_page_rect(PixelRect::new(10, 0, 20, 20), 4, 100, 100),
            Some(GrownCrop {
                outer: PixelRect::new(6, 0, 28, 24),
                margins: CropMargins {
                    left: 4,
                    top: 0,
                    right: 4,
                    bottom: 4,
                },
            })
        );
    }

    /// A rect that is not inside the page, or is empty, is still refused: that is a bad
    /// selection rather than a tight one, and no amount of clipping makes it a crop.
    #[test]
    fn a_capture_outside_the_page_is_still_refused() {
        assert_eq!(grow_page_rect(PixelRect::new(90, 10, 20, 20), 4, 100, 100), None);
        assert_eq!(grow_page_rect(PixelRect::new(10, 90, 20, 20), 4, 100, 100), None);
        assert_eq!(grow_page_rect(PixelRect::new(10, 10, 0, 20), 4, 100, 100), None);
        assert_eq!(grow_page_rect(PixelRect::new(10, 10, 20, 0), 4, 100, 100), None);
    }

    /// A crop whose ring the engine could not MEASURE AT ALL is refused at capture time.
    ///
    /// Stating a level cannot license it — `run_reference_intake` returns `reference_ring_error`
    /// whatever is asserted — so offering it the colour control would walk the user through
    /// stating a background, committing, and losing the crop. The refusal also happens before
    /// the scratch PNG is written, so nothing is left behind to leak.
    #[test]
    fn a_crop_with_no_measurable_ring_is_refused_instead_of_offered_a_colour() {
        let _guard = locale_guard();
        let (path, rects) = write_synthetic_page("capture-ring");
        let scratch_before = count_scratch_crops();
        let request = LibraryCaptureRequest {
            page: ChapterPageTask {
                page_idx: 0,
                path: path.clone(),
                overlay_size: Some([200, 600]),
            },
            rect: rects[0],
            // A ring requirement no crop of this page can meet: the measurement then reports
            // `RingTooSmall`, which is the verdict under test.
            sample_params: SampleParams {
                min_ring_pixels: usize::MAX,
                ..SampleParams::default()
            },
            max_side: CHAPTER_MAX_TEMPLATE_SIDE,
        };
        let error = run_library_capture(request).expect_err("an unmeasurable ring is refused");
        assert!(
            error.message.contains(&usize::MAX.to_string()),
            "the message must name what the ring needed: {}",
            error.message
        );
        assert_eq!(
            count_scratch_crops(),
            scratch_before,
            "a refused capture writes no scratch file"
        );

        // The same selection with the ordinary tunables still measures, so the refusal is
        // about the ring and not about the selection.
        let ok = run_library_capture(LibraryCaptureRequest {
            page: ChapterPageTask {
                page_idx: 0,
                path: path.clone(),
                overlay_size: Some([200, 600]),
            },
            rect: rects[0],
            sample_params: SampleParams::default(),
            max_side: CHAPTER_MAX_TEMPLATE_SIDE,
        })
        .expect("the synthetic mark sits on flat white");
        assert!(ok.measured.is_some());
        remove_scratch_crop(&ok.path);
        let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
    }

    /// THE USER'S OWN ENTRY, end to end through the capture lane.
    ///
    /// `wm-1786790637-268d778f`: footprint 319x236, anchor x=480, page width 800. The mark's
    /// right edge therefore lands on 799 of an 800-px page and one pixel of background is left
    /// where the ring wants four. Every attempt to add a sample to it died here — the capture
    /// refused before a crop existed, so the panel had no row to put the reason on and the lane
    /// wrote nothing to the log.
    ///
    /// It must now produce a crop whose level is measured, whose margins name the truncated
    /// side, and which the intake then accepts.
    #[test]
    fn the_users_edge_hugging_entry_can_finally_take_a_sample() {
        let _guard = locale_guard();
        let dir = std::env::temp_dir().join("manhwastudio-wm-edge-entry-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        // An 800x1200 page whose background is flat and whose mark sits flush against the
        // right border, exactly as `entry.json` records it.
        let footprint = PixelRect::new(480, 200, 319, 236);
        let mut page = RgbaImage::from_pixel(800, 1200, image::Rgba([240, 240, 240, 255]));
        for y in footprint.y..footprint.y + footprint.height {
            for x in footprint.x..footprint.x + footprint.width {
                // Structured, so the ring could never be mistaken for the mark.
                // Cast justification: the product is taken modulo 200, far inside u8.
                let shade = ((x * 7 + y * 13) % 200) as u8;
                page.put_pixel(x, y, image::Rgba([shade, shade, shade, 255]));
            }
        }
        let path = dir.join("page.png");
        page.save(&path).expect("write the page");

        let captured = run_library_capture(LibraryCaptureRequest {
            page: ChapterPageTask {
                page_idx: 0,
                path: path.clone(),
                overlay_size: Some([800, 1200]),
            },
            rect: OverlayRectPx {
                x: footprint.x as usize,
                y: footprint.y as usize,
                w: footprint.width as usize,
                h: footprint.height as usize,
            },
            sample_params: SampleParams::default(),
            max_side: CHAPTER_MAX_TEMPLATE_SIDE,
        })
        .expect("a mark against the page border must be capturable");

        let level = captured.measured.expect("the surviving ring measures the background");
        assert!(
            (level[0] - 240.0).abs() < 0.5,
            "the measured level is the page's own background: {level:?}"
        );
        assert_eq!(
            captured.margins,
            CropMargins {
                left: 4,
                top: 4,
                right: 1,
                bottom: 4,
            },
            "and the crop states the background it actually carries on each side"
        );
        // The crop the intake will read is the footprint plus exactly those margins.
        let crop = image::open(&captured.path).expect("the scratch crop was written").to_rgba8();
        assert_eq!(crop.dimensions(), (319 + 4 + 1, 236 + 4 + 4));

        remove_scratch_crop(&captured.path);
        let _ = fs::remove_dir_all(&dir);
    }

    /// THE ACCEPTANCE TEST FOR THE CANVAS LANE: the user's own two marks build through
    /// `run_library_capture` -> `run_reference_intake`, in either order, on real pixels.
    ///
    /// The file-picker lane already built them; this one did not, and it is the one the user
    /// works in. Its input differs in exactly one way — it STATES its `CropMargins` — and that
    /// statement used to veto the joint footprint rule outright, so the entry fell back on a
    /// footprint shaped like whatever rectangle had been dragged. Two hand-cut crops of one mark
    /// are almost never the same rectangle, so the second was refused as too small.
    ///
    /// `test/` is a working directory, not a published fixture path: the test is a no-op when the
    /// files are absent and must never fail for a checkout that does not carry them.
    #[test]
    fn the_users_real_crops_build_both_marks_through_the_canvas_lane() {
        let _guard = locale_guard();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test/водяные метки");
        for (names, level) in [
            (["1_w.png", "1_b.png"], [255u8, 0u8]),
            (["2_w.png", "2_b.png"], [255, 0]),
        ] {
            let paths = names.map(|name| fixtures.join(name));
            if !paths.iter().all(|path| path.is_file()) {
                continue;
            }
            let dir = std::env::temp_dir().join(format!("manhwastudio-wm-canvas-{}", names[0]));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp dir");

            // Each crop pasted into a page of its OWN background, so the drag has real page
            // around it exactly as the user's does. The selection is the crop's own rectangle:
            // the capture lane then grows it by the ring margin and states what it took.
            let mut captured = Vec::new();
            for (index, path) in paths.iter().enumerate() {
                let crop = image::open(path)
                    .unwrap_or_else(|err| panic!("{} decodes: {err}", path.display()))
                    .to_rgba8();
                let (cw, ch) = crop.dimensions();
                let shade = level[index];
                let mut page = RgbaImage::from_pixel(cw + 200, ch + 200, image::Rgba([shade, shade, shade, 255]));
                for y in 0..ch {
                    for x in 0..cw {
                        page.put_pixel(100 + x, 100 + y, *crop.get_pixel(x, y));
                    }
                }
                let page_path = dir.join(format!("page-{index}.png"));
                page.save(&page_path).expect("write the page");
                let shot = run_library_capture(LibraryCaptureRequest {
                    page: ChapterPageTask {
                        page_idx: 0,
                        path: page_path,
                        overlay_size: Some([(cw + 200) as usize, (ch + 200) as usize]),
                    },
                    rect: OverlayRectPx {
                        x: 100,
                        y: 100,
                        w: cw as usize,
                        h: ch as usize,
                    },
                    sample_params: SampleParams::default(),
                    max_side: CHAPTER_MAX_TEMPLATE_SIDE,
                })
                .unwrap_or_else(|err| panic!("{} must be capturable: {}", names[index], err.message));
                assert_eq!(
                    shot.margins,
                    CropMargins::uniform(reference_crop_margin_px(&SampleParams::default())),
                    "the selection sits well inside the page, so nothing was clipped"
                );
                captured.push(shot);
            }

            // Both orders, because the whole defect was that the first crop decided the geometry.
            let mut ran = 0usize;
            for order in [[0usize, 1usize], [1, 0]] {
                let files: Vec<PathBuf> = order.iter().map(|&i| captured[i].path.clone()).collect();
                let margins: Vec<Option<CropMargins>> =
                    order.iter().map(|&i| Some(captured[i].margins)).collect();
                let label = format!("{:?} via the canvas", order.map(|i| names[i]));
                let outcome = run_reference_intake(ReferenceIntakeRequest {
                    files,
                    manual_levels: Vec::new(),
                    crop_margins: margins,
                    sample_params: SampleParams::default(),
                    base: None,
                    name: "Знак".to_string(),
                    max_side: CHAPTER_MAX_TEMPLATE_SIDE,
                })
                .unwrap_or_else(|err| panic!("{label} must build: {}", err.message));
                assert!(
                    outcome.conditioning.is_separable(),
                    "{label}: white and black separate the model: {:?}",
                    outcome.conditioning
                );
                assert_eq!(outcome.request.samples.len(), 2);
                // The footprint is the MARK's, so it is strictly smaller than the SMALLEST of the
                // two selections in both axes — which is what no rule shaped like "cover the first
                // crop" can ever produce for a pair where neither crop covers the other.
                let smallest = paths
                    .iter()
                    .map(|path| {
                        image::image_dimensions(path).unwrap_or_else(|err| panic!("{} reads: {err}", path.display()))
                    })
                    .fold((u32::MAX, u32::MAX), |acc, size| (acc.0.min(size.0), acc.1.min(size.1)));
                assert!(
                    outcome.request.width < smallest.0 && outcome.request.height < smallest.1,
                    "{label}: the footprint {}x{} must be the mark's, not a drag's (smallest crop {smallest:?})",
                    outcome.request.width,
                    outcome.request.height
                );
                ran += 1;
            }

            assert_eq!(ran, 2, "both orders must have been exercised, not skipped");
            for shot in &captured {
                remove_scratch_crop(&shot.path);
            }
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// A chapter mark saved to the library with NO calibration crops is a deliberate write, not
    /// an omission: the entry keeps its template and its footprint, carries no model, and is
    /// exactly the state the user then adds samples to from the panel.
    ///
    /// The user's own entry was created this way, silently. It stays possible — refusing it
    /// would throw away a discovered mark's template — but it is now stated, and logged.
    #[test]
    fn a_chapter_save_with_no_crops_writes_a_deliberate_empty_sample_list() {
        let _guard = locale_guard();
        // A mark discovered by correlation alone: it has its template crop and nothing else,
        // which is precisely what «В библиотеку» hands over in this case.
        let mut template_crop =
            RgbaImage::from_pixel(SYNTHETIC_MARK_SIDE, SYNTHETIC_MARK_SIDE, image::Rgba([255, 255, 255, 255]));
        stamp_synthetic_mark(&mut template_crop, 0, 0);
        let rect = PixelRect::new(0, 0, SYNTHETIC_MARK_SIDE, SYNTHETIC_MARK_SIDE);
        let kind = WatermarkKind::new(
            "mark-1",
            MarkTemplate::from_page(&template_crop, rect).expect("template"),
            alpha_blend_operator(),
        );
        let mark = ChapterMark::new("Знак".to_string(), template_crop, 200);
        assert!(
            mark.crops.is_empty(),
            "this fixture is the no-calibration case under test"
        );
        let request = build_library_request(&mark, &kind, None);
        assert!(
            request.samples.is_empty(),
            "no crop is invented to fill the list"
        );
        assert!(
            request.template.is_some(),
            "but the discovered template IS kept: it is the mark's identity"
        );
        assert!(
            request.planes.is_none(),
            "and no model is claimed from measurements that were never made"
        );
        assert_eq!(
            request.calibration.verdict, "not_enough_samples",
            "the verdict says what the entry is: {:?}",
            request.calibration.verdict
        );
    }

    /// Scratch crops of a run that was killed are swept; the ones this process may still be
    /// holding never are, and neither is anything that is not ours.
    #[test]
    fn the_scratch_sweep_takes_only_what_no_run_can_still_be_holding() {
        let pid = 4242;
        let day = Duration::from_secs(24 * 60 * 60);
        assert!(scratch_crop_is_stale(
            "manhwastudio-wm-capture-99-1.png",
            day,
            pid
        ));
        assert!(
            !scratch_crop_is_stale("manhwastudio-wm-capture-99-1.png", Duration::ZERO, pid),
            "another instance may still be holding a fresh crop"
        );
        assert!(
            !scratch_crop_is_stale("manhwastudio-wm-capture-4242-1.png", day * 30, pid),
            "this process's own crops are still reachable from the panel"
        );
        assert!(
            !scratch_crop_is_stale("holiday-photo.png", day * 30, pid),
            "the sweep touches nothing but its own files"
        );
        assert!(
            !scratch_crop_is_stale("manhwastudio-wm-capture-99-1.txt", day * 30, pid),
            "only the crops themselves are swept"
        );
    }

    /// Scratch crops currently in the system temp directory, whoever wrote them.
    fn count_scratch_crops() -> usize {
        let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
            return 0;
        };
        entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(SCRATCH_CROP_PREFIX))
            })
            .count()
    }

    fn sample_request(
        catalog: ChapterCatalog,
        path: &Path,
        rect: OverlayRectPx,
        target: Option<usize>,
    ) -> ChapterSampleRequest {
        ChapterSampleRequest {
            catalog,
            target,
            page: ChapterPageTask {
                page_idx: 0,
                path: path.to_path_buf(),
                overlay_size: Some([200, 600]),
            },
            rect,
            settings: WatermarkRemovalSettings::default().normalized(),
            next_number: 1,
        }
    }

    /// The catalog resolves identity through the engine's signature: a second occurrence of
    /// the SAME mark joins the existing entry instead of becoming a second one.
    #[test]
    fn catalog_matches_a_second_sample_of_the_same_mark() {
        let (path, rects) = write_synthetic_page("match");
        let first = run_chapter_sample(sample_request(
            ChapterCatalog::default(),
            &path,
            rects[0],
            None,
        ))
        .expect("first sample");
        assert_eq!(first.catalog.len(), 1);
        assert_eq!(first.catalog.kinds[0].samples().len(), 1);
        assert_eq!(first.catalog.marks[0].crops.len(), 1);
        // One flat white level: the imprint is exact, the alpha scale is assumed.
        assert!(matches!(
            first.catalog.kinds[0].conditioning(),
            ModelConditioning::DepositExact { .. }
        ));

        let second = run_chapter_sample(sample_request(first.catalog, &path, rects[1], None))
            .expect("second sample");
        assert_eq!(
            second.catalog.len(),
            1,
            "an identical mark must not become a second catalog entry"
        );
        assert_eq!(second.catalog.kinds[0].samples().len(), 2);
        assert_eq!(second.catalog.marks[0].crops.len(), 2);

        // The dark occurrence is added to the SAME mark explicitly, which is what the
        // graded verdict asks for — and it makes the fit separable.
        let third = run_chapter_sample(sample_request(second.catalog, &path, rects[2], Some(0)))
            .expect("third sample");
        assert_eq!(third.catalog.len(), 1);
        assert_eq!(third.catalog.kinds[0].samples().len(), 3);
        assert!(matches!(
            third.catalog.kinds[0].conditioning(),
            ModelConditioning::Separable { .. }
        ));

        // With both levels measured the model is the truth: removal recovers the page.
        let model = third.catalog.kinds[0].model().expect("fitted model");
        let page = decode_chapter_page(&path).expect("decode page");
        for (rect, expected) in [(rects[0], 255u8), (rects[2], 0u8)] {
            let patch = crate::watermark_chapter::remove_occurrence(
                &page,
                PixelRect::new(rect.x as u32, rect.y as u32, SYNTHETIC_MARK_SIDE, SYNTHETIC_MARK_SIDE),
                model,
                crate::watermark_chapter::SubpixelShift::NONE,
            )
            .expect("removal");
            for chunk in patch.pixels.chunks_exact(4) {
                for &value in &chunk[..3] {
                    assert!(
                        value.abs_diff(expected) <= 1,
                        "recovered {value} instead of {expected}"
                    );
                }
            }
        }

        let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
    }

    /// A selection whose ring is not uniform is refused as a CALIBRATION target and kept as
    /// a detection template — the plan's explicit requirement.
    #[test]
    fn a_structured_ring_is_refused_as_calibration() {
        let (path, _) = write_synthetic_page("ring");
        // This rect straddles the white/black boundary, so its ring is not uniform.
        let straddling = OverlayRectPx {
            x: 100,
            y: 340,
            w: SYNTHETIC_MARK_SIDE as usize,
            h: SYNTHETIC_MARK_SIDE as usize,
        };
        let outcome = run_chapter_sample(sample_request(
            ChapterCatalog::default(),
            &path,
            straddling,
            None,
        ))
        .expect("template-only sample");
        assert_eq!(outcome.catalog.len(), 1);
        assert_eq!(
            outcome.catalog.kinds[0].samples().len(),
            0,
            "a structured ring must not reach the estimator"
        );
        assert_eq!(outcome.catalog.marks[0].template_only, 1);
        let verdict = outcome.catalog.marks[0]
            .last_verdict
            .as_ref()
            .expect("verdict");
        assert!(!verdict.is_calibration());
        assert!(verdict.usable_as_template());
        let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
    }

    /// A selection larger than the chapter template limit is refused with the limit named.
    #[test]
    fn an_oversized_selection_is_refused() {
        let _guard = locale_guard();
        let (path, _) = write_synthetic_page("oversized");
        let huge = OverlayRectPx {
            x: 0,
            y: 0,
            w: 200,
            h: 600,
        };
        let error = run_chapter_sample(sample_request(ChapterCatalog::default(), &path, huge, None))
            .expect_err("an oversized selection must be refused");
        assert!(
            error.contains(&CHAPTER_MAX_TEMPLATE_SIDE.to_string()),
            "the message must name the limit: {error}"
        );
        let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
    }

    /// The graded verdict renders as four lines: what was measured, the levels, what the
    /// alpha SCALE is worth, and the concrete sample that would collapse it.
    #[test]
    fn graded_verdict_renders_levels_and_the_fix() {
        let _guard = locale_guard();
        let graded = ModelConditioning::DepositExact {
            levels: vec![255.0],
            spread: 0.0,
            samples: 3,
            alpha: AlphaUncertainty::from_percent(AlphaSource::Assumed, 30.0),
        };
        let lines = describe_conditioning(&graded, false);
        assert_eq!(lines.len(), 4);
        // The imprint is what is measured exactly — never «c».
        assert_eq!(
            lines[0],
            t!("cleaning.tools.watermark.chapter.verdict_deposit_exact")
        );
        assert!(lines[1].contains("255"), "the measured level must be shown: {}", lines[1]);
        assert!(lines[2].contains("30"), "the alpha scale bound must be shown: {}", lines[2]);
        // Samples on white: the fix is a DARKER one, and it is named.
        assert!(
            lines[3].contains("191"),
            "the suggested background must be named: {}",
            lines[3]
        );

        // A refusal produces no alpha line, but still names the fix.
        let refused = ModelConditioning::NotEnoughSamples { have: 0, need: 2 };
        let lines = describe_conditioning(&refused, false);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[1],
            t!("cleaning.tools.watermark.chapter.levels_none")
        );
    }

    /// The quantization report must never read as a quality score, and must carry the two
    /// numbers that are genuinely informative.
    #[test]
    fn residual_is_reported_as_quantization_and_clipping() {
        let _guard = locale_guard();
        let mut residual = RemovalResidual::default();
        residual.mark_pixels = 200;
        residual.exact_pixels = 150;
        residual.clipped_pixels = 10;
        residual.max_uncertainty_lsb = 0.61;
        let line = describe_residual(&residual);
        assert!(line.contains("75.0"), "exact share missing: {line}");
        assert!(line.contains("5.0"), "clipped share missing: {line}");
        assert!(line.contains("0.61"), "uncertainty missing: {line}");
    }

    /// Installs the embedded English catalog for a test that asserts on rendered text.
    ///
    /// Without an installed catalog `t!`/`tf!` return the key verbatim, so a message test
    /// would pass on a template that never interpolates. The returned guard is held for the
    /// test's lifetime because the active locale is process-global.
    fn locale_guard() -> std::sync::MutexGuard<'static, ()> {
        let guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");
        guard
    }

    /// `OverlayRectPx` carries no `PartialEq`, so its fields are compared explicitly.
    fn assert_overlay_rect(actual: OverlayRectPx, expected: [usize; 4]) {
        assert_eq!(
            [actual.x, actual.y, actual.w, actual.h],
            expected,
            "unexpected overlay rect"
        );
    }

    /// Patch geometry: overlay and page grids agree when they are the same size, scale
    /// proportionally when they are not, and degenerate rects are refused instead of being
    /// clamped into a one-pixel write.
    #[test]
    fn patch_geometry_maps_between_overlay_and_page() {
        let identity = overlay_rect_to_page(
            OverlayRectPx {
                x: 10,
                y: 20,
                w: 30,
                h: 40,
            },
            [200, 600],
            200,
            600,
        )
        .expect("identity mapping");
        assert_eq!(identity, PixelRect::new(10, 20, 30, 40));
        assert_overlay_rect(
            page_rect_to_overlay(identity, [200, 600], [200, 600]).expect("back"),
            [10, 20, 30, 40],
        );

        // Half-size overlay: the rect doubles on the page and halves on the way back.
        let scaled = overlay_rect_to_page(
            OverlayRectPx {
                x: 10,
                y: 20,
                w: 30,
                h: 40,
            },
            [100, 300],
            200,
            600,
        )
        .expect("scaled mapping");
        assert_eq!(scaled, PixelRect::new(20, 40, 60, 80));
        assert_overlay_rect(
            page_rect_to_overlay(scaled, [200, 600], [100, 300]).expect("back"),
            [10, 20, 30, 40],
        );

        // Degenerate and out-of-page inputs yield nothing at all.
        assert!(
            overlay_rect_to_page(
                OverlayRectPx {
                    x: 10,
                    y: 20,
                    w: 0,
                    h: 40
                },
                [200, 600],
                200,
                600
            )
            .is_none()
        );
        assert!(
            overlay_rect_to_page(
                OverlayRectPx {
                    x: 10,
                    y: 20,
                    w: 30,
                    h: 40
                },
                [0, 600],
                200,
                600
            )
            .is_none()
        );
        assert!(page_rect_to_overlay(PixelRect::new(0, 0, 1, 1), [200, 600], [0, 0]).is_none());
    }

    /// The library request carries the calibration verdict, the search metadata and the
    /// crops — and the crops alone reconstruct the same model, which is the contract that
    /// makes a stored entry usable without any calibration.
    #[test]
    fn library_request_reconstructs_the_same_model() {
        let (path, rects) = write_synthetic_page("library");
        let mut catalog = ChapterCatalog::default();
        for (index, rect) in rects.iter().enumerate() {
            let target = (index > 0).then_some(0);
            catalog = run_chapter_sample(sample_request(catalog, &path, *rect, target))
                .expect("sample")
                .catalog;
        }
        let mark = &catalog.marks[0];
        let kind = &catalog.kinds[0];
        let request = build_library_request(
            mark,
            kind,
            Some(StoredSourceRef {
                source_key: "series".to_string(),
                page_width: 200,
                anchor_key: kind.template().anchor_key(),
                variant_id: kind.id().to_string(),
                chapter: Some("ch01".to_string()),
            }),
        );
        assert_eq!(request.calibration.verdict, "separable");
        assert_eq!(
            request.calibration.fit_method.as_deref(),
            Some("closed_form_flat")
        );
        assert_eq!(request.calibration.levels.len(), 2);
        assert_eq!(request.samples.len(), 3);
        let request_template = request.template.clone().expect("a chapter mark always has one");
        assert_eq!(
            request_template.dimensions(),
            (SYNTHETIC_MARK_SIDE, SYNTHETIC_MARK_SIDE)
        );
        let planes = request.planes.as_ref().expect("a fitted model has planes");

        // Rebuild the kind the way `run_chapter_library_load` does — from the crops only.
        let rect = PixelRect::new(0, 0, request.width, request.height);
        let template = MarkTemplate::from_page(&request_template, rect).expect("template");
        let mut restored = WatermarkKind::new("restored", template, alpha_blend_operator());
        for sample in &request.samples {
            let sample_rect = PixelRect::new(0, 0, sample.image.width(), sample.image.height());
            restored
                .add_sample(
                    CalibrationSample::from_page(
                        &sample.image,
                        0,
                        sample_rect,
                        engine_background(sample.background),
                    )
                    .expect("calibration sample"),
                )
                .expect("add sample");
        }
        refit_chapter_kind(&mut restored).expect("refit");
        let restored_model = restored.model().expect("restored model");
        for (a, b) in restored_model.c().iter().zip(planes.c.iter()) {
            assert!((a - b).abs() <= 0.01, "c drifted: {a} vs {b}");
        }
        for (a, b) in restored_model.s().iter().zip(planes.s.iter()) {
            assert!((a - b).abs() <= 0.0001, "s drifted: {a} vs {b}");
        }
        let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
    }

    /// Turns a save request into the entry a loader would hand back, without touching the
    /// installation's own library.
    fn loaded_entry(id: &str, request: &SaveEntryRequest) -> LoadedEntry {
        LoadedEntry {
            meta: super::super::watermark_library::EntryFile {
                format: super::super::watermark_library::WATERMARK_LIBRARY_FORMAT,
                id: id.to_string(),
                name: request.name.clone(),
                created_unix: 1,
                updated_unix: 2,
                operator: request.operator.clone(),
                width: request.width,
                height: request.height,
                anchors: request.anchors.clone(),
                anchor_key: request.anchor_key.clone(),
                alpha_assumption: request.alpha_assumption,
                signature: request.signature,
                calibration: request.calibration.clone(),
                sources: Vec::new(),
                samples: Vec::new(),
                template: Some("template.png".to_string()),
                planes: None,
            },
            template: request.template.clone(),
            samples: request.samples.clone(),
        }
    }

    /// The listing row of an entry built from `request`, optionally with its deposit scaled
    /// so it describes a DIFFERENT mark that happens to share the artwork.
    fn library_row(id: &str, request: &SaveEntryRequest, deposit_scale: f32) -> EntrySummary {
        let signature = request.signature.map(|signature| StoredSignature {
            mean_deposit: signature.mean_deposit * deposit_scale,
            ..signature
        });
        EntrySummary {
            has_template: true,
            partial_rings: 0,
            id: id.to_string(),
            name: id.to_string(),
            width: request.width,
            height: request.height,
            anchor_key: request.anchor_key.clone(),
            verdict: request.calibration.verdict.clone(),
            levels: request.calibration.levels.clone(),
            spread: request.calibration.spread,
            samples: request.samples.len(),
            alpha: request.calibration.alpha.clone(),
            fit_method: request.calibration.fit_method.clone(),
            signature,
            sources: Vec::new(),
            updated_unix: 2,
            format: 1,
            clamped_pixels: request.calibration.clamped_pixels,
            manual_backgrounds: Vec::new(),
        }
    }

    /// Auto-match end to end: a chapter that measured only ONE background level adopts the
    /// library entry that carries the same mark and becomes exact, while an entry whose
    /// artwork is identical but whose deposit is not never even enters the candidate list.
    /// The override then hands the chapter its own measurements back.
    #[test]
    fn auto_match_adopts_the_entry_that_carries_this_mark() {
        let (path, rects) = write_synthetic_page("automatch");

        // The library entry: all three occurrences, so the fit separated `c` from `s`.
        let mut exact = ChapterCatalog::default();
        for (index, rect) in rects.iter().enumerate() {
            let target = (index > 0).then_some(0);
            exact = run_chapter_sample(sample_request(exact, &path, *rect, target))
                .expect("sample")
                .catalog;
        }
        let entry_request = build_library_request(&exact.marks[0], &exact.kinds[0], None);
        assert_eq!(entry_request.calibration.verdict, "separable");
        let entry = loaded_entry("wm-exact", &entry_request);

        // The open chapter: one white occurrence only, so its own fit is the graded one.
        let graded = run_chapter_sample(sample_request(
            ChapterCatalog::default(),
            &path,
            rects[0],
            None,
        ))
        .expect("sample")
        .catalog;
        let mut catalog = graded;
        assert!(matches!(
            catalog.kinds[0].conditioning(),
            ModelConditioning::DepositExact { .. }
        ));
        assert_eq!(catalog.marks[0].crops.len(), 1);

        // A decoy sharing the footprint and the artwork but depositing half as much: the
        // measured colour/greyscale twin of the second chapter. It must NOT match.
        let library = vec![
            library_row("wm-twin", &entry_request, 0.5),
            library_row("wm-exact", &entry_request, 1.0),
        ];
        let matched = match_catalog_against_library(&mut catalog, &library, |id| {
            assert_eq!(id, "wm-exact", "the twin must never be loaded");
            Ok(entry.clone())
        });

        assert_eq!(matched, 1);
        assert_eq!(
            catalog.marks[0].matches.len(),
            1,
            "the artwork-identical twin must be filtered out by its deposit"
        );
        assert_eq!(catalog.marks[0].matches[0].entry_id, "wm-exact");
        assert_eq!(catalog.marks[0].adopted_entry.as_deref(), Some("wm-exact"));
        assert!(
            matches!(
                catalog.kinds[0].conditioning(),
                ModelConditioning::Separable { .. }
            ),
            "adopting the entry must make the chapter's mark exact"
        );
        assert_eq!(catalog.marks[0].crops.len(), entry_request.samples.len());

        // The override: back to the chapter's own single measurement, not to nothing.
        let released = run_chapter_use_match(ChapterMatchRequest {
            catalog,
            index: 0,
            entry: None,
        })
        .expect("release");
        assert!(released.catalog.marks[0].adopted_entry.is_none());
        assert_eq!(released.catalog.marks[0].crops.len(), 1);
        assert!(matches!(
            released.catalog.kinds[0].conditioning(),
            ModelConditioning::DepositExact { .. }
        ));

        let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
    }
}
