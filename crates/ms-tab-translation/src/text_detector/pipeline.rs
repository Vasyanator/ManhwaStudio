/*
FILE OVERVIEW: crates/ms-tab-translation/src/text_detector/pipeline.rs
The one entry point of model-based text detection in the Translation tab (pages) and the Cleaning
tools (regions): plan with `ms_text_detect::plan_detection`, run the plan through a runner
(native Paddle or the Python backend), and turn the result into the tab's types and messages.

Key items:
- `DetectorRoute` / `detector_native_route()`: where a PaddleOCR forward runs (target-neutral,
  pure; the disk read lives in `native::current_detector_route`).
- `paddle_route()`: the current PaddleOCR route (always `Backend` on wasm).
- `detect_image()`: plan + run one in-memory RGB page; native Paddle first when routed there,
  re-running the whole page on the backend after a native failure that the backend may not
  share (`backend_fallback_applies`).
- `detect_page_file()`: decode a page file on the worker and run `detect_image`.
- `ctd_detect_params()`: the CTD options' share of `DetectParams`.
- `detect_error_message()`: `DetectError` -> localized text.

Notes:
Logs (`ms_log::runtime_log`, `[text-detector]` prefix): the plan (engine, source size, scale,
scaled size, grid, tiles), each runner batch (in the runners), and the outcome with timings.
No mask dilation here: the caller dilates (page batches in `mod.rs`, regions in `region.rs`).
Worker-thread only: decode, IPC and inference are blocking.
*/

use super::TextDetectorPageResult;
use super::backend::{IpcRunner, SharedClientTransport, ensure_backend_models, forward_engine};
use image::RgbImage;
use ms_text_detect::mask::MaskError;
use ms_text_detect::{
    DetectError, DetectParams, Detection, DetectionPlan, EngineKind, PlanError, plan_detection, run_detection,
};
use std::path::Path;
use web_time::Instant;

/// Where a PaddleOCR text-detection forward pass runs. CTD and Surya always run on the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetectorRoute {
    /// In-process ONNX Runtime (`ms_native_runtime`).
    #[cfg_attr(
        target_arch = "wasm32",
        expect(dead_code, reason = "the web build has no native runtime, so only desktop constructs this route")
    )]
    Native,
    /// The Python backend over framed IPC.
    Backend,
}

/// Pure routing decision for a native PaddleOCR detection attempt: [`DetectorRoute::Native`]
/// only when the user selected the native AI runtime AND the per-scope SIGILL load guard is not
/// `Suspect`. Mirrors `ocr::ocr_route` for detection. Desktop only (its sole caller,
/// `native::current_detector_route`, is).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn detector_native_route(runtime: ms_config::AiRuntime, guard: ms_config::OrtLoadDecision) -> DetectorRoute {
    if runtime == ms_config::AiRuntime::Native && guard == ms_config::OrtLoadDecision::Safe {
        DetectorRoute::Native
    } else {
        DetectorRoute::Backend
    }
}

/// The current PaddleOCR detection route. Desktop reads it off disk (worker thread only); the
/// web build has no native runtime and always uses the backend.
pub(super) fn paddle_route() -> DetectorRoute {
    #[cfg(not(target_arch = "wasm32"))]
    {
        super::native::current_detector_route()
    }
    #[cfg(target_arch = "wasm32")]
    {
        DetectorRoute::Backend
    }
}

/// The CTD options' contribution to the plan: the requested detection size. A negative value
/// (never produced by the panel) becomes 0, which the plan clamps to its minimum like any other
/// out-of-range request.
pub(super) fn ctd_detect_params(options: &super::TextDetectorAiCtdOptions) -> DetectParams {
    DetectParams { ctd_detect_size: u32::try_from(options.detect_size).unwrap_or(0) }
}

/// Whether the native forward is attempted for this engine and route.
pub(super) fn native_applies(engine: EngineKind, route: DetectorRoute) -> bool {
    engine == EngineKind::Paddle && route == DetectorRoute::Native
}

/// Whether a failed native detection is re-run on the backend.
///
/// Forward and runner-output failures are native-specific (dylib, session, device, a malformed
/// map), so the backend may succeed where native failed. Plan, page-size, unsupported-engine and
/// oversized-mask errors are decided by the shared plan and postprocess, identical on both
/// routes, so a re-run would only fail again.
pub(super) fn backend_fallback_applies(err: &DetectError) -> bool {
    match err {
        DetectError::Runner(_)
        | DetectError::MapCount { .. }
        | DetectError::ChannelCount { .. }
        | DetectError::MapShape { .. }
        | DetectError::Map(_) => true,
        DetectError::Plan(_) | DetectError::PageSize { .. } | DetectError::Unsupported { .. } | DetectError::Mask(_) => false,
    }
}

/// Product name of an engine for messages and logs (a proper name, never translated).
fn engine_label(engine: EngineKind) -> &'static str {
    match engine {
        EngineKind::Classic => "Classic",
        EngineKind::Ctd => "CTD",
        EngineKind::Paddle => "PaddleOCR",
        EngineKind::Surya => "Surya",
    }
}

/// The localized user-facing text of a detection failure. Runner errors already carry their
/// localized message; every technical variant is wrapped with the engine name.
pub(super) fn detect_error_message(engine: EngineKind, err: &DetectError) -> String {
    match err {
        DetectError::Runner(runner) => runner.message.clone(),
        DetectError::Plan(PlanError::EmptyImage { width, height }) => {
            tf!("translation.text_detector.page_empty_error", w = width, h = height)
        }
        DetectError::Plan(PlanError::TooLarge { width, height, max }) => {
            tf!("translation.text_detector.page_too_large_error", w = width, h = height, max = max)
        }
        DetectError::Mask(MaskError::TooLarge { width, height }) => {
            tf!("translation.text_detector.result_mask_too_large_error", w = width, h = height)
        }
        DetectError::Unsupported { .. }
        | DetectError::PageSize { .. }
        | DetectError::MapCount { .. }
        | DetectError::ChannelCount { .. }
        | DetectError::MapShape { .. }
        | DetectError::Map(_) => {
            tf!("translation.text_detector.pipeline_error", engine = engine_label(engine), err = err)
        }
    }
}

/// Logs the plan of one detection: what the panel notice also shows, plus the tile input.
fn log_plan(plan: &DetectionPlan) {
    let [source_w, source_h] = plan.source_size();
    let [scale_x, scale_y] = plan.scale();
    let [scaled_w, scaled_h] = plan.scaled_size();
    let [cols, rows] = plan.grid();
    let [tile_w, tile_h] = plan.tile_input();
    ms_log::runtime_log::log_info(format!(
        "[text-detector] plan {}: source {source_w}x{source_h}, scale {scale_x:.4}x{scale_y:.4}, scaled {scaled_w}x{scaled_h}, grid {cols}x{rows}, {} tiles of {tile_w}x{tile_h}",
        engine_label(plan.engine()),
        plan.tiles().len()
    ));
}

/// Logs a finished detection.
fn log_outcome(engine: EngineKind, route: &str, detection: &Detection, started: Instant) {
    ms_log::runtime_log::log_info(format!(
        "[text-detector] {} via {route}: {} blocks, {} tiles in {} batches, {} ms",
        engine_label(engine),
        detection.blocks.len(),
        detection.stats.tiles,
        detection.stats.batches,
        started.elapsed().as_millis()
    ));
}

/// Detects text on an in-memory RGB page with `engine`.
///
/// `route` is consulted only for PaddleOCR (CTD and Surya always use the backend). Under the
/// native route a failure for which [`backend_fallback_applies`] is logged and the WHOLE page is
/// re-run on the backend, so the user still gets a result when the backend is up.
/// Worker-thread only (blocking inference / IPC, model ensure).
///
/// # Errors
/// The localized text of a plan failure, an unsupported engine (`Classic` has no runner), a
/// missing model, a runner failure or a postprocess failure (all logged).
pub(super) fn detect_image(engine: EngineKind, page: &RgbImage, params: &DetectParams, route: DetectorRoute) -> Result<Detection, String> {
    let plan = plan_detection(engine, [page.width(), page.height()], params).map_err(|err| {
        ms_log::runtime_log::log_error(format!("[text-detector] {} plan failed: {err}", engine_label(engine)));
        detect_error_message(engine, &DetectError::Plan(err))
    })?;
    log_plan(&plan);

    if native_applies(engine, route)
        && let Some(done) = try_native(engine, &plan, page)
    {
        return done;
    }

    let Some(forward) = forward_engine(engine) else {
        let err = DetectError::Unsupported { engine };
        ms_log::runtime_log::log_error(format!("[text-detector] {err}"));
        return Err(detect_error_message(engine, &err));
    };
    ensure_backend_models(forward)?;
    let started = Instant::now();
    let mut runner = IpcRunner::new(forward, SharedClientTransport);
    match run_detection(&plan, page, &mut runner) {
        Ok(detection) => {
            log_outcome(engine, "backend", &detection, started);
            Ok(detection)
        }
        Err(err) => {
            ms_log::runtime_log::log_error(format!("[text-detector] {} via backend failed: {err:?}", engine_label(engine)));
            Err(detect_error_message(engine, &err))
        }
    }
}

/// Runs the plan on the native Paddle runner. `Some` is final (success, or a failure the
/// backend would share); `None` means "re-run on the backend" (the reason is logged).
#[cfg(not(target_arch = "wasm32"))]
fn try_native(engine: EngineKind, plan: &DetectionPlan, page: &RgbImage) -> Option<Result<Detection, String>> {
    let started = Instant::now();
    let mut runner = super::native::NativePaddleRunner;
    match run_detection(plan, page, &mut runner) {
        Ok(detection) => {
            log_outcome(engine, "native", &detection, started);
            Some(Ok(detection))
        }
        Err(err) if backend_fallback_applies(&err) => {
            ms_log::runtime_log::log_error(format!(
                "[text-detector] native {} detection failed, re-running the page on the backend: {err:?}",
                engine_label(engine)
            ));
            None
        }
        Err(err) => {
            ms_log::runtime_log::log_error(format!("[text-detector] native {} detection failed: {err:?}", engine_label(engine)));
            Some(Err(detect_error_message(engine, &err)))
        }
    }
}

/// The web build has no native runtime: every page goes to the backend.
#[cfg(target_arch = "wasm32")]
fn try_native(_engine: EngineKind, _plan: &DetectionPlan, _page: &RgbImage) -> Option<Result<Detection, String>> {
    None
}

/// Converts a pipeline result into the tab's per-page result (consumes it, no copy).
pub(super) fn detection_into_page_result(detection: Detection) -> TextDetectorPageResult {
    TextDetectorPageResult {
        source_size: detection.source_size,
        blocks: detection.blocks,
        mask_size: detection.mask.size,
        mask_alpha: detection.mask.alpha,
    }
}

/// Decodes the page file `path` to RGB (alpha dropped) on the calling worker and runs
/// [`detect_image`]. The returned mask is NOT dilated (the batch driver does it).
///
/// # Errors
/// The localized open / empty-image message, or [`detect_image`]'s error.
pub(super) fn detect_page_file(path: &Path, engine: EngineKind, params: &DetectParams, route: DetectorRoute) -> Result<TextDetectorPageResult, String> {
    let page = image::open(path)
        .map_err(|err| {
            ms_log::runtime_log::log_error(format!("[text-detector] cannot open page {}: {err}", path.display()));
            tf!("translation.text_detector.open_image_error", path = path.display(), err = err)
        })?
        .to_rgb8();
    if page.width() == 0 || page.height() == 0 {
        return Err(tf!("translation.text_detector.empty_image_error", path = path.display()));
    }
    ms_log::runtime_log::log_info(format!("[text-detector] {} page {}", engine_label(engine), path.display()));
    detect_image(engine, &page, params, route).map(detection_into_page_result)
}
