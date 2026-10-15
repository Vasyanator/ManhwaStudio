/*
FILE OVERVIEW: crates/ms-tab-translation/src/text_detector/backend.rs
Python-backend side of the text detector: the forward-only IPC runner
(`textdetector.{ctd,paddle,surya}.forward`, protocol v4) that the `ms_text_detect` pipeline
drives, and the backend readiness / model gates around it.

Key items:
- `ensure_v2_backend_ready()`: readiness gate (the `hello` handshake of `shared_client()`).
- `DetectorTransport` / `SharedClientTransport`: the one call seam (production = the shared
  framed-IPC client; tests substitute a fake to exercise response validation).
- `IpcRunner`: `ms_text_detect::ProbMapRunner` over the transport — packs tiles with
  `ms_backend_ipc::textdetector::build_forward_request`, validates the reply with
  `decode_forward_response`, and wraps each map into a `ProbMap`.
- `ipc_max_batch()`: tiles per request = min(`IPC_MAX_TILES_PER_REQUEST`, the tiles that fit
  `IPC_FRAME_BUDGET_PERCENT` (90 %) of the frame in the larger direction, computed by
  `ms_backend_ipc::textdetector::max_tiles_per_request`), at least 1.
- `forward_engine()` / `ensure_backend_models()`: engine mapping and app-managed model gate;
  for Surya the gate is stat-only (`surya_detection_dir_in`: the pinned
  `SURYA_DETECTOR_SPECS` checkpoint must be installed, else the localized "not downloaded"
  error naming the detector panel's download button) and its directory travels in every
  forward header as `surya_detection_dir` (protocol v12; merged by `IpcRunner`, because
  `ms_backend_ipc::textdetector::build_forward_request` is model-agnostic).

Notes:
No detection rule lives here (scale, tiling, stitching, postprocess are `ms_text_detect`); this
file only moves tiles and maps across the process boundary. Worker-thread only (blocking IPC).
*/

use ms_local_ai::backend_health::ai_backend_offline_error;
use image::RgbImage;
use ms_backend_ipc::textdetector::{
    ForwardEngine, ForwardMaps, ForwardWireError, build_forward_request, decode_forward_response, max_tiles_per_request,
};
use ms_backend_ipc::{self as backend_ipc, CallError};
use ms_config as config;
use ms_sysprobe::ai_models;
use ms_sysprobe::ai_models::external::{ExternalModelSpec, ExternalModelStatus, external_model_status};
use ms_sysprobe::ai_models::external_catalog::SURYA_DETECTION_2025_05_07;
use ms_sysprobe::ai_models::{EnsureCtx, ModelEnsureError};
use ms_text_detect::{EngineKind, ProbMap, ProbMapRunner, RunnerError, TileMaps};
use serde_json::Value;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use web_time::{Duration, Instant};

/// Per-call timeout for the framed backend. Model warmup on first use plus a batch of large
/// tiles on CPU can take a while.
const DETECTOR_BACKEND_CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// Internal cap on tiles per forward request (detector decision Q2: not user-configurable).
/// Bounds backend device memory per call; the frame budget may lower it further.
pub(super) const IPC_MAX_TILES_PER_REQUEST: usize = 4;

/// Share of `MAX_BLOB_BYTES` (percent) one forward batch may fill in either direction; the
/// 10 % margin keeps a full batch clear of the frame limit.
const IPC_FRAME_BUDGET_PERCENT: u8 = 90;

/// v2 readiness gate. A successful `shared_client()` performs the `hello` handshake, which fails
/// fast when the backend is not running; that failure becomes the unified "backend offline"
/// message.
///
/// # Errors
/// The localized offline message when no backend answers.
pub(super) fn ensure_v2_backend_ready() -> Result<(), String> {
    backend_ipc::shared_client()
        .map(|_| ())
        .map_err(|_| ai_backend_offline_error().to_string())
}

/// The forward-wire engine of a detector engine; `None` for `Classic`, which has no model.
pub(super) fn forward_engine(engine: EngineKind) -> Option<ForwardEngine> {
    match engine {
        EngineKind::Ctd => Some(ForwardEngine::Ctd),
        EngineKind::Paddle => Some(ForwardEngine::Paddle),
        EngineKind::Surya => Some(ForwardEngine::Surya),
        EngineKind::Classic => None,
    }
}

/// The download bundle of the Surya text detector: the pinned `text_detection` checkpoint
/// (shared with the Surya OCR bundle). Downloaded only explicitly, from the detector panel.
pub(crate) const SURYA_DETECTOR_SPECS: [&ExternalModelSpec; 1] = [&SURYA_DETECTION_2025_05_07];

/// The model directories one backend forward needs besides the tiles: only Surya has one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct BackendModelDirs {
    /// Installed Surya `text_detection` checkpoint (`surya_detection_dir` on the wire).
    pub(super) surya_detection: Option<PathBuf>,
}

/// Makes sure the model files the backend engine loads are present: CTD and Paddle files are
/// downloaded when missing (cancellable, reporting through `ctx`); the Surya checkpoint is
/// only CHECKED (its download is explicit, from the panel). Worker-thread only (disk and
/// network I/O).
///
/// # Errors
/// `Cancelled`, or `Failed` with the localized model-ensure / "not downloaded" message.
pub(super) fn ensure_backend_models(engine: ForwardEngine, ctx: &mut EnsureCtx<'_>) -> Result<BackendModelDirs, ModelEnsureError> {
    let models_root = config::models_dir();
    match engine {
        ForwardEngine::Ctd => ai_models::ensure_comic_text_detector_torch(&models_root, ctx).map(|_| BackendModelDirs::default()),
        ForwardEngine::Paddle => ai_models::ensure_paddle_ocr_detector(&models_root, ctx).map(|_| BackendModelDirs::default()),
        ForwardEngine::Surya => {
            surya_detection_dir_in(&config::side_models_dir()).map(|dir| BackendModelDirs { surya_detection: Some(dir) })
        }
    }
}

/// The installed Surya detection checkpoint under `side_models_root` (stat-only, never
/// downloads).
///
/// # Errors
/// `Failed` with the localized "not downloaded" message naming the download / resume button
/// the detector panel shows for the checkpoint's current state.
pub(super) fn surya_detection_dir_in(side_models_root: &Path) -> Result<PathBuf, ModelEnsureError> {
    let statuses: Vec<(&ExternalModelSpec, ExternalModelStatus)> =
        SURYA_DETECTOR_SPECS.iter().map(|spec| (*spec, external_model_status(side_models_root, spec))).collect();
    if let Some((spec, status)) = statuses.iter().find(|(_, status)| *status != ExternalModelStatus::Installed) {
        ms_log::runtime_log::log_info(format!(
            "[text-detector] external model '{}' is not installed ({status:?}); asking the user to download it",
            spec.id
        ));
        let button = crate::ocr_model_download::download_label_for_statuses(&statuses);
        return Err(ModelEnsureError::Failed(tf!("translation.text_detector.surya_model_not_downloaded_error", button = button)));
    }
    Ok(SURYA_DETECTION_2025_05_07.local_dir(side_models_root))
}

/// How many tiles of `tile_input` fit `IPC_FRAME_BUDGET_PERCENT` % of the frame limit in the
/// larger of the request and response directions (the margin keeps a full batch clear of the
/// limit). The rule itself is owned by `ms_backend_ipc::textdetector::max_tiles_per_request`.
///
/// # Errors
/// As `max_tiles_per_request`: a zero, misaligned or overflowing tile size.
pub(super) fn ipc_frame_budget_tiles(engine: ForwardEngine, tile_input: [u32; 2]) -> Result<usize, ForwardWireError> {
    max_tiles_per_request(engine, tile_input[0], tile_input[1], IPC_FRAME_BUDGET_PERCENT)
}

/// Tiles per forward request: `min(IPC_MAX_TILES_PER_REQUEST, ipc_frame_budget_tiles)`, never
/// below 1. A tile size the wire refuses is logged and yields 1, so the request builder reports
/// the precise error on the first call instead of the batch size hiding it.
pub(super) fn ipc_max_batch(engine: ForwardEngine, tile_input: [u32; 2]) -> NonZeroUsize {
    let budget = ipc_frame_budget_tiles(engine, tile_input).unwrap_or_else(|err| {
        ms_log::runtime_log::log_warn(format!("[text-detector] backend batch size for {tile_input:?} tiles: {err}; using 1"));
        1
    });
    NonZeroUsize::new(budget.min(IPC_MAX_TILES_PER_REQUEST)).unwrap_or(NonZeroUsize::MIN)
}

/// One blocking request/response exchange with the backend. The seam exists so the runner's
/// packing and response validation are testable without a backend process.
pub(super) trait DetectorTransport {
    /// Sends `method` with `header` fields and `blob`, returning the response header and blob.
    ///
    /// # Errors
    /// A user-facing (localized or backend-provided) message.
    fn call(&self, method: &str, header: Value, blob: &[u8]) -> Result<(Value, Vec<u8>), String>;
}

/// The production transport: the process-wide framed-IPC client.
pub(super) struct SharedClientTransport;

impl DetectorTransport for SharedClientTransport {
    /// Maps `CallError` to user-facing text: `Error` is the backend's own message, `Interrupted`
    /// the localized abort notice, `Transport` the client's connect/framing message; a missing
    /// client is the unified offline message.
    fn call(&self, method: &str, header: Value, blob: &[u8]) -> Result<(Value, Vec<u8>), String> {
        let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
        client.call(method, header, blob, DETECTOR_BACKEND_CALL_TIMEOUT).map_err(|err| match err {
            CallError::Error(msg) | CallError::Transport(msg) => msg,
            CallError::Interrupted(msg) => tf!("translation.text_detector.request_aborted_error", msg = msg),
        })
    }
}

/// `ProbMapRunner` that runs one engine's forward pass in the Python backend.
pub(super) struct IpcRunner<T> {
    engine: ForwardEngine,
    transport: T,
    /// Model directories merged into every request header (see [`forward_header`]).
    model_dirs: BackendModelDirs,
}

impl<T: DetectorTransport> IpcRunner<T> {
    /// A runner for `engine` over `transport`, sending `model_dirs` with every request.
    pub(super) fn new(engine: ForwardEngine, transport: T, model_dirs: BackendModelDirs) -> Self {
        Self { engine, transport, model_dirs }
    }
}

/// Adds the model directories to a `build_forward_request` header: `surya_detection_dir`
/// (protocol v12, required by `textdetector.surya.forward`) when resolved. Other engines get
/// none, so their headers are unchanged.
///
/// # Errors
/// A localized message when the directory is not valid UTF-8 (never sent lossily).
fn forward_header(mut header: Value, model_dirs: &BackendModelDirs) -> Result<Value, String> {
    if let Some(dir) = &model_dirs.surya_detection {
        let dir = dir.to_str().ok_or_else(|| tf!("translation.ocr.model_path_not_utf8_error", path = dir.display()))?;
        if let Value::Object(map) = &mut header {
            map.insert(backend_ipc::protocol::HEADER_SURYA_DETECTION_DIR.to_owned(), Value::String(dir.to_owned()));
        }
    }
    Ok(header)
}

impl<T: DetectorTransport> ProbMapRunner for IpcRunner<T> {
    fn max_batch(&self, tile_input: [u32; 2]) -> NonZeroUsize {
        ipc_max_batch(self.engine, tile_input)
    }

    /// Packs `tiles` into one request, sends it and validates the reply against it.
    ///
    /// # Errors
    /// The transport's message unchanged; a localized `forward_response_error` for a request the
    /// wire refuses or a reply that breaks the contract (also logged with the technical detail).
    fn forward(&mut self, tiles: &[RgbImage]) -> Result<Vec<TileMaps>, RunnerError> {
        let engine = self.engine;
        let [width, height] = tiles.first().map_or([0, 0], |tile| [tile.width(), tile.height()]);
        let raw: Vec<&[u8]> = tiles.iter().map(|tile| tile.as_raw().as_slice()).collect();
        let (header, blob) = build_forward_request(engine, width, height, &raw).map_err(|err| wire_error(engine, &err))?;
        let header = forward_header(header, &self.model_dirs).map_err(|message| RunnerError { message })?;
        let started = Instant::now();
        let (response_header, response_blob) = self
            .transport
            .call(engine.method(), header, &blob)
            .map_err(|message| {
                ms_log::runtime_log::log_error(format!(
                    "[text-detector] backend {} forward of {} tiles {width}x{height} failed: {message}",
                    engine.wire_name(),
                    tiles.len()
                ));
                RunnerError { message }
            })?;
        let maps = decode_forward_response(engine, tiles.len(), width, height, &response_header, response_blob)
            .map_err(|err| wire_error(engine, &err))?;
        ms_log::runtime_log::log_info(format!(
            "[text-detector] backend {} batch: {} tiles {width}x{height} in {} ms",
            engine.wire_name(),
            tiles.len(),
            started.elapsed().as_millis()
        ));
        forward_maps_to_tiles(&maps).map_err(|detail| response_error(engine, &detail))
    }
}

/// Splits a validated response into one `TileMaps` per tile, channels in wire order.
///
/// # Errors
/// A technical message when a map is missing or does not form a `ProbMap` (impossible for a
/// response `decode_forward_response` accepted; checked, never assumed).
fn forward_maps_to_tiles(maps: &ForwardMaps) -> Result<Vec<TileMaps>, String> {
    let [map_width, map_height] = maps.map_size();
    let channels = maps.engine().channel_count();
    (0..maps.tile_count())
        .map(|tile| {
            let maps = (0..channels)
                .map(|channel| {
                    let data = maps.map(tile, channel).ok_or_else(|| format!("tile {tile} channel {channel} is missing"))?;
                    ProbMap::new(map_width, map_height, data.to_vec()).map_err(|err| err.to_string())
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(TileMaps { maps })
        })
        .collect()
}

/// Logs a wire-contract failure and turns it into the localized runner error.
fn wire_error(engine: ForwardEngine, err: &ForwardWireError) -> RunnerError {
    response_error(engine, &err.to_string())
}

/// Logs `detail` and returns the localized "invalid backend exchange" runner error.
fn response_error(engine: ForwardEngine, detail: &str) -> RunnerError {
    ms_log::runtime_log::log_error(format!("[text-detector] backend {} forward exchange rejected: {detail}", engine.wire_name()));
    RunnerError { message: tf!("translation.text_detector.forward_response_error", engine = engine.wire_name(), err = detail) }
}
