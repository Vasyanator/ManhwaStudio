/*
FILE OVERVIEW: crates/ms-tab-translation/src/text_detector/mod.rs
Background text-detector controller for the Translation tab, and the public surface of the
detector module (the `cleaning` tab reaches its region helpers through these re-exports).

Main types:
- `TextDetectorRect`: detected text box in source-image pixels (`ms_text_detect::DetectRect`).
- `TextDetectorPageResult`: per-page detection output (boxes + optional mask).
- `TextDetectorAiCtdOptions`: CTD options (detection size -> plan; region dilation).
- `TextDetectorPaddleOcrOptions`: PaddleOCR options (region dilation).
- `TextDetectorSuryaOptions`: Surya options (none today).
- `TextDetectorRunMode`: detector mode (`Classic` local, `PaddleOcr` native or backend,
  `AiCtd` via backend, `Surya` via backend); `plan_inputs()` is the one mode -> plan-input
  mapping, shared by the page worker and the panel's per-page plan notice.
- `TextDetectorControllerEvent`: UI-facing detection progress/results/errors.
- `TranslationTextDetectorController`: command/event bridge + busy-state lifecycle.

Worker flow:
- `worker_loop` receives `WorkerCommand::Detect` and runs `run_detect_batch`.
- `run_detect_batch` reads the PaddleOCR route once, gates the backend, ensures models,
  processes pages sequentially in background (model engines through
  `pipeline::detect_page_file`), applies the Rust mask dilation to every page result and emits
  progress.

Submodules:
- `classic`: the local classic detector (decode wrapper + pure gray-image pipeline).
- `pipeline`: the model-based entry point (`detect_image`): plan, runner, native -> backend
  fallback, logging, error texts.
- `backend`: the forward-only IPC runner and the backend readiness / model gates.
- `native` (desktop only): native ONNX PaddleOCR route read and runner.
- `region`: in-memory region helpers for the Cleaning tools.

Notes:
Scale, tiling, stitching, postprocess, block order/cap and mask normalization are owned by
`ms_text_detect`; square dilation and Otsu by `ms_raster`. This module drives them, owns the
runners and adapts their output formats.
*/

mod backend;
mod classic;
#[cfg(not(target_arch = "wasm32"))]
mod native;
mod pipeline;
mod region;
#[cfg(test)]
mod tests;

// Public paths kept stable for the `cleaning` tab (`mask_generation.rs`).
pub use ms_text_detect::DetectRect as TextDetectorRect;
pub use region::{detect_ai_ctd_mask_for_image, detect_paddle_mask_for_image, detect_surya_mask_for_image};

use ms_config as config;
use ms_sysprobe::ai_models;
use ms_text_detect::{DetectParams, EngineKind};
use pipeline::DetectorRoute;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use ms_thread::{self as thread, JoinHandle};

const DETECTOR_EVENT_POLL_BUDGET: usize = 128;

#[derive(Debug, Clone)]
pub struct TextDetectorPageResult {
    pub source_size: [u32; 2],
    pub blocks: Vec<TextDetectorRect>,
    pub mask_size: [u32; 2],
    pub mask_alpha: Vec<u8>,
}

/// CTD options. `detect_size` is the requested model tile side (the plan clamps and snaps it,
/// `ms_text_detect::effective_ctd_detect_size`); `mask_dilate_size` is the region helpers' Rust
/// dilation radius (page batches take the radius from `start_detection`).
#[derive(Debug, Clone)]
pub struct TextDetectorAiCtdOptions {
    pub detect_size: i32,
    pub mask_dilate_size: i32,
}

impl Default for TextDetectorAiCtdOptions {
    fn default() -> Self {
        Self { detect_size: 1280, mask_dilate_size: 2 }
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

impl TextDetectorRunMode {
    /// The engine and parameters this mode hands to `ms_text_detect::plan_detection`. The one
    /// mapping from a run mode to its plan: the page worker dispatches through it and the
    /// detector panel's per-page notice plans with it, so the notice announces what runs.
    /// `Classic` maps to [`EngineKind::Classic`], whose plan mirrors the classic detector's own
    /// downscale (`classic::MAX_DETECTOR_DIM`).
    #[must_use]
    pub fn plan_inputs(&self) -> (EngineKind, DetectParams) {
        match self {
            TextDetectorRunMode::Classic => (EngineKind::Classic, DetectParams::default()),
            TextDetectorRunMode::PaddleOcr(_) => (EngineKind::Paddle, DetectParams::default()),
            TextDetectorRunMode::AiCtd(options) => (EngineKind::Ctd, pipeline::ctd_detect_params(options)),
            TextDetectorRunMode::Surya(_) => (EngineKind::Surya, DetectParams::default()),
        }
    }
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
    // The PaddleOCR route is read ONCE per batch (a disk read), so the backend gate below and
    // every page's dispatch agree. Other modes never consult it.
    let route = match &mode {
        TextDetectorRunMode::PaddleOcr(_) => pipeline::paddle_route(),
        TextDetectorRunMode::Classic | TextDetectorRunMode::AiCtd(_) | TextDetectorRunMode::Surya(_) => {
            DetectorRoute::Backend
        }
    };
    // Backend readiness gate — route-aware for PaddleOCR. AiCtd and Surya have no
    // native path and always need the Python backend; classic detection is fully
    // local. PaddleOCR skips the backend when the native ONNX Runtime route is active
    // (native detection loads lazily and runs without the backend). This is what lets
    // native PaddleOCR detection run with the backend offline; a per-page native
    // failure still re-runs that page on the backend (`pipeline::detect_image`), which
    // reports the offline message itself when the backend is down.
    if detector_mode_needs_backend(&mode, route) && let Err(error) = backend::ensure_v2_backend_ready() {
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
        // Model engines plan with `mode.plan_inputs()` (the same inputs the panel notice uses);
        // `route` is `Backend` for every mode but PaddleOCR (see above).
        let detect_result = match &mode {
            TextDetectorRunMode::Classic => classic::detect_page_classic(&path),
            TextDetectorRunMode::PaddleOcr(_) | TextDetectorRunMode::AiCtd(_) | TextDetectorRunMode::Surya(_) => {
                let (engine, params) = mode.plan_inputs();
                pipeline::detect_page_file(&path, engine, &params, route)
            }
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

/// Whether a detector mode needs the Python backend ready at batch start.
///
/// `Classic` is fully local. `AiCtd`/`Surya` have no native path and always need the
/// backend. `PaddleOcr` needs it only when `paddle_route` is not native (the web build always
/// reports `Backend`). Pure: the caller reads the route.
fn detector_mode_needs_backend(mode: &TextDetectorRunMode, paddle_route: DetectorRoute) -> bool {
    match mode {
        TextDetectorRunMode::Classic => false,
        TextDetectorRunMode::AiCtd(_) | TextDetectorRunMode::Surya(_) => true,
        TextDetectorRunMode::PaddleOcr(_) => paddle_route == DetectorRoute::Backend,
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

fn apply_mask_dilation(result: &mut TextDetectorPageResult, dilate_size: i32) {
    dilate_mask_alpha(&mut result.mask_alpha, result.mask_size, dilate_size);
}

/// Dilates a 0/255 detector mask in place by a square of radius `dilate_size` (clamped to
/// 0..=30; 0 is a no-op). The output stays 0/255.
///
/// A mask whose length does not match `mask_size` is left unchanged and the mismatch is
/// logged: the caller's mask is then not dilatable, but it is still a valid result.
fn dilate_mask_alpha(mask_alpha: &mut Vec<u8>, mask_size: [u32; 2], dilate_size: i32) {
    let radius = usize::try_from(dilate_size.clamp(0, 30)).unwrap_or(0);
    if radius == 0 || mask_alpha.is_empty() || mask_size[0] == 0 || mask_size[1] == 0 {
        return;
    }

    let Ok(width) = usize::try_from(mask_size[0]) else {
        return;
    };
    let Ok(height) = usize::try_from(mask_size[1]) else {
        return;
    };
    // `dilate_square` treats any nonzero as set and writes `on` = 255, which is exactly the
    // 0/255 detector mask contract.
    match ms_raster::dilate_square(mask_alpha, width, height, radius, radius, 255) {
        Ok(dilated) => *mask_alpha = dilated,
        Err(err) => {
            ms_log::runtime_log::log_error(format!(
                "[text-detector] mask dilation skipped: {err} (radius {radius})"
            ));
        }
    }
}
