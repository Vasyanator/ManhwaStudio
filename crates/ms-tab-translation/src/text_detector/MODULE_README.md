# Module: crates/ms-tab-translation/src/text_detector

## Purpose
The Translation tab's text detector: the background controller/worker that turns page files into
`TextDetectorPageResult`s (block rects in source pixels + a 0/255 mask), the model-based detection
entry point with its two runners (native PaddleOCR, Python backend forward pass), the classic
local detector, and the region-image helpers the Cleaning tab reuses for its mask tools.

## Architecture
- `mod.rs` is the public surface: result/option/mode/event types, `TranslationTextDetectorController`
  and its `ms_thread` worker. `run_detect_batch` reads the PaddleOCR route ONCE, gates the backend
  (route-aware), ensures models (NeuroLate: reads its API key once), dispatches each page
  sequentially (Classic -> `classic.rs`, the model engines -> `pipeline::detect_page_file` with
  `TextDetectorRunMode::plan_inputs()`, NeuroLate -> `neurolate::detect_page`), then applies the
  Rust mask dilation to EVERY page result (`dilate_mask_alpha`, radius from the tab, clamped
  0..=30).
- `neurolate.rs` is the cloud detector: the page (or an OCR selection's crop) is encoded as PNG
  and sent in ONE paid `MASK` request (`ms_ai_api::neurolate::recognize`, never retried; a
  whole page is sent with `kind=PAGE`, so a small page is never read as one block, while a
  selection crop leaves `kind` to the server's size default); the
  answer is converted by the one owner `detection_from_image` (polygons with holes ->
  `ms_text_detect::rasterize_polygons`, blocks -> `finalize_blocks`), wrapped as a page result
  (`page_result_from_image`) or a region (`region_from_image`, also used by the NeuroLate OCR
  engine for its own answer). With "recognize text right away" (`NeurolateDetect::with_text`) a
  page is sent in `MASK_AND_TEXT` instead, and `PageDetected::texts` carries each block's raw text
  aligned with the finalized blocks (`page_texts_from_image` over `finalize_paired`, the
  `finalize_blocks` order with payloads; `usable_block` / `block_text` decide which blocks and
  texts count, for the OCR engine too); the tab turns them into bubbles (`tab/block_bubbles.rs`).
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
- `region_detect.rs` is an OCR selection's region detection on the controller's worker
  (`start_region_detection` -> `WorkerCommand::DetectRegion` -> `RegionDetected` / `RegionFailed`):
  the batch's backend gate and model ensure for the engine's mode, then decode, the OCR crop rule
  (`crate::page_crop`), `detect_image` on the crop, Rust dilation, blocks offset to page pixels.
  The tab merges the answer with `merge_region_into_page` ("replace inside the selection").
- Domain rules are NOT owned here: scale/tiling plan, stitching, the CTD/Paddle/Surya postprocess,
  block sort/cap and 0/255 mask normalization are `ms_text_detect`; square dilation and Otsu are
  `ms_raster`; the wire contract is `ms_backend_ipc::textdetector`.

## Files and submodules
- `mod.rs`: types, controller, worker loop, batch driver, the pure backend-gate decision
  (`detector_mode_needs_backend(mode, paddle_route)`), model-ensure, `dilate_mask_alpha`; `pub use`
  re-exports of every path used outside the crate.
- `pipeline.rs`: `DetectFailure`, `DetectorRoute` + `detector_native_route` (pure) + `paddle_route`, `detect_image`,
  `detect_page_file` (decode on the worker with `image::open(..).to_rgb8()`), `ctd_detect_params`,
  `backend_fallback_applies`, `detect_error_message` (DetectError -> localized text), the plan /
  outcome logs.
- `backend.rs`: `ensure_v2_backend_ready`, `DetectorTransport` / `SharedClientTransport`,
  `IpcRunner` (merges `BackendModelDirs` into every forward header: `surya_detection_dir`, v12),
  `ipc_max_batch` / `ipc_frame_budget_tiles`, `forward_engine`, `ensure_backend_models` (CTD /
  Paddle files downloaded on demand; the Surya checkpoint only checked, `surya_detection_dir_in`)
  and `SURYA_DETECTOR_SPECS` (the Surya checkpoint's explicit-download bundle, re-exported by
  `mod.rs` for the panel and the tab). A Surya batch checks it before the first page, so a missing
  checkpoint fails the batch once with the "not downloaded, press <button>" error.
- `native.rs` (desktop only): `current_detector_route` (disk read), `NativePaddleRunner`,
  `runner_error_from_native`.
- `classic.rs`: `detect_page_classic` (decode wrapper) over the pure `detect_classic_from_gray`
  (downscale to 1600, Otsu, (2, 1) dilation, connected components, nearest mask promotion).
- `region.rs`: `detect_{ai_ctd,paddle,surya}_mask_for_image`, `region_mask`, `color_image_to_rgb`.
- `neurolate.rs`: `NeurolateKey` (redacted `Debug`, read once per batch / region),
  `detect_page`, `recognize_image`, `detection_from_image`, `page_result_from_image`,
  `region_from_image` (`pub(crate)` for the OCR engine), fixture-driven unit tests over the
  sanitized live answers in `crates/ms-ai-api/fixtures/neurolate/`.
- `region_detect.rs`: `RegionDetectEngine` (`selection_detects`: the one trigger rule —
  PaddleOCR / Surya / NeuroLate with that engine's own parameter, `ocr::DetectMaskFlags`;
  `for_ocr` maps it onto a request's `OcrRuntimeOptions`, and the `NeuroLate(OcrParams)` engine
  carries its resolved request parameters; `rides_on_ocr_answer` / `ocr_region_want`: a quick
  NeuroLate selection takes its region from the OCR's own `MASK_AND_TEXT` answer, converted by
  `neurolate::region_from_image`), `RegionDetectRequest`, `RegionDetection`,
  `run_region_detection` (worker), the pure `merge_region_into_page` / `empty_page_result`, and
  their unit tests.
- `storage.rs` (+ `storage/tests.rs`): the ONE owner of the per-chapter `text_detection` storage
  format — `{idx:05}_blocks.json` (`source_size`, `blocks`, `mask_size`, `mask_file`) +
  `{idx:05}_mask.png`, the reader `read_text_detection_page` and the writer
  `write_text_detection_page`. GUI-free, worker threads only.
- `tests.rs`: unit tests (routing, gate, batch sizing, IPC runner over a fake transport, fallback
  decision, error texts, region contract) and the Phase 1 characterization tests of `classic.rs`
  and the dilation adapter.

## Contracts and invariants
- Public paths are stable: `text_detector::{TextDetectorRect, TextDetectorPageResult, option types,
  detect_*_mask_for_image}` are used by `ms-tab-cleaning` (`mask_generation.rs`). Keep them as
  re-exports from `mod.rs` when moving code between submodules. Region helpers return the mask at
  the REGION size (`[0, 0]` for an empty region).
- `TextDetectorRect` IS `ms_text_detect::DetectRect` (re-export, not a copy).
- The `text_detection` storage format has one reader and one writer (`storage.rs`): the tab's
  load / Save (`tab/detection_storage.rs`, directory policy only) and Cleaning's stored-mask
  reads (`ms-tab-cleaning` `text_mask_storage.rs`, an adapter) both go through them. A page whose
  files exist but cannot be read is an error, never "no detection".
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
  run does. Never build plan inputs for a mode anywhere else. It is `None` only for `NeuroLate`
  (no local plan; the panel shows the paid-service notice); a model mode reaching dispatch
  without a plan, or NeuroLate without its key, is a logged `mode_dispatch_error` page failure,
  never a panic.
- NeuroLate: a cloud route (no backend gate, never `--no-ai`-gated). Only blocks with
  `OcrBlock::has_usable_mask` contribute (an EMPTY region's mask is its own rectangle); the
  answered image size must equal the sent size (polygons are image-absolute), and a per-image
  error is a page error. The batch's `cancel` flag is the request's `abandon` (returns at once;
  the detached request may still be charged); `stop` never reaches it. The key is never logged.
- Native Paddle batches: `ms_native_runtime::paddle_det_max_batch` (4 tiles on the CPU provider,
  1 on any accelerator, to bound GPU memory). A native failure is retried on the backend only
  for runner and runner-output errors; plan, page-size and mask-limit errors are final (identical on both routes).
- Error texts: runner messages are already localized (backend message, offline message,
  `forward_response_error`, the native runtime's `Display`); everything else maps through
  `detect_error_message`. Logs carry the technical detail with the `[text-detector]` prefix.
- Everything here except the controller's channel calls runs on the worker or the Cleaning tool
  worker, never the GUI thread (file decode, IPC, native inference, model ensure).
- Cancel and download progress: every model-based entry (`detect_image`, `detect_page_file`,
  the region helpers) takes the caller's `ai_models::EnsureCtx`; its flag stops a model download
  (native detector model through `NativePaddleRunner`, backend engine files through
  `ensure_backend_models`) and its sink receives the download events. A cancel is
  `DetectFailure::Cancelled`: never retried on the backend (a native runner failure while the
  flag is set is treated as the cancel), never a page failure; the batch ends with
  `DetectCancelled`. The page batch owns one flag per batch, set by the controller's drop and
  checked before every page.
- Stop after the current page (`request_stop`, the panel's Stop): a SEPARATE per-batch stop flag
  (`batch_stop`), set only while a page batch runs (`is_batch_running`; a region detection does
  not count) and checked only before each page, never inside a download or a forward pass. The
  page in flight finishes and its result is delivered, then the batch ends with
  `DetectCancelled`. Stop never sets the cancel flag: the native PaddleOCR forward observes that
  one mid-page, so reusing it would drop the current page.
- Region detection: one region at a time and only while the controller is idle (busy is shared
  with page batches; nothing is queued, the caller reports the refusal). Its crop is exactly the
  OCR crop of the same selection (`page_crop::uv_rect_to_pixel_rect`, one owner). The merge
  replaces the selection's rect only. Mask: one raster rule, `ms_models::text_mask_model::
  replace_mask_region`, applied to the tab's result here and to the model's current mask by
  `TextMaskModel::merge_region`. Blocks: decided only here (`region_contains_block_centre` drops
  the result's blocks whose centre is in the rect, the region's blocks are appended, then
  `ms_text_detect` finalizes them); the tab hands that finalized set (as integer boxes) to the
  model, which stores it as is, so both stores hold the same blocks. The answer carries the page
  size, so the GUI thread never reads the page to create a missing entry. Dilation stops at the
  selection border. The tab holds an answer back while the chapter's stored detection is
  loading or saving (`tab/detector_region.rs`).
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
- Which OCR selections also detect, the region worker and the region merge: `region_detect.rs`;
  the tab's trigger / apply: `../tab/detector_region.rs`.
- NeuroLate request / answer handling: `neurolate.rs`; its parameters: `../neurolate.rs`; the
  client and wire format: `crates/ms-ai-api/src/neurolate/`.
- Scale, tiling, stitching, postprocess, block order/cap, mask normalization:
  `crates/ms-text-detect/src/`.
