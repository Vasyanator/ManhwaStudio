/*
FILE OVERVIEW: crates/ms-tab-translation/src/text_detector/native.rs
Native (in-process ONNX Runtime) PaddleOCR text detection for the detector module.
Desktop only: the whole module is compiled out on wasm (`ms-native-runtime` / `ort` are not
part of the web build).

Key items:
- `current_detector_route()`: reads the route inputs off disk (worker thread only) and applies
  the pure `pipeline::detector_native_route` decision. Mirrored by `ocr::ocr_route` for OCR.
- `NativePaddleRunner`: `ms_text_detect::ProbMapRunner` over
  `ms_native_runtime::paddle_det_forward` (batched, already-planned tiles in, one quantized
  probability map per tile out; batch size from `paddle_det_max_batch`, provider-dependent).

Notes:
The runner only runs the model; planning, stitching, the DB postprocess and the glyph mask are
`ms_text_detect`. A native failure is reported as a `RunnerError` and the pipeline decides
whether the page is re-run on the backend (`pipeline::backend_fallback_applies`).
*/

use super::pipeline::{DetectorRoute, detector_native_route};
use image::RgbImage;
use ms_config as config;
use ms_native_runtime::{self as native_runtime, NativeRuntimeError};
use ms_onnx_runtime::OrtDownloadProgress;
use ms_text_detect::{ProbMapRunner, RunnerError, TileMaps};
use serde_json::Value;
use std::num::NonZeroUsize;
use web_time::Instant;

/// Reads the native runtime selection + SIGILL guard (scoped to the effective native provider)
/// fresh off disk and returns the detection route. Worker-thread only (disk I/O). Returns
/// [`DetectorRoute::Backend`] when the config cannot be read.
pub(super) fn current_detector_route() -> DetectorRoute {
    let cfg = config::load_raw_user_settings_for_startup().unwrap_or(Value::Null);
    let runtime = config::AiRuntime::from_user_settings(&cfg);
    let scope = native_runtime::native_load_scope_key();
    let decision = config::ort_load_decision(config::read_ort_load_guard(&cfg, &scope));
    detector_native_route(runtime, decision)
}

/// `ProbMapRunner` running the PaddleOCR detection model in-process.
pub(super) struct NativePaddleRunner;

impl ProbMapRunner for NativePaddleRunner {
    /// `ms_native_runtime::paddle_det_max_batch`: 4 tiles on the CPU provider, 1 on any
    /// accelerator (bounds GPU memory; an OOM would send the page to the backend). Runs on the
    /// detector worker; the provider selection is cached by `current_detector_route` before.
    fn max_batch(&self, _tile_input: [u32; 2]) -> NonZeroUsize {
        native_runtime::paddle_det_max_batch()
    }

    /// Runs the shared native detector on `tiles` (guarded load, serialized with OCR).
    ///
    /// # Errors
    /// [`runner_error_from_native`] of the runtime error (logged with its stable variant name).
    fn forward(&mut self, tiles: &[RgbImage]) -> Result<Vec<TileMaps>, RunnerError> {
        let mut progress = |_snapshot: OrtDownloadProgress| {};
        let started = Instant::now();
        let maps = native_runtime::paddle_det_forward(tiles, &mut progress).map_err(|err| {
            // Debug (stable English variant name) keeps logs grep-able regardless of the UI
            // language; the localized Display goes to the user.
            ms_log::runtime_log::log_error(format!("[text-detector] native Paddle forward of {} tiles failed: {err:?}", tiles.len()));
            runner_error_from_native(&err)
        })?;
        ms_log::runtime_log::log_info(format!(
            "[text-detector] native Paddle batch: {} tiles {:?} in {} ms",
            tiles.len(),
            tiles.first().map(RgbImage::dimensions),
            started.elapsed().as_millis()
        ));
        Ok(maps.into_iter().map(|map| TileMaps { maps: vec![map] }).collect())
    }
}

/// The user-facing runner error of a native runtime failure: its localized `Display`.
pub(super) fn runner_error_from_native(err: &NativeRuntimeError) -> RunnerError {
    RunnerError { message: err.to_string() }
}
