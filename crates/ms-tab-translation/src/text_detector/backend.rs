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
- `forward_engine()` / `ensure_backend_models()`: engine mapping and app-managed model gate.

Notes:
No detection rule lives here (scale, tiling, stitching, postprocess are `ms_text_detect`); this
file only moves tiles and maps across the process boundary. Worker-thread only (blocking IPC).
*/

use crate::backend_health::ai_backend_offline_error;
use image::RgbImage;
use ms_backend_ipc::textdetector::{
    ForwardEngine, ForwardMaps, ForwardWireError, build_forward_request, decode_forward_response, max_tiles_per_request,
};
use ms_backend_ipc::{self as backend_ipc, CallError};
use ms_config as config;
use ms_sysprobe::ai_models;
use ms_text_detect::{EngineKind, ProbMap, ProbMapRunner, RunnerError, TileMaps};
use serde_json::Value;
use std::num::NonZeroUsize;
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

/// Makes sure the app-managed model files the backend engine loads are present (downloads them
/// when missing). Surya's weights are managed by its library, so it needs nothing here.
/// Worker-thread only (disk and network I/O).
///
/// # Errors
/// The localized model-ensure message.
pub(super) fn ensure_backend_models(engine: ForwardEngine) -> Result<(), String> {
    let models_root = config::models_dir();
    match engine {
        ForwardEngine::Ctd => ai_models::ensure_comic_text_detector_torch(&models_root).map(|_| ()),
        ForwardEngine::Paddle => ai_models::ensure_paddle_ocr_detector(&models_root).map(|_| ()),
        ForwardEngine::Surya => Ok(()),
    }
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
}

impl<T: DetectorTransport> IpcRunner<T> {
    /// A runner for `engine` over `transport`.
    pub(super) fn new(engine: ForwardEngine, transport: T) -> Self {
        Self { engine, transport }
    }
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
