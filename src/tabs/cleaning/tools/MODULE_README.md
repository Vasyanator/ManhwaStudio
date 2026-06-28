# Module: src/tabs/cleaning/tools

## Purpose
This directory contains the concrete tools used by the Cleaning tab and the shared bases that
connect tool input, preview state, region editing, mask editing, and final overlay commits.

## Architecture
`base.rs` defines the `CleaningTool` trait consumed by `CleaningTabState`. The tab owns tool
selection and dispatches canvas pointer events, wheel/key events, floating-window drawing, cursor
painting, and backend availability into the active tool.

Brush tools use `BrushToolBase` to draw into a local scratch overlay. Scratch previews are tiled
for large images and are committed to `CanvasView` only at stroke boundaries. Region tools use
`RegionEditToolBase` to select an overlay rectangle, load the composited source page plus current
clean overlay on a worker thread, show a detached region editor, and insert the accepted result
back into the page overlay. Mask-inpaint tools use `RegionMaskInpaintToolBase` to add editable
binary masks, optional sample masks, text-detector mask generation, and worker-thread run
closures.

AI-backed tools (`lama.rs`, `lama_mpe.rs`, `aot.rs`, `sdxl.rs`) send region and mask as raw PNG
bytes in the IPC request blob (no base64), ensure required app-managed models through `ai_models.rs`,
verify backend health, call the Python AI backend via `backend_ipc::shared_client()`, validate the
returned PNG size (from the response blob), and surface backend errors in the region editor status.
All backend transport goes through `crate::backend_ipc` (framed IPC over the AF_UNIX socket);
`sdxl.rs` uses `call_streaming` with a progress callback for native streaming progress/preview
frames, while the one-shot tools use `shared_client().call(...)`.

## Files and submodules
- `mod.rs`: module exports for the cleaning tab.
- `base.rs`: `CleaningTool`, stroke/cursor types, brush scratch pipeline, region editor pipeline,
  mask-inpaint editor, text-detector mask generation, and region loader worker.
- `zamazka.rs`: primary paint/erase/eyedropper/rectangle tool for direct clean-overlay edits.
- `stamp.rs`: copies pixels into clean overlays either from `project/alt_vers/<name>` or from the
  current page image/clean overlay using a Photoshop-like source point, with lazy background
  source-page loading where file decode is needed.
- `gradient.rs`: local mask fill using Lab scanline estimation and smoothing.
- `texture_synthesis.rs`: local inpaint through the `texture-synthesis` crate, with optional
  sample mask limiting the texture source area.
- `lama.rs`: LaMa V2 backend inpaint, fixed supported model catalog, model scan, model ensure, and
  `inpaint.lama_v2` IPC calls (image+mask as concatenated request blob, result PNG in response
  blob). Exposes `lama_model_catalog`, `default_lama_model_filename`, and
  `ensure_lama_model_for_external` so other tools (SDXL 4-channel prefill) can reuse the catalog.
- `sdxl.rs`: SDXL inpaint backend tool (IPC method `inpaint.sdxl`) with two channel modes.
  `nine_channel` uses a dedicated 9-channel inpaint model at full denoise; `four_channel` uses an
  ordinary SDXL checkpoint with a LaMa prefill (model chosen from `lama.rs`) and a moderate
  denoise. Region selection is forced to multiples of 8 (SDXL VAE). The `inpaint.sdxl` call is
  streamed via `call_streaming`: the tool receives `progress` frames (each carrying `step`/`total`
  plus an optional latent preview PNG blob), updates a shared `SdxlSharedProgress`, and the editor
  renders a step progress bar plus a live latent preview while it repaints during processing. All
  generation controls live in a collapsible "Параметры генерации (SDXL)" section (collapsed by
  default). Per-mode generation parameters (prompts, steps, cfg, denoise, seed, sampler, mask
  blur/dilation, weights path) persist to a dedicated `sdxl_inpaint_settings.json` (see
  `config::sdxl_inpaint_settings_path`); loads/saves run on background threads, never
  `user_config.json`.
- `flux_fill.rs`: FLUX.1-Fill-dev tool (IPC methods `inpaint.flux_fill` streaming, `.unload`,
  `.status`) with two modes — `object_removal` (default) and `inpaint`. The GGUF quant (catalog from
  `.status`, with a ✓/«скачать» hint) and diffusers components are downloaded on demand by the
  backend into `side_models/`; the streamed `progress` frames carry a `phase` (`download` bytes /
  `generate` steps) + `label`, rendered as a single progress bar over a collapsible "Параметры
  (FLUX.1 Fill)" section (collapsed by default; default mode = object removal). Poisson seam
  matching is a toggle. Settings persist to `flux_fill_inpaint_settings.json` (see
  `config::flux_fill_inpaint_settings_path`) on background threads.
- `lama_mpe.rs`: LaMa MPE backend inpaint and `inpaint.lama_mpe` IPC calls.
- `aot.rs`: AOT backend inpaint and `inpaint.aot` IPC calls.
- `region_edit_test.rs`: development-only mask-inpaint pipeline test tool; it is not exported by
  `mod.rs`.

## Contracts and invariants
- Tools must mutate clean overlays through `CanvasView` APIs such as `replace_overlay_region*` and
  `commit_overlay_page_to_model`; they must not write `CleanOverlaysModel` storage directly.
- Region, mask, and output image dimensions must match before processing or applying a result.
  Empty images or empty masks should return the original region or a clear user-facing error.
- File decode, source-page loading, AI calls, model scans/downloads, and CPU-heavy inpaint must run
  off the GUI thread. GUI code may poll channels, update textures, and apply prepared patches.
- Shared model locks must be held only long enough to snapshot or apply data. Do not hold them
  while decoding images, running detectors, calling Python, or building textures.
- AI tools that require Torch must honor backend availability supplied by the tab and fail visibly
  when the backend or model is unavailable.
- Text-detector mask generation inside the region editor must use the typed detector helpers from
  the translation module and must treat returned masks as binary alpha data in region coordinates.
- Tool pointer capture and zoom/scroll blocking are part of the canvas contract. An open region
  editor must block canvas zoom and capture pointer input inside its window.

## Editing map
- To add a new cleaning tool, implement `CleaningTool`, export it from `mod.rs`, and register it in
  `CleaningTabState::default`.
- To change common brush radius, scratch preview, stroke commit, or dirty-tile behavior, edit
  `BrushToolBase` in `base.rs`.
- To change region selection, composited-region loading, editor zoom/scroll, or apply behavior,
  edit `RegionEditToolBase` in `base.rs`.
- To change mask editor controls, text-detector mask generation, sample-mask handling, or
  worker-run lifecycle, edit `RegionMaskInpaintToolBase` in `base.rs`.
- To change direct paint behavior, edit `zamazka.rs`; to change alt-version or current-page
  stamping, edit `stamp.rs`.
- To change local fill/inpaint algorithms, edit `gradient.rs` or `texture_synthesis.rs`.
- To change Python backend IPC method names, request/response blob layout, model selection, unload
  behavior, or model ensure logic, edit the relevant AI tool file and keep `ai_models.rs` as the
  model boundary.
