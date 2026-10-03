# Module: crates/ms-tab-translation/src/text_detector

## Purpose
The Translation tab's text detector: the background controller/worker that turns page files into
`TextDetectorPageResult`s (block rects in source pixels + a 0/255 mask), the model-based detection
entry point with its two runners (native PaddleOCR, Python backend forward pass), the classic
local detector, and the region-image helpers the Cleaning tab reuses for its mask tools.

## Architecture
- `mod.rs` is the public surface: result/option/mode/event types, `TranslationTextDetectorController`
  and its `ms_thread` worker. `run_detect_batch` reads the PaddleOCR route ONCE, gates the backend
  (route-aware), ensures models, dispatches each page sequentially (Classic -> `classic.rs`, the
  model engines -> `pipeline::detect_page_file` with `TextDetectorRunMode::plan_inputs()`), then applies the Rust mask dilation to EVERY page
  result (`dilate_mask_alpha`, radius from the tab, clamped 0..=30).
- `pipeline.rs` is the one model-based entry point, `detect_image(engine, &RgbImage, &DetectParams,
  route)`: `ms_text_detect::plan_detection` -> `run_detection` with a runner. PaddleOCR on the native
  route runs `native::NativePaddleRunner` first; a native failure that the backend may not share
  (`backend_fallback_applies`: runner / runner-output errors) is logged and the WHOLE page is re-run
  on `backend::IpcRunner`. CTD and Surya always use the backend runner.
- Runners only run the model: `backend::IpcRunner` packs tiles with
  `ms_backend_ipc::textdetector::build_forward_request`, calls `textdetector.<engine>.forward`
  through a `DetectorTransport` (production: `shared_client()`), validates the reply with
  `decode_forward_response`; `native::NativePaddleRunner` calls `ms_native_runtime::paddle_det_forward`.
- `region.rs` converts a region `ColorImage` (unmultiplied colour, alpha dropped) and runs the same
  `detect_image`, then dilates the region-size mask in Rust.
- Domain rules are NOT owned here: scale/tiling plan, stitching, the CTD/Paddle/Surya postprocess,
  block sort/cap and 0/255 mask normalization are `ms_text_detect`; square dilation and Otsu are
  `ms_raster`; the wire contract is `ms_backend_ipc::textdetector`.

## Files and submodules
- `mod.rs`: types, controller, worker loop, batch driver, the pure backend-gate decision
  (`detector_mode_needs_backend(mode, paddle_route)`), model-ensure, `dilate_mask_alpha`; `pub use`
  re-exports of every path used outside the crate.
- `pipeline.rs`: `DetectorRoute` + `detector_native_route` (pure) + `paddle_route`, `detect_image`,
  `detect_page_file` (decode on the worker with `image::open(..).to_rgb8()`), `ctd_detect_params`,
  `backend_fallback_applies`, `detect_error_message` (DetectError -> localized text), the plan /
  outcome logs.
- `backend.rs`: `ensure_v2_backend_ready`, `DetectorTransport` / `SharedClientTransport`,
  `IpcRunner`, `ipc_max_batch` / `ipc_frame_budget_tiles`, `forward_engine`,
  `ensure_backend_models`.
- `native.rs` (desktop only): `current_detector_route` (disk read), `NativePaddleRunner`,
  `runner_error_from_native`.
- `classic.rs`: `detect_page_classic` (decode wrapper) over the pure `detect_classic_from_gray`
  (downscale to 1600, Otsu, (2, 1) dilation, connected components, nearest mask promotion).
- `region.rs`: `detect_{ai_ctd,paddle,surya}_mask_for_image`, `region_mask`, `color_image_to_rgb`.
- `tests.rs`: unit tests (routing, gate, batch sizing, IPC runner over a fake transport, fallback
  decision, error texts, region contract) and the Phase 1 characterization tests of `classic.rs`
  and the dilation adapter.

## Contracts and invariants
- Public paths are stable: `text_detector::{TextDetectorRect, TextDetectorPageResult, option types,
  detect_*_mask_for_image}` are used by `ms-tab-cleaning` (`mask_generation.rs`). Keep them as
  re-exports from `mod.rs` when moving code between submodules. Region helpers return the mask at
  the REGION size (`[0, 0]` for an empty region).
- `TextDetectorRect` IS `ms_text_detect::DetectRect` (re-export, not a copy).
- Every engine returns blocks finalized by `ms_text_detect` and a mask that is either empty
  (`[0, 0]`) or exactly `w * h` bytes of 0/255 at source size.
- Mask dilation has ONE owner, Rust (`dilate_mask_alpha` over `ms_raster::dilate_square`): the batch
  driver for pages (every mode), `region_mask` for regions (every engine). Neither the pipeline nor
  the backend dilates.
- Backend batches: at most `IPC_MAX_TILES_PER_REQUEST` (4) tiles, further limited to 90 % of
  `MAX_BLOB_BYTES` in the larger direction (the rule is
  `ms_backend_ipc::textdetector::max_tiles_per_request`; this crate only passes the margin), at
  least 1. The backend sends no detection parameters:
  only `{n, width, height}` and the tiles; the CTD options carry only the detection size (plan
  input) and the region dilation.
- `TextDetectorRunMode::plan_inputs()` is the ONE mode -> `(EngineKind, DetectParams)` mapping.
  The page worker dispatches through it and the detector panel's per-page notice
  (`panels/text_detector.rs`) plans with it, so the notice shows exactly the resize / tiling the
  run does. Never build plan inputs for a mode anywhere else.
- Native Paddle batches: `ms_native_runtime::paddle_det_max_batch` (4 tiles on the CPU provider,
  1 on any accelerator, to bound GPU memory). A native failure is retried on the backend only
  for runner and runner-output errors; plan, page-size and mask-limit errors are final (identical on both routes).
- Error texts: runner messages are already localized (backend message, offline message,
  `forward_response_error`, the native runtime's `Display`); everything else maps through
  `detect_error_message`. Logs carry the technical detail with the `[text-detector]` prefix.
- Everything here except the controller's channel calls runs on the worker or the Cleaning tool
  worker, never the GUI thread (file decode, IPC, native inference, model ensure).
- The `characterization_*` expected values in `tests.rs` were observed before the Phase 1
  refactor; never edit them to make a change pass.

## Editing map
- New engine or mode: `TextDetectorRunMode` + `plan_inputs` + dispatch in `mod.rs`, the panel's
  `TextDetectorPanelOptions::run_mode`, `EngineKind` mapping in `pipeline.rs` /
  `backend::forward_engine`, its plan and postprocess in `ms-text-detect`.
- Backend batch size or transport: `backend.rs`; the wire shape: `ms-backend-ipc` `textdetector.rs`.
- Native error mapping: `native.rs`; native batch size: `ms-native-runtime`
  (`paddle_det_max_batch`).
- Fallback policy, logs, user-facing error texts: `pipeline.rs`.
- Classic thresholds and component filters: `classic.rs`.
- Scale, tiling, stitching, postprocess, block order/cap, mask normalization:
  `crates/ms-text-detect/src/`.
