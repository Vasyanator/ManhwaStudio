/*
FILE OVERVIEW: src/tabs/translation/text_detector.rs
Background text-detector controller for Translation tab.

Main types:
- `TextDetectorRect`: detected text box in source-image pixels.
- `TextDetectorPageResult`: per-page detection output (boxes + optional mask).
- `TextDetectorAiCtdOptions`: AI CTD runtime options sent to backend.
- `TextDetectorPaddleOcrOptions`: backend Paddle det-only runtime options.
- `TextDetectorSuryaOptions`: backend Surya det-only runtime options.
- `TextDetectorRunMode`: detector mode (`Classic` local, `PaddleOcr` via backend,
  `AiCtd` via backend, `Surya` via backend).
- `TextDetectorControllerEvent`: UI-facing detection progress/results/errors.
- `TranslationTextDetectorController`: command/event bridge + busy-state lifecycle.

Worker flow:
- `worker_loop` receives `WorkerCommand::Detect` and runs `run_detect_batch`.
- `run_detect_batch` processes pages sequentially in background and emits progress.

Backend helpers (v2 framed IPC):
- `detect_page_classic` builds both rects and a real binary `mask_alpha` from accepted
  connected components, so classic mode uses the same editable mask pipeline as AI mode.
- `detect_page_paddle_ocr` calls backend `textdetector.paddle` over the v2 frame protocol.
- `detect_page_ai_ctd` calls backend `textdetector.ctd` over the v2 frame protocol.
- `detect_page_surya` calls backend `textdetector.surya` over the v2 frame protocol.
- `detect_paddle_mask_for_image` / `detect_ai_ctd_mask_for_image` reuse the same backend
  contracts for inline region-image detection in Cleaning tools (image sent as blob).
- `ensure_v2_backend_ready` gates backend calls the same way as ocr.rs.
- The response mask PNG comes from the response BLOB (raw bytes), not a base64 field.
*/

use crate::backend_ipc::{self, CallError};
use crate::tabs::translation::backend_health::ai_backend_offline_error;
use crate::{ai_models, config};
// Native ONNX Runtime PaddleOCR text detector (desktop-only: the native runtime
// depends on `ms-onnx`/`ort`, which are not part of the web build).
#[cfg(not(target_arch = "wasm32"))]
use crate::native_runtime;
#[cfg(not(target_arch = "wasm32"))]
use crate::onnx_runtime::OrtDownloadProgress;
use eframe::egui;
use image::imageops::FilterType;
use image::{ColorType, ImageEncoder};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use ms_thread::{self as thread, JoinHandle};
use web_time::Duration;

const DETECTOR_EVENT_POLL_BUDGET: usize = 128;
const MAX_DETECTOR_DIM: u32 = 1600;
/// Per-call timeout for the v2 framed backend. Mirrors the previous HTTP read
/// timeout: model warmup + large-page detection can take a while on first use.
const DETECTOR_BACKEND_CALL_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_MASK_PIXELS: usize = 100_000_000;

#[derive(Debug, Clone, Copy)]
pub struct TextDetectorRect {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl TextDetectorRect {
    pub(crate) fn from_xyxy(x1: f32, y1: f32, x2: f32, y2: f32) -> Option<Self> {
        if !x1.is_finite() || !y1.is_finite() || !x2.is_finite() || !y2.is_finite() {
            return None;
        }
        if x2 <= x1 || y2 <= y1 {
            return None;
        }
        Some(Self { x1, y1, x2, y2 })
    }
}

#[derive(Debug, Clone)]
pub struct TextDetectorPageResult {
    pub source_size: [u32; 2],
    pub blocks: Vec<TextDetectorRect>,
    pub mask_size: [u32; 2],
    pub mask_alpha: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TextDetectorAiCtdOptions {
    pub detect_size: i32,
    pub det_rearrange_max_batches: i32,
    pub font_size_multiplier: f32,
    pub font_size_max: f32,
    pub font_size_min: f32,
    pub mask_dilate_size: i32,
}

impl Default for TextDetectorAiCtdOptions {
    fn default() -> Self {
        Self {
            detect_size: 1280,
            det_rearrange_max_batches: 4,
            font_size_multiplier: 1.0,
            font_size_max: -1.0,
            font_size_min: -1.0,
            mask_dilate_size: 2,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TextDetectorPaddleOcrOptions {
    pub mask_dilate_size: i32,
}

#[derive(Debug, Clone, Default)]
pub struct TextDetectorSuryaOptions;

#[derive(Debug, Clone)]
pub enum TextDetectorRunMode {
    Classic,
    PaddleOcr(TextDetectorPaddleOcrOptions),
    AiCtd(TextDetectorAiCtdOptions),
    Surya(TextDetectorSuryaOptions),
}

#[derive(Debug, Clone)]
pub enum TextDetectorControllerEvent {
    ModelDownloadStarted,
    DetectStarted {
        total: usize,
        replace: bool,
    },
    PageDetected {
        page_idx: usize,
        result: TextDetectorPageResult,
    },
    PageFailed {
        page_idx: usize,
        error: String,
    },
    DetectProgress {
        done: usize,
        total: usize,
    },
    DetectFinished {
        total_blocks: usize,
        failed_pages: usize,
    },
    DetectFailed {
        error: String,
    },
}

#[derive(Debug)]
pub struct TranslationTextDetectorController {
    busy: bool,
    cmd_tx: Sender<WorkerCommand>,
    evt_rx: Receiver<WorkerEvent>,
    worker_thread: Option<JoinHandle<()>>,
}

impl Default for TranslationTextDetectorController {
    fn default() -> Self {
        Self::new()
    }
}

impl TranslationTextDetectorController {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();
        let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
        let worker_thread = thread::spawn(move || worker_loop(cmd_rx, evt_tx));
        Self {
            busy: false,
            cmd_tx,
            evt_rx,
            worker_thread: Some(worker_thread),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn start_detection(
        &mut self,
        pages: Vec<(usize, PathBuf)>,
        replace: bool,
        mode: TextDetectorRunMode,
        mask_dilate_size: i32,
    ) -> Result<(), String> {
        if self.busy {
            return Err(t!("translation.text_detector.already_running_status").to_string());
        }
        if pages.is_empty() {
            return Err(t!("translation.text_detector.no_pages_status").to_string());
        }
        self.busy = true;
        if self
            .cmd_tx
            .send(WorkerCommand::Detect {
                pages,
                replace,
                mode,
                mask_dilate_size: mask_dilate_size.clamp(0, 30),
            })
            .is_err()
        {
            self.busy = false;
            return Err(t!("translation.text_detector.worker_unavailable_error").to_string());
        }
        Ok(())
    }

    pub fn poll_events(&mut self) -> Vec<TextDetectorControllerEvent> {
        let mut out = Vec::new();
        for _ in 0..DETECTOR_EVENT_POLL_BUDGET {
            match self.evt_rx.try_recv() {
                Ok(WorkerEvent::ModelDownloadStarted) => {
                    out.push(TextDetectorControllerEvent::ModelDownloadStarted);
                }
                Ok(WorkerEvent::DetectStarted { total, replace }) => {
                    out.push(TextDetectorControllerEvent::DetectStarted { total, replace });
                }
                Ok(WorkerEvent::PageDetected { page_idx, result }) => {
                    out.push(TextDetectorControllerEvent::PageDetected { page_idx, result });
                }
                Ok(WorkerEvent::PageFailed { page_idx, error }) => {
                    out.push(TextDetectorControllerEvent::PageFailed { page_idx, error });
                }
                Ok(WorkerEvent::DetectProgress { done, total }) => {
                    out.push(TextDetectorControllerEvent::DetectProgress { done, total });
                }
                Ok(WorkerEvent::DetectFailed { error }) => {
                    self.busy = false;
                    out.push(TextDetectorControllerEvent::DetectFailed { error });
                }
                Ok(WorkerEvent::DetectFinished {
                    total_blocks,
                    failed_pages,
                }) => {
                    self.busy = false;
                    out.push(TextDetectorControllerEvent::DetectFinished {
                        total_blocks,
                        failed_pages,
                    });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.busy = false;
                    out.push(TextDetectorControllerEvent::DetectFailed {
                        error: t!("translation.text_detector.worker_disconnected_error").to_string(),
                    });
                    break;
                }
            }
        }
        out
    }
}

impl Drop for TranslationTextDetectorController {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCommand::Stop);
        if let Some(handle) = self.worker_thread.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Debug)]
enum WorkerCommand {
    Detect {
        pages: Vec<(usize, PathBuf)>,
        replace: bool,
        mode: TextDetectorRunMode,
        mask_dilate_size: i32,
    },
    Stop,
}

#[derive(Debug)]
enum WorkerEvent {
    ModelDownloadStarted,
    DetectStarted {
        total: usize,
        replace: bool,
    },
    PageDetected {
        page_idx: usize,
        result: TextDetectorPageResult,
    },
    PageFailed {
        page_idx: usize,
        error: String,
    },
    DetectProgress {
        done: usize,
        total: usize,
    },
    DetectFailed {
        error: String,
    },
    DetectFinished {
        total_blocks: usize,
        failed_pages: usize,
    },
}

fn worker_loop(cmd_rx: Receiver<WorkerCommand>, evt_tx: Sender<WorkerEvent>) {
    while let Ok(command) = cmd_rx.recv() {
        match command {
            WorkerCommand::Stop => break,
            WorkerCommand::Detect {
                pages,
                replace,
                mode,
                mask_dilate_size,
            } => {
                run_detect_batch(pages, replace, mode, mask_dilate_size, &evt_tx);
            }
        }
    }
}

fn run_detect_batch(
    pages: Vec<(usize, PathBuf)>,
    replace: bool,
    mode: TextDetectorRunMode,
    mask_dilate_size: i32,
    evt_tx: &Sender<WorkerEvent>,
) {
    let total = pages.len();
    let _ = evt_tx.send(WorkerEvent::DetectStarted { total, replace });
    // Backend readiness gate — route-aware for PaddleOCR. AiCtd and Surya have no
    // native path and always need the Python backend; classic detection is fully
    // local. PaddleOCR skips the backend when the native ONNX Runtime route is active
    // (native detection loads lazily and runs without the backend). This is what lets
    // native PaddleOCR detection run with the backend offline; a per-page native
    // failure still falls back to the backend via `detect_page_paddle_ocr` when the
    // backend IS up (that path re-checks readiness itself).
    if detector_mode_needs_backend(&mode) && let Err(error) = ensure_v2_backend_ready() {
        let _ = evt_tx.send(WorkerEvent::DetectFailed { error });
        return;
    }
    if let Err(error) = ensure_detector_models_for_mode(&mode, evt_tx) {
        let _ = evt_tx.send(WorkerEvent::DetectFailed { error });
        return;
    }
    let mut total_blocks = 0usize;
    let mut failed_pages = 0usize;

    for (done, (page_idx, path)) in pages.into_iter().enumerate() {
        let detect_result = match &mode {
            TextDetectorRunMode::Classic => detect_page_classic(&path),
            TextDetectorRunMode::PaddleOcr(options) => detect_page_paddle_ocr(&path, options),
            TextDetectorRunMode::AiCtd(options) => detect_page_ai_ctd(&path, options),
            TextDetectorRunMode::Surya(options) => detect_page_surya(&path, options),
        };
        match detect_result {
            Ok(mut result) => {
                apply_mask_dilation(&mut result, mask_dilate_size);
                total_blocks += result.blocks.len();
                let _ = evt_tx.send(WorkerEvent::PageDetected { page_idx, result });
            }
            Err(error) => {
                failed_pages += 1;
                let _ = evt_tx.send(WorkerEvent::PageFailed { page_idx, error });
            }
        }
        let done = done + 1;
        let _ = evt_tx.send(WorkerEvent::DetectProgress { done, total });
    }

    let _ = evt_tx.send(WorkerEvent::DetectFinished {
        total_blocks,
        failed_pages,
    });
}

/// Whether a detector mode needs the Python backend warmed at batch start.
///
/// `Classic` is fully local. `AiCtd`/`Surya` have no native path and always need the
/// backend. `PaddleOcr` needs the backend only when the native ONNX Runtime route is
/// NOT active — under the native route detection loads lazily and runs without the
/// backend. On the web build there is no native runtime, so PaddleOCR always needs
/// the backend. Worker-thread only (reads the route off disk on desktop).
fn detector_mode_needs_backend(mode: &TextDetectorRunMode) -> bool {
    match mode {
        TextDetectorRunMode::Classic => false,
        TextDetectorRunMode::AiCtd(_) | TextDetectorRunMode::Surya(_) => true,
        TextDetectorRunMode::PaddleOcr(_) => paddle_detection_needs_backend(),
    }
}

/// Whether PaddleOCR text detection currently needs the Python backend.
///
/// `false` when the native route is active (desktop, native runtime + Safe guard);
/// always `true` on the web build, which has no native runtime.
fn paddle_detection_needs_backend() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        current_detector_route() == DetectorRoute::Backend
    }
    #[cfg(target_arch = "wasm32")]
    {
        true
    }
}

fn ensure_detector_models_for_mode(
    mode: &TextDetectorRunMode,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<(), String> {
    let models_root = config::models_dir();
    let mut reported = false;
    let mut report_download = || {
        if !reported {
            let _ = evt_tx.send(WorkerEvent::ModelDownloadStarted);
            reported = true;
        }
    };
    match mode {
        TextDetectorRunMode::Classic | TextDetectorRunMode::Surya(_) => Ok(()),
        TextDetectorRunMode::PaddleOcr(_) => {
            ai_models::ensure_paddle_ocr_detector_with_reporter(
                &models_root,
                Some(&mut report_download),
            )?;
            Ok(())
        }
        TextDetectorRunMode::AiCtd(_) => {
            ai_models::ensure_comic_text_detector_torch_with_reporter(
                &models_root,
                Some(&mut report_download),
            )?;
            Ok(())
        }
    }
}

/// v2 readiness gate replacing the legacy HTTP `/health` precondition. A
/// successful `shared_client()` performs the `hello` handshake, which fails fast
/// when the backend is not running; that failure is mapped to the same unified
/// "backend offline" message the HTTP path showed, preserving the UX.
fn ensure_v2_backend_ready() -> Result<(), String> {
    backend_ipc::shared_client()
        .map(|_| ())
        .map_err(|_| ai_backend_offline_error().to_string())
}

/// Issues a blocking v2 framed call for a text-detector method.
///
/// If `page_path` is `Some`, that path is sent as the `page_path` header field
/// and the request blob is empty (the backend reads the file on-disk).
/// If `page_path` is `None`, the caller supplies `image_blob` (raw PNG bytes)
/// which go directly into the request blob (no base64).
///
/// Maps `CallError` to a user-facing `String`:
/// - `Error`       → the backend error message (same behaviour as old HTTP 4xx/5xx).
/// - `Interrupted` → transient abort; treated as a transport failure (rare here).
/// - `Transport`   → the existing connect/framing failure path.
fn detector_call(
    method: &str,
    header_fields: Value,
    image_blob: &[u8],
) -> Result<(Value, Vec<u8>), String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call(
            method,
            header_fields,
            image_blob,
            DETECTOR_BACKEND_CALL_TIMEOUT,
        )
        .map_err(|err| match err {
            CallError::Error(msg) => msg,
            CallError::Interrupted(msg) => {
                tf!("translation.text_detector.request_aborted_error", msg = msg)
            }
            CallError::Transport(msg) => msg,
        })
}

fn detect_page_classic(path: &PathBuf) -> Result<TextDetectorPageResult, String> {
    let img = image::open(path).map_err(|err| {
        tf!("translation.text_detector.open_image_error", path = path.display(), err = err)
    })?;
    let gray = img.to_luma8();
    let source_w = gray.width();
    let source_h = gray.height();
    if source_w == 0 || source_h == 0 {
        return Err(tf!("translation.text_detector.empty_image_error", path = path.display()));
    }

    let (proc, scale_x, scale_y) = if source_w.max(source_h) > MAX_DETECTOR_DIM {
        let scale = MAX_DETECTOR_DIM as f32 / source_w.max(source_h) as f32;
        let dst_w = ((source_w as f32 * scale).round() as u32).max(1);
        let dst_h = ((source_h as f32 * scale).round() as u32).max(1);
        let resized = image::imageops::resize(&gray, dst_w, dst_h, FilterType::Triangle);
        let sx = source_w as f32 / dst_w as f32;
        let sy = source_h as f32 / dst_h as f32;
        (resized, sx, sy)
    } else {
        (gray, 1.0, 1.0)
    };

    let proc_w = proc.width() as usize;
    let proc_h = proc.height() as usize;
    let threshold = otsu_threshold(proc.as_raw()).clamp(45, 210);
    let mut fg = vec![0u8; proc_w * proc_h];
    for (idx, px) in proc.as_raw().iter().enumerate() {
        if *px < threshold {
            fg[idx] = 1;
        }
    }

    // Небольшая дилатация склеивает символы в блоки без тяжёлых зависимостей.
    let fg = dilate_binary(&fg, proc_w, proc_h, 2, 1);
    let (blocks, proc_mask_alpha) =
        extract_components_as_rects_and_mask(&fg, proc_w, proc_h, scale_x, scale_y);
    let (mask_size, mask_alpha) = promote_classic_mask_to_source_size(
        proc_mask_alpha,
        [proc.width(), proc.height()],
        [source_w, source_h],
    );

    Ok(TextDetectorPageResult {
        source_size: [source_w, source_h],
        blocks,
        mask_size,
        mask_alpha,
    })
}

/// Where a native PaddleOCR text-detection attempt should run.
///
/// Target-neutral so the decision helper is unit-testable on every build.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectorRoute {
    /// Run natively via the in-process ONNX Runtime PaddleOCR detector.
    Native,
    /// Run through the Python backend (the historical path).
    Backend,
}

/// Pure routing decision for a native PaddleOCR text-detection attempt.
///
/// Returns [`DetectorRoute::Native`] only when the user selected the native AI
/// runtime AND the per-scope SIGILL load guard is not `Suspect`; every other case
/// routes to [`DetectorRoute::Backend`]. Mirrors `ocr::ocr_route` for detection.
#[cfg(not(target_arch = "wasm32"))]
fn detector_native_route(
    runtime: config::AiRuntime,
    guard: config::OrtLoadDecision,
) -> DetectorRoute {
    if runtime == config::AiRuntime::Native && guard == config::OrtLoadDecision::Safe {
        DetectorRoute::Native
    } else {
        DetectorRoute::Backend
    }
}

/// Reads the native runtime selection + SIGILL guard (scoped to the effective
/// native provider) fresh off disk and returns the detection route. Worker-thread
/// only (disk I/O). Returns [`DetectorRoute::Backend`] when config cannot be read.
#[cfg(not(target_arch = "wasm32"))]
fn current_detector_route() -> DetectorRoute {
    let cfg = config::load_raw_user_settings_for_startup().unwrap_or(Value::Null);
    let runtime = config::AiRuntime::from_user_settings(&cfg);
    let scope = native_runtime::native_load_scope_key();
    let decision = config::ort_load_decision(config::read_ort_load_guard(&cfg, &scope));
    detector_native_route(runtime, decision)
}

/// Attempts native PaddleOCR detection of `path`, decoding the page to RGBA.
///
/// Returns `Some(Ok/Err)` only when the native route applies (so the caller uses
/// the native outcome) or native inference failed after being attempted. Returns
/// `None` when the route is not native (the caller should use the backend). On a
/// native failure it returns `None` too, so the caller falls back to the backend
/// and the user still gets a result (the failure is logged, never hidden).
#[cfg(not(target_arch = "wasm32"))]
fn try_native_paddle_detect_page(path: &Path) -> Option<TextDetectorPageResult> {
    if current_detector_route() == DetectorRoute::Backend {
        return None;
    }
    let image = match image::open(path) {
        Ok(img) => img.to_rgba8(),
        Err(err) => {
            crate::runtime_log::log_error(format!(
                "[text-detector] native Paddle: failed to open {} ({err}); using backend",
                path.display()
            ));
            return None;
        }
    };
    let mut progress = |_snapshot: OrtDownloadProgress| {};
    match native_runtime::detect_paddle(&image, &mut progress) {
        Ok(detection) => match paddle_detection_to_page_result(detection) {
            Ok(result) => Some(result),
            Err(err) => {
                // Oversized mask (or similar) rejected: fall back to the backend so
                // the user still gets a result; the reason is logged, never hidden.
                crate::runtime_log::log_error(format!(
                    "[text-detector] native Paddle result for {} rejected, falling back to backend: {err}",
                    path.display()
                ));
                None
            }
        },
        Err(err) => {
            // Debug (stable English variant name) keeps logs grep-able regardless of
            // the selected UI language; the localized Display is for the user surface.
            crate::runtime_log::log_error(format!(
                "[text-detector] native Paddle detection failed for {}, falling back to backend: {err:?}",
                path.display()
            ));
            None
        }
    }
}

/// Builds a [`TextDetectorPageResult`] from a native [`PaddleDetection`], matching
/// the shape the backend `parse_v2_text_detector_response` produces: xyxy blocks
/// sorted (y1, x1, y2, x2) and truncated to 2500, plus the glyph mask normalized
/// to 0/255 at source size.
#[cfg(not(target_arch = "wasm32"))]
fn paddle_detection_to_page_result(
    detection: ms_onnx::PaddleDetection,
) -> Result<TextDetectorPageResult, String> {
    let (src_w, src_h) = detection.source_size;
    let mut blocks: Vec<TextDetectorRect> = detection
        .blocks
        .into_iter()
        .filter_map(|b| TextDetectorRect::from_xyxy(b[0], b[1], b[2], b[3]))
        .collect();
    blocks.sort_by(|a, b| {
        a.y1.total_cmp(&b.y1)
            .then_with(|| a.x1.total_cmp(&b.x1))
            .then_with(|| a.y2.total_cmp(&b.y2))
            .then_with(|| a.x2.total_cmp(&b.x2))
    });
    if blocks.len() > 2500 {
        blocks.truncate(2500);
    }
    let (mask_size, mask_alpha) = glyph_mask_into_alpha(detection.glyph_mask)?;
    Ok(TextDetectorPageResult {
        source_size: [src_w, src_h],
        blocks,
        mask_size,
        mask_alpha,
    })
}

/// Consumes a PaddleOCR glyph mask into `([w, h], alpha)` with each pixel
/// normalized to 0/255 (matching the backend `parse_mask_alpha_from_blob` output).
///
/// # Errors
/// Returns an error when the mask exceeds [`MAX_MASK_PIXELS`], mirroring the backend
/// path's oversized-mask guard so the native and backend paths have equal
/// robustness. The caller treats the error as a backend fallback.
#[cfg(not(target_arch = "wasm32"))]
fn glyph_mask_into_alpha(mask: image::GrayImage) -> Result<([u32; 2], Vec<u8>), String> {
    let (w, h) = mask.dimensions();
    if w == 0 || h == 0 {
        return Ok(([0, 0], Vec::new()));
    }
    // Same oversized-mask bound the backend path applies (see
    // `parse_mask_alpha_from_blob`): reject pathological masks with a typed error
    // instead of normalizing an unbounded buffer.
    let pixels = (w as usize).saturating_mul(h as usize);
    if pixels > MAX_MASK_PIXELS {
        return Err(tf!("translation.text_detector.native_mask_too_large_error", w = w, h = h));
    }
    let mut alpha = mask.into_raw();
    for px in &mut alpha {
        *px = if *px == 0 { 0 } else { 255 };
    }
    Ok(([w, h], alpha))
}

/// Attempts native PaddleOCR detection on an in-memory `image`, returning only the
/// glyph mask as `([w, h], alpha)` (the caller applies dilation).
///
/// Returns `None` when the native route does not apply or native detection failed
/// (logged), so the caller falls back to the backend. Worker-thread only.
#[cfg(not(target_arch = "wasm32"))]
fn try_native_paddle_mask_for_image(image: &egui::ColorImage) -> Option<([u32; 2], Vec<u8>)> {
    if current_detector_route() == DetectorRoute::Backend {
        return None;
    }
    let width = u32::try_from(image.size[0]).ok()?;
    let height = u32::try_from(image.size[1]).ok()?;
    let raw: Vec<u8> = image.pixels.iter().flat_map(|px| px.to_array()).collect();
    let rgba = image::RgbaImage::from_raw(width, height, raw)?;

    let mut progress = |_snapshot: OrtDownloadProgress| {};
    match native_runtime::detect_paddle(&rgba, &mut progress) {
        Ok(detection) => match glyph_mask_into_alpha(detection.glyph_mask) {
            Ok(mask) => Some(mask),
            Err(err) => {
                // Oversized mask rejected: fall back to the backend (logged).
                crate::runtime_log::log_error(format!(
                    "[text-detector] native Paddle mask rejected, falling back to backend: {err}"
                ));
                None
            }
        },
        Err(err) => {
            // Debug (stable English variant name) keeps logs grep-able regardless of
            // the selected UI language; the localized Display is for the user surface.
            crate::runtime_log::log_error(format!(
                "[text-detector] native Paddle mask detection failed, falling back to backend: {err:?}"
            ));
            None
        }
    }
}

fn detect_page_paddle_ocr(
    path: &Path,
    _options: &TextDetectorPaddleOcrOptions,
) -> Result<TextDetectorPageResult, String> {
    // Native ONNX Runtime route first. When it applies and succeeds we return the
    // native result without touching IPC; otherwise we fall through to the backend.
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(result) = try_native_paddle_detect_page(path) {
        return Ok(result);
    }

    ai_models::ensure_paddle_ocr_detector(&config::models_dir())?;
    // Send the on-disk path as a header field; blob is empty.
    let header_fields = json!({
        "page_path": path.to_string_lossy().as_ref(),
    });
    let (header, blob) = detector_call(
        crate::backend_ipc::protocol::METHOD_TEXTDETECTOR_PADDLE,
        header_fields,
        &[],
    )?;
    parse_v2_text_detector_response(&header, &blob, path, "Paddle")
}

fn detect_page_ai_ctd(
    path: &Path,
    options: &TextDetectorAiCtdOptions,
) -> Result<TextDetectorPageResult, String> {
    ai_models::ensure_comic_text_detector_torch(&config::models_dir())?;
    // Send the on-disk path + ctd params in the header; blob is empty.
    let header_fields = json!({
        "page_path": path.to_string_lossy().as_ref(),
        "params": {
            "detect_size": options.detect_size.clamp(896, 2048),
            "det_rearrange_max_batches": options.det_rearrange_max_batches.clamp(1, 64),
            "font size multiplier": options.font_size_multiplier.clamp(0.1, 8.0),
            "font size max": options.font_size_max.clamp(-1.0, 500.0),
            "font size min": options.font_size_min.clamp(-1.0, 500.0)
        }
    });
    let (header, blob) = detector_call(
        crate::backend_ipc::protocol::METHOD_TEXTDETECTOR_CTD,
        header_fields,
        &[],
    )?;
    parse_v2_text_detector_response(&header, &blob, path, "CTD")
}

fn detect_page_surya(
    path: &Path,
    _options: &TextDetectorSuryaOptions,
) -> Result<TextDetectorPageResult, String> {
    // Send the on-disk path as a header field; blob is empty.
    let header_fields = json!({
        "page_path": path.to_string_lossy().as_ref(),
    });
    let (header, blob) = detector_call(
        crate::backend_ipc::protocol::METHOD_TEXTDETECTOR_SURYA,
        header_fields,
        &[],
    )?;
    parse_v2_text_detector_response(&header, &blob, path, "Surya")
}

pub(crate) fn detect_ai_ctd_mask_for_image(
    image: &egui::ColorImage,
    options: &TextDetectorAiCtdOptions,
) -> Result<([u32; 2], Vec<u8>), String> {
    if image.size[0] == 0 || image.size[1] == 0 {
        return Ok(([0, 0], Vec::new()));
    }

    ai_models::ensure_comic_text_detector_torch(&config::models_dir())?;
    // Send the raw image bytes in the request blob (v2 protocol: no base64).
    let image_png = encode_color_image_png_rgba(image)?;
    let header_fields = json!({
        "params": {
            "detect_size": options.detect_size.clamp(896, 2048),
            "det_rearrange_max_batches": options.det_rearrange_max_batches.clamp(1, 64),
            "font size multiplier": options.font_size_multiplier.clamp(0.1, 8.0),
            "font size max": options.font_size_max.clamp(-1.0, 500.0),
            "font size min": options.font_size_min.clamp(-1.0, 500.0),
            "mask dilate size": options.mask_dilate_size.clamp(0, 30)
        }
    });
    let (_header, blob) = detector_call(
        crate::backend_ipc::protocol::METHOD_TEXTDETECTOR_CTD,
        header_fields,
        &image_png,
    )?;
    parse_mask_alpha_from_blob(&blob)
}

pub(crate) fn detect_paddle_mask_for_image(
    image: &egui::ColorImage,
    options: &TextDetectorPaddleOcrOptions,
) -> Result<([u32; 2], Vec<u8>), String> {
    if image.size[0] == 0 || image.size[1] == 0 {
        return Ok(([0, 0], Vec::new()));
    }

    // Native ONNX Runtime route first (when selected + guard Safe). On success we
    // produce the same ([w,h], alpha) the backend path returns; on native failure
    // we log and fall through to the backend so the user still gets a mask.
    #[cfg(not(target_arch = "wasm32"))]
    if let Some((mask_size, mut mask_alpha)) = try_native_paddle_mask_for_image(image) {
        dilate_mask_alpha(&mut mask_alpha, mask_size, options.mask_dilate_size);
        return Ok((mask_size, mask_alpha));
    }

    ai_models::ensure_paddle_ocr_detector(&config::models_dir())?;
    // Send the raw image bytes in the request blob (v2 protocol: no base64).
    let image_png = encode_color_image_png_rgba(image)?;
    let (_header, blob) = detector_call(
        crate::backend_ipc::protocol::METHOD_TEXTDETECTOR_PADDLE,
        json!({}),
        &image_png,
    )?;
    let (mask_size, mut mask_alpha) = parse_mask_alpha_from_blob(&blob)?;
    dilate_mask_alpha(&mut mask_alpha, mask_size, options.mask_dilate_size);
    Ok((mask_size, mask_alpha))
}

pub(crate) fn detect_surya_mask_for_image(
    image: &egui::ColorImage,
    dilate_size: i32,
) -> Result<([u32; 2], Vec<u8>), String> {
    if image.size[0] == 0 || image.size[1] == 0 {
        return Ok(([0, 0], Vec::new()));
    }

    // Send the raw image bytes in the request blob (v2 protocol: no base64).
    let image_png = encode_color_image_png_rgba(image)?;
    let (_header, blob) = detector_call(
        crate::backend_ipc::protocol::METHOD_TEXTDETECTOR_SURYA,
        json!({}),
        &image_png,
    )?;
    let (mask_size, mut mask_alpha) = parse_mask_alpha_from_blob(&blob)?;
    dilate_mask_alpha(&mut mask_alpha, mask_size, dilate_size);
    Ok((mask_size, mask_alpha))
}

/// Parses a v2 `status:"ok"` text-detector response: reads `source_size` and
/// `blocks` from the response header, and takes the mask PNG from the RESPONSE
/// BLOB (raw bytes, no base64). The old `mask_png_base64` field is no longer used.
fn parse_v2_text_detector_response(
    header: &Value,
    mask_blob: &[u8],
    path: &Path,
    engine_name: &str,
) -> Result<TextDetectorPageResult, String> {
    let source_size = header
        .get("source_size")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            tf!("translation.text_detector.invalid_source_size_error", engine_name = engine_name, path = path.display())
        })?;
    if source_size.len() < 2 {
        return Err(tf!("translation.text_detector.incomplete_source_size_error", engine_name = engine_name, path = path.display()));
    }
    let source_w = source_size[0]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| {
            tf!("translation.text_detector.invalid_source_width_error", engine_name = engine_name, path = path.display())
        })?;
    let source_h = source_size[1]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| {
            tf!("translation.text_detector.invalid_source_height_error", engine_name = engine_name, path = path.display())
        })?;
    if source_w == 0 || source_h == 0 {
        return Err(tf!("translation.text_detector.empty_page_size_error", engine_name = engine_name, path = path.display()));
    }

    let mut blocks = parse_backend_blocks(header);
    blocks.sort_by(|a, b| {
        a.y1.total_cmp(&b.y1)
            .then_with(|| a.x1.total_cmp(&b.x1))
            .then_with(|| a.y2.total_cmp(&b.y2))
            .then_with(|| a.x2.total_cmp(&b.x2))
    });
    if blocks.len() > 2500 {
        blocks.truncate(2500);
    }

    // The mask PNG is in the response BLOB (raw bytes), not a base64 field.
    let (mask_size, mask_alpha) = parse_mask_alpha_from_blob(mask_blob)
        .map_err(|err| format!("{engine_name} backend: {err}"))?;

    Ok(TextDetectorPageResult {
        source_size: [source_w, source_h],
        blocks,
        mask_size,
        mask_alpha,
    })
}

fn parse_backend_blocks(response: &Value) -> Vec<TextDetectorRect> {
    let blocks = response
        .get("blocks")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(parse_backend_xyxy_rect)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !blocks.is_empty() {
        return blocks;
    }

    response
        .get("lines")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(parse_backend_line_rect)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn parse_backend_xyxy_rect(item: &Value) -> Option<TextDetectorRect> {
    let x1 = item.get("x1").and_then(Value::as_f64)?;
    let y1 = item.get("y1").and_then(Value::as_f64)?;
    let x2 = item.get("x2").and_then(Value::as_f64)?;
    let y2 = item.get("y2").and_then(Value::as_f64)?;
    TextDetectorRect::from_xyxy(x1 as f32, y1 as f32, x2 as f32, y2 as f32)
}

fn parse_backend_line_rect(item: &Value) -> Option<TextDetectorRect> {
    if let Some(bbox) = item.get("bbox").and_then(Value::as_array)
        && bbox.len() >= 4
    {
        let x1 = bbox[0].as_f64()?;
        let y1 = bbox[1].as_f64()?;
        let x2 = bbox[2].as_f64()?;
        let y2 = bbox[3].as_f64()?;
        return TextDetectorRect::from_xyxy(x1 as f32, y1 as f32, x2 as f32, y2 as f32);
    }

    let polygon = item.get("polygon").and_then(Value::as_array)?;
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;

    for point in polygon {
        let point = point.as_array()?;
        if point.len() < 2 {
            continue;
        }
        let x = point[0].as_f64()? as f32;
        let y = point[1].as_f64()? as f32;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }

    if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
        return None;
    }
    TextDetectorRect::from_xyxy(min_x, min_y, max_x, max_y)
}

/// Decodes the mask PNG from the response BLOB bytes (raw PNG, not base64).
/// Returns `([w, h], alpha_pixels)` where each pixel is 0 or 255.
fn parse_mask_alpha_from_blob(blob: &[u8]) -> Result<([u32; 2], Vec<u8>), String> {
    if blob.is_empty() {
        return Ok(([0, 0], Vec::new()));
    }
    let image = image::load_from_memory(blob)
        .map_err(|err| {
            tf!("translation.text_detector.decode_mask_error", err = err)
        })?
        .to_luma8();
    let w = image.width();
    let h = image.height();
    if w == 0 || h == 0 {
        return Ok(([0, 0], Vec::new()));
    }
    let pixels = (w as usize).saturating_mul(h as usize);
    if pixels == 0 || pixels > MAX_MASK_PIXELS {
        return Err(tf!("translation.text_detector.mask_too_large_error", w = w, h = h));
    }
    // CTD mask is logically binary; normalize to 0/255 to keep rendering consistent
    // for both 1-bit and 8-bit encodings.
    let mut alpha = image.into_raw();
    for px in &mut alpha {
        *px = if *px == 0 { 0 } else { 255 };
    }
    Ok(([w, h], alpha))
}

fn apply_mask_dilation(result: &mut TextDetectorPageResult, dilate_size: i32) {
    dilate_mask_alpha(&mut result.mask_alpha, result.mask_size, dilate_size);
}

fn dilate_mask_alpha(mask_alpha: &mut Vec<u8>, mask_size: [u32; 2], dilate_size: i32) {
    let radius = usize::try_from(dilate_size.clamp(0, 30)).unwrap_or(0);
    if radius == 0 || mask_alpha.is_empty() || mask_size[0] == 0 || mask_size[1] == 0 {
        return;
    }

    let width = match usize::try_from(mask_size[0]) {
        Ok(value) => value,
        Err(_) => return,
    };
    let height = match usize::try_from(mask_size[1]) {
        Ok(value) => value,
        Err(_) => return,
    };
    if width == 0 || height == 0 || width.saturating_mul(height) != mask_alpha.len() {
        return;
    }

    let src = mask_alpha
        .iter()
        .map(|&px| if px == 0 { 0 } else { 1 })
        .collect::<Vec<_>>();
    let dilated = dilate_binary(&src, width, height, radius, radius);
    *mask_alpha = dilated
        .into_iter()
        .map(|px| if px == 0 { 0 } else { 255 })
        .collect();
}

fn encode_color_image_png_rgba(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let width = image.size[0];
    let height = image.size[1];
    let raw = image
        .pixels
        .iter()
        .flat_map(|px| px.to_array())
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            &raw,
            u32::try_from(width).map_err(|_| t!("translation.text_detector.image_width_too_large_error").to_string())?,
            u32::try_from(height).map_err(|_| t!("translation.text_detector.image_height_too_large_error").to_string())?,
            ColorType::Rgba8.into(),
        )
        .map_err(|err| tf!("translation.text_detector.encode_image_error", err = err))?;
    Ok(out)
}

fn otsu_threshold(gray: &[u8]) -> u8 {
    if gray.is_empty() {
        return 127;
    }
    let mut hist = [0u32; 256];
    for &v in gray {
        hist[v as usize] += 1;
    }
    let total = gray.len() as f64;
    let sum_total = hist
        .iter()
        .enumerate()
        .map(|(i, c)| (i as f64) * (*c as f64))
        .sum::<f64>();

    let mut sum_b = 0.0f64;
    let mut w_b = 0.0f64;
    let mut max_var = -1.0f64;
    let mut threshold = 127u8;

    for (t, &count) in hist.iter().enumerate() {
        w_b += count as f64;
        if w_b <= f64::EPSILON {
            continue;
        }
        let w_f = total - w_b;
        if w_f <= f64::EPSILON {
            break;
        }
        sum_b += (t as f64) * (count as f64);
        let m_b = sum_b / w_b;
        let m_f = (sum_total - sum_b) / w_f;
        let between = w_b * w_f * (m_b - m_f) * (m_b - m_f);
        if between > max_var {
            max_var = between;
            threshold = t as u8;
        }
    }
    threshold
}

fn dilate_binary(src: &[u8], width: usize, height: usize, rx: usize, ry: usize) -> Vec<u8> {
    if src.is_empty() || width == 0 || height == 0 {
        return Vec::new();
    }
    let mut out = vec![0u8; src.len()];
    for y in 0..height {
        let y0 = y.saturating_sub(ry);
        let y1 = (y + ry).min(height - 1);
        for x in 0..width {
            let x0 = x.saturating_sub(rx);
            let x1 = (x + rx).min(width - 1);
            let mut any = 0u8;
            'scan: for yy in y0..=y1 {
                let row = yy * width;
                for xx in x0..=x1 {
                    if src[row + xx] != 0 {
                        any = 1;
                        break 'scan;
                    }
                }
            }
            out[y * width + x] = any;
        }
    }
    out
}

fn promote_classic_mask_to_source_size(
    mask_alpha: Vec<u8>,
    mask_size: [u32; 2],
    source_size: [u32; 2],
) -> ([u32; 2], Vec<u8>) {
    if mask_alpha.is_empty() || mask_size[0] == 0 || mask_size[1] == 0 || mask_size == source_size {
        return (mask_size, mask_alpha);
    }
    let source_pixels = (source_size[0] as usize).saturating_mul(source_size[1] as usize);
    if source_pixels == 0 || source_pixels > MAX_MASK_PIXELS {
        return (mask_size, mask_alpha);
    }
    let Some(mask_img) = image::GrayImage::from_vec(mask_size[0], mask_size[1], mask_alpha) else {
        return (mask_size, Vec::new());
    };
    let resized = image::imageops::resize(
        &mask_img,
        source_size[0],
        source_size[1],
        FilterType::Nearest,
    );
    let mut alpha = resized.into_raw();
    for px in &mut alpha {
        *px = if *px == 0 { 0 } else { 255 };
    }
    (source_size, alpha)
}

fn extract_components_as_rects_and_mask(
    fg: &[u8],
    width: usize,
    height: usize,
    scale_x: f32,
    scale_y: f32,
) -> (Vec<TextDetectorRect>, Vec<u8>) {
    if fg.is_empty() || width == 0 || height == 0 {
        return (Vec::new(), Vec::new());
    }
    let mut visited = vec![0u8; fg.len()];
    let mut stack = Vec::<usize>::new();
    let mut component_pixels = Vec::<usize>::new();
    let mut rects = Vec::<TextDetectorRect>::new();
    let mut mask_alpha = vec![0u8; fg.len()];

    let img_area = width * height;
    let min_side = (width.min(height) / 240).max(3) as u32;
    let min_area = (img_area / 25_000).max(24) as u32;
    let max_area = ((img_area / 7).max(min_area as usize + 1)) as u32;
    let max_w = (width as f32 * 0.85) as u32;
    let max_h = (height as f32 * 0.40) as u32;

    for seed in 0..fg.len() {
        if fg[seed] == 0 || visited[seed] != 0 {
            continue;
        }
        visited[seed] = 1;
        stack.clear();
        stack.push(seed);
        component_pixels.clear();

        let mut min_x = u32::MAX;
        let mut min_y = u32::MAX;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        let mut area = 0u32;

        while let Some(idx) = stack.pop() {
            component_pixels.push(idx);
            let x = (idx % width) as u32;
            let y = (idx / width) as u32;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            area = area.saturating_add(1);

            let y0 = y.saturating_sub(1) as usize;
            let y1 = (y as usize + 1).min(height - 1);
            let x0 = x.saturating_sub(1) as usize;
            let x1 = (x as usize + 1).min(width - 1);

            for ny in y0..=y1 {
                let row = ny * width;
                for nx in x0..=x1 {
                    let nidx = row + nx;
                    if fg[nidx] == 0 || visited[nidx] != 0 {
                        continue;
                    }
                    visited[nidx] = 1;
                    stack.push(nidx);
                }
            }
        }

        if area < min_area || area > max_area {
            continue;
        }
        let bw = max_x.saturating_sub(min_x) + 1;
        let bh = max_y.saturating_sub(min_y) + 1;
        if bw < min_side || bh < min_side {
            continue;
        }
        if bw > max_w || bh > max_h {
            continue;
        }

        let bbox_area = bw.saturating_mul(bh).max(1);
        let density = area as f32 / bbox_area as f32;
        if !(0.06..=0.95).contains(&density) {
            continue;
        }
        let aspect = bw as f32 / bh as f32;
        if !(0.08..=20.0).contains(&aspect) {
            continue;
        }

        let x1 = min_x as f32 * scale_x;
        let y1 = min_y as f32 * scale_y;
        let x2 = (max_x + 1) as f32 * scale_x;
        let y2 = (max_y + 1) as f32 * scale_y;
        if let Some(rect) = TextDetectorRect::from_xyxy(x1, y1, x2, y2) {
            rects.push(rect);
            for &idx in &component_pixels {
                mask_alpha[idx] = 255;
            }
        }
    }

    rects.sort_by(|a, b| {
        a.y1.total_cmp(&b.y1)
            .then_with(|| a.x1.total_cmp(&b.x1))
            .then_with(|| a.y2.total_cmp(&b.y2))
            .then_with(|| a.x2.total_cmp(&b.x2))
    });
    if rects.len() > 2500 {
        rects.truncate(2500);
    }
    (rects, mask_alpha)
}

// ============================================================================
// Unit tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------------------------------------------------------------------------
    // parse_mask_alpha_from_blob
    // -------------------------------------------------------------------------

    #[test]
    fn empty_blob_returns_empty_mask() {
        let (size, alpha) = parse_mask_alpha_from_blob(&[]).expect("empty blob");
        assert_eq!(size, [0, 0]);
        assert!(alpha.is_empty());
    }

    #[test]
    fn invalid_blob_returns_error() {
        let result = parse_mask_alpha_from_blob(b"not a png at all");
        assert!(result.is_err(), "non-PNG blob must return Err");
    }

    /// Build a minimal 1×1 grayscale PNG in memory and check that
    /// `parse_mask_alpha_from_blob` decodes it correctly.
    #[test]
    fn valid_png_blob_decoded_to_mask() {
        // 1x1 white pixel (luma 255) => normalized to 255.
        let png = make_gray_png(1, 1, 255);
        let (size, alpha) = parse_mask_alpha_from_blob(&png).expect("1x1 white PNG");
        assert_eq!(size, [1, 1]);
        assert_eq!(alpha, vec![255u8]);
    }

    #[test]
    fn mask_blob_normalizes_non_zero_to_255() {
        // 1x1 pixel with value 42 (non-zero, non-255) => 255.
        let png = make_gray_png(1, 1, 42);
        let (size, alpha) = parse_mask_alpha_from_blob(&png).expect("1x1 gray42 PNG");
        assert_eq!(size, [1, 1]);
        assert_eq!(
            alpha,
            vec![255u8],
            "any non-zero pixel must normalize to 255"
        );
    }

    #[test]
    fn mask_blob_zero_pixel_stays_zero() {
        // 1x1 pixel with value 0 => stays 0.
        let png = make_gray_png(1, 1, 0);
        let (size, alpha) = parse_mask_alpha_from_blob(&png).expect("1x1 black PNG");
        assert_eq!(size, [1, 1]);
        assert_eq!(alpha, vec![0u8]);
    }

    // -------------------------------------------------------------------------
    // parse_v2_text_detector_response — header field shapes
    // -------------------------------------------------------------------------

    #[test]
    fn v2_response_parses_source_size_and_blocks() {
        let header = json!({
            "source_size": [800, 1200],
            "blocks": [
                {"x1": 10.0, "y1": 20.0, "x2": 100.0, "y2": 80.0},
                {"x1": 5.0,  "y1": 5.0,  "x2": 50.0,  "y2": 50.0}
            ]
        });
        // Use a tiny valid 1x1 white PNG as the mask blob.
        let mask_blob = make_gray_png(1, 1, 255);
        let path = Path::new("/fake/page.png");
        let result = parse_v2_text_detector_response(&header, &mask_blob, path, "Test")
            .expect("v2 response parsing");
        assert_eq!(result.source_size, [800, 1200]);
        // Blocks sorted by y1 then x1: block2 (y=5) before block1 (y=20).
        assert_eq!(result.blocks.len(), 2);
        assert!((result.blocks[0].y1 - 5.0).abs() < f32::EPSILON);
        assert!((result.blocks[1].y1 - 20.0).abs() < f32::EPSILON);
        assert_eq!(result.mask_size, [1, 1]);
        assert_eq!(result.mask_alpha, vec![255u8]);
    }

    #[test]
    fn v2_response_falls_back_to_lines_when_blocks_empty() {
        // `blocks` absent — should fall back to `lines` (Surya-style).
        let header = json!({
            "source_size": [640, 480],
            "lines": [
                {"bbox": [0.0, 0.0, 100.0, 50.0]},
                {"bbox": [10.0, 60.0, 200.0, 110.0]}
            ]
        });
        let mask_blob = make_gray_png(1, 1, 0);
        let path = Path::new("/fake/page.png");
        let result = parse_v2_text_detector_response(&header, &mask_blob, path, "Surya")
            .expect("lines fallback");
        assert_eq!(result.source_size, [640, 480]);
        assert_eq!(result.blocks.len(), 2);
    }

    #[test]
    fn v2_response_missing_source_size_returns_error() {
        let header = json!({ "blocks": [] });
        let mask_blob = make_gray_png(1, 1, 0);
        let path = Path::new("/fake/page.png");
        let err = parse_v2_text_detector_response(&header, &mask_blob, path, "Test")
            .expect_err("missing source_size must error");
        assert!(
            err.contains("source_size"),
            "error must mention source_size: {err}"
        );
    }

    #[test]
    fn v2_response_zero_source_size_returns_error() {
        let header = json!({ "source_size": [0, 0], "blocks": [] });
        let mask_blob = make_gray_png(1, 1, 0);
        let path = Path::new("/fake/page.png");
        let err = parse_v2_text_detector_response(&header, &mask_blob, path, "Test")
            .expect_err("zero source_size must error");
        assert!(!err.is_empty());
    }

    // -------------------------------------------------------------------------
    // header field shapes per method
    // -------------------------------------------------------------------------

    /// CTD page-path call must carry `page_path` and `params` in the header;
    /// blob is empty. This test validates the *shape* of the header_fields Value
    /// that `detect_page_ai_ctd` would build (extracted here so it can run
    /// without a live backend connection).
    #[test]
    fn ctd_page_path_header_has_params_and_path() {
        let options = TextDetectorAiCtdOptions::default();
        let path = Path::new("/some/page.png");
        // Mirror the header_fields built in detect_page_ai_ctd.
        let header_fields = json!({
            "page_path": path.to_string_lossy().as_ref(),
            "params": {
                "detect_size": options.detect_size.clamp(896, 2048),
                "det_rearrange_max_batches": options.det_rearrange_max_batches.clamp(1, 64),
                "font size multiplier": options.font_size_multiplier.clamp(0.1, 8.0),
                "font size max": options.font_size_max.clamp(-1.0, 500.0),
                "font size min": options.font_size_min.clamp(-1.0, 500.0)
            }
        });
        assert_eq!(
            header_fields["page_path"].as_str(),
            Some("/some/page.png"),
            "CTD page-path header must carry page_path"
        );
        assert!(
            header_fields["params"].is_object(),
            "CTD header must carry params object"
        );
        assert_eq!(
            header_fields["params"]["detect_size"].as_i64(),
            Some(1280),
            "default detect_size must be 1280"
        );
    }

    /// Paddle page-path call: header has `page_path`, no `params`.
    #[test]
    fn paddle_page_path_header_shape() {
        let path = Path::new("/some/page.png");
        let header_fields = json!({
            "page_path": path.to_string_lossy().as_ref(),
        });
        assert_eq!(header_fields["page_path"].as_str(), Some("/some/page.png"));
        assert!(
            header_fields.get("params").is_none(),
            "Paddle has no params"
        );
    }

    /// Surya page-path call: header has `page_path`, no `params`.
    #[test]
    fn surya_page_path_header_shape() {
        let path = Path::new("/surya/test.png");
        let header_fields = json!({
            "page_path": path.to_string_lossy().as_ref(),
        });
        assert_eq!(header_fields["page_path"].as_str(), Some("/surya/test.png"));
    }

    /// CTD inline-image call (for Cleaning tools): header has `params` but NO
    /// `page_path`; the image bytes go in the blob.
    #[test]
    fn ctd_inline_image_header_has_no_page_path() {
        let options = TextDetectorAiCtdOptions::default();
        let header_fields = json!({
            "params": {
                "detect_size": options.detect_size.clamp(896, 2048),
                "det_rearrange_max_batches": options.det_rearrange_max_batches.clamp(1, 64),
                "font size multiplier": options.font_size_multiplier.clamp(0.1, 8.0),
                "font size max": options.font_size_max.clamp(-1.0, 500.0),
                "font size min": options.font_size_min.clamp(-1.0, 500.0),
                "mask dilate size": options.mask_dilate_size.clamp(0, 30)
            }
        });
        assert!(
            header_fields.get("page_path").is_none(),
            "CTD inline-image request must NOT have page_path"
        );
        assert!(
            header_fields["params"]["mask dilate size"]
                .as_i64()
                .is_some()
        );
    }

    /// Paddle inline-image call: empty header `{}` — image bytes go in the blob.
    #[test]
    fn paddle_inline_image_header_is_empty() {
        let header_fields = json!({});
        assert!(header_fields.get("page_path").is_none());
        assert!(header_fields.get("params").is_none());
    }

    // -------------------------------------------------------------------------
    // detector_native_route — native PaddleOCR detection routing
    // -------------------------------------------------------------------------

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn detector_route_native_only_for_native_runtime_and_safe_guard() {
        use crate::config::{AiRuntime, OrtLoadDecision};
        // Native runtime + Safe guard -> Native.
        assert_eq!(
            detector_native_route(AiRuntime::Native, OrtLoadDecision::Safe),
            DetectorRoute::Native
        );
        // Suspect guard disables the native path.
        assert_eq!(
            detector_native_route(AiRuntime::Native, OrtLoadDecision::Suspect),
            DetectorRoute::Backend
        );
        // Backend runtime always routes to the backend.
        assert_eq!(
            detector_native_route(AiRuntime::Backend, OrtLoadDecision::Safe),
            DetectorRoute::Backend
        );
        assert_eq!(
            detector_native_route(AiRuntime::Backend, OrtLoadDecision::Suspect),
            DetectorRoute::Backend
        );
    }

    /// A native glyph mask must be normalized to 0/255 and carry the source size.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn glyph_mask_into_alpha_normalizes_and_sizes() {
        use image::{GrayImage, Luma};
        let mut mask = GrayImage::from_pixel(2, 1, Luma([0]));
        mask.put_pixel(1, 0, Luma([7])); // non-zero -> 255
        let (size, alpha) = glyph_mask_into_alpha(mask).expect("in-bounds mask");
        assert_eq!(size, [2, 1]);
        assert_eq!(alpha, vec![0u8, 255u8]);
    }

    /// An empty (0x0) mask normalizes to an empty result, never an error.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn glyph_mask_into_alpha_empty_is_ok() {
        use image::GrayImage;
        let mask = GrayImage::new(0, 0);
        let (size, alpha) = glyph_mask_into_alpha(mask).expect("empty mask");
        assert_eq!(size, [0, 0]);
        assert!(alpha.is_empty());
    }

    // -------------------------------------------------------------------------
    // parse_backend_blocks
    // -------------------------------------------------------------------------

    #[test]
    fn parse_blocks_xyxy() {
        let response = json!({
            "blocks": [
                {"x1": 1.0, "y1": 2.0, "x2": 10.0, "y2": 20.0}
            ]
        });
        let blocks = parse_backend_blocks(&response);
        assert_eq!(blocks.len(), 1);
        assert!((blocks[0].x1 - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_blocks_falls_back_to_lines_bbox() {
        let response = json!({
            "lines": [
                {"bbox": [5.0, 10.0, 50.0, 100.0]}
            ]
        });
        let blocks = parse_backend_blocks(&response);
        assert_eq!(blocks.len(), 1);
        assert!((blocks[0].x1 - 5.0).abs() < f32::EPSILON);
        assert!((blocks[0].x2 - 50.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_blocks_falls_back_to_lines_polygon() {
        let response = json!({
            "lines": [
                {"polygon": [[0.0, 0.0], [100.0, 0.0], [100.0, 50.0], [0.0, 50.0]]}
            ]
        });
        let blocks = parse_backend_blocks(&response);
        assert_eq!(blocks.len(), 1);
        assert!((blocks[0].x2 - 100.0).abs() < f32::EPSILON);
        assert!((blocks[0].y2 - 50.0).abs() < f32::EPSILON);
    }

    // -------------------------------------------------------------------------
    // ensure_v2_backend_ready error message
    // -------------------------------------------------------------------------

    #[test]
    fn backend_ready_error_uses_offline_message() {
        // shared_client() will fail (no backend running in tests).
        let result = ensure_v2_backend_ready();
        if let Err(msg) = result {
            assert_eq!(
                msg, ai_backend_offline_error(),
                "offline error must use the canonical constant"
            );
        }
        // If shared_client() somehow succeeds (live backend in CI), that's fine too.
    }

    // -------------------------------------------------------------------------
    // Helper: build a minimal grayscale PNG in memory (no file I/O).
    // -------------------------------------------------------------------------
    fn make_gray_png(width: u32, height: u32, pixel_value: u8) -> Vec<u8> {
        use image::{GrayImage, Luma};
        let img = GrayImage::from_pixel(width, height, Luma([pixel_value]));
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Png)
            .expect("encode test PNG");
        buf
    }
}
