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
binary masks, optional sample masks, mask generation from a backend source, and worker-thread run
closures. The mask editor offers four generation sources — the text detectors ComicTextDetector,
PaddleOCR and Surya (through the translation module's typed helpers) and the watermark detector
(`watermark.detect`, streamed) — so every mask-inpaint tool gains watermark removal with a
user-editable mask without a tool of its own.

A tool whose network predicts its own mask has no mask to paint and therefore builds on
`RegionEditToolBase` ALONE (`watermark_removal.rs` is the first such tool). That base gives the
selection, the loader, the editor window, zoom/scroll and Apply; everything the mask base keeps in
its private `RegionInpaintEditorState` — the run channel, the undo stack, the result preview and
the Escape handling — is then owned by the tool itself.

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
  mask-inpaint editor, mask generation (text detectors + watermark detector) with its shared
  `RegionMaskGenerationState`, and region loader worker.
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
  `LamaModelSpec.file_name` is the persisted selection identity; `display_key` is an i18n catalog
  key resolved to a localized label via `LamaModelSpec::display_name()` at render time (the model
  name is display-only and is free to localize, `docs/i18n_exclusions.md` §A5).
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
- `watermark_removal.rs`: the standalone «Удаление водяных знаков» tool. Built on
  `RegionEditToolBase` alone — nothing here needs a painted mask — and it hosts THREE modes,
  picked in the editor window above the parameter sections:
  - `mask_only` (default, streams `watermark.detect`, draws the predicted mask over the region and
    leaves pixels untouched — Apply is then a visual no-op);
  - `clean` (streams `watermark.remove`, replaces the region with the network's reconstruction).
    EXPLICITLY experimental and says so in the UI: on manhwa line art it softens strokes and leaves
    residue, which is why the mask-first flow is the default
    (`dev-docs/watermark_removal_plan.md` §1.2, §7.4);
  - `chapter` — «По главе (точное вычитание)»: no backend, no Torch, no weights. The catalog of
    marks, the calibration samples, the chapter scan, the apply and the reports around
    `../watermark_chapter.rs`, plus the on-disk library in `watermark_library.rs`. Its whole UI
    lives inside the SAME region-editor window (a new floating surface would have to be a
    panel-dock tab and is not needed).
  `WatermarkMode` is the three-way user selection; `WatermarkNetworkMode` is the two-way one that
  reaches the backend, so "which IPC method does the chapter mode call" cannot be asked. The model
  catalog, the ✓/«скачать» hint, the status query, the progress bar and the `CallError` mapping are
  REUSED from `base.rs` (`pub(super)` items), never duplicated. Settings persist to
  `watermark_removal_settings.json` (see `config::watermark_removal_settings_path`) on background
  threads; the chapter parameters in that file are normalized by the ENGINE's own
  `DetectionParams`/`SampleParams`, so the file and the values used cannot disagree.
- `watermark_library.rs`: the reusable library of measured watermarks under
  `config::watermark_library_dir()` — one self-contained directory per entry (`entry.json`,
  `template.png`, `planes/c.png` + `planes/s.png` as 16-bit PNGs, `samples/NNN.png`), so an entry
  can be copied or shared as a folder. Pure I/O and serde: engine types never cross its boundary,
  `watermark_entry.rs` maps between them. `(source key, page width, anchor key, variant id)` is
  SEARCH METADATA inside an entry, not its storage key — one entry may legitimately serve several
  sources — and matching an open chapter to an entry goes through `MarkSignature` /
  `find_matching_kind` first. It also owns the INTERCHANGE boundary: `export_entry_zip` /
  `export_entry_dir` write exactly the members `entry.json` declares, and `import_entry` stages an
  incoming entry beside the library, runs `validate_entry_dir` on it, and only then renames it
  into place under a free id.
- `watermark_entry.rs`: the bridge between the engine and the library. It owns the literal wire
  tags of a verdict / fit method / alpha source and their inverse (`stored_calibration`,
  `conditioning_from_stored`), the REFERENCE-CROP INTAKE (`run_reference_intake`) and the
  auto-match ranking (`rank_library_candidates`, `candidate_improves`). GUI-free.
- `watermark_library_window.rs`: the library management window, opened from the tool. A
  tool-owned `egui::Window` — NOT a panel-dock tab, because nothing on it docks or persists a
  layout (`dev-docs/watermark_library_plan.md`; precedent: the font-properties window). Lists
  every entry with its preview, its verbatim name, its quality verdict, its calibration levels
  and its sources, and offers rename / delete / export / import / build-from-reference-crops /
  improve-with-another-level. Every one of those runs on a worker; the window polls a channel.
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
  The watermark source calls `watermark.detect` itself but obeys the same contract: the response
  blob is an L8 mask PNG at the region resolution, decoded through the shared
  `text_detector::parse_mask_alpha_from_blob`.
- Watermark model ids (`slbr`/`wdnet`/`splitnet`) are wire values and the persisted selection
  identity, so they stay literals; only the display label is an i18n key resolved at render time
  (same split as `LamaModelSpec`). The catalog lives ONCE, in `base.rs`; the mask source and the
  standalone tool both read it from there.
- `watermark.remove` answers with `clean_png ++ mask_png` in a single response blob. `image_len`
  and `mask_len` from the response header must be validated with STRICT equality against the blob
  length before slicing, so a truncated or padded frame is rejected instead of sliced into garbage.
- Every request parameter the watermark tool sends is passed through
  `WatermarkRemovalSettings::normalized()` first: a hand-edited settings file cannot push an
  out-of-range tile, overlap, threshold, dilation, mode or model id onto the network, nor an
  out-of-range anchor tolerance, background radius or ring width onto the chapter engine.
- Chapter mode owns no maths. Sample validation, the `c`/`s` fit, the conditioning verdict, mark
  identity, anchor discovery, detection and removal all belong to `../watermark_chapter.rs`; this
  layer decodes pages, runs jobs, applies patches, persists and reports.
- Chapter jobs decode ONE page at a time (a chapter is several strips of ~700x18000) and take a
  COPY of the catalog; the GUI keeps the previous copy to draw and holds the catalog UI read-only
  until the worker hands its version back. `ChapterCatalog::kinds` and `::marks` are INDEX-ALIGNED,
  and a mark's crops are kept in LOCKSTEP with its kind's calibration samples — the engine does not
  hand sample pixels back and the library needs them.
- Catalog identity is `WatermarkKind::id` and lives in exactly one place; `ChapterMark` does not
  duplicate it. A new selection offered as a mark is resolved with `find_matching_kind`, never by
  comparing templates.
- Chapter removal is applied per page through `CanvasView::replace_overlay_region_px` on the GUI
  thread. Occurrences only correlation vouched for, and marks with no model, are COUNTED as refused
  and reported; they are never subtracted, because subtracting a mark that is not there injects an
  inverse one.
- Honest reporting is a contract, not a wording preference
  (`dev-docs/watermark_chapter_decomposition_plan.md`, corrections): the UI says the IMPRINT is
  measured exactly and never that «c точен»; the stated ±% bounds the alpha SCALE only; the
  exact/clipped shares are labelled a quantization-and-clipping report, and model quality is
  reported by the detection gain and the t-statistic instead.
- Library entries store the user-visible name VERBATIM (no trim, no normalization) and only
  samples over an exactly measured flat background, so folding a new chapter into an entry can
  never overwrite a measurement with an estimate. The calibration crops are the reconstruction
  source — a loaded entry is refitted from them — and the plane PNGs are an inspection and
  interchange artifact.
- Every library write is ATOMIC (sibling temp + `write_all` + `sync_all` + close + rename +
  directory fsync, the `tabs/typing/panel/doc_store.rs` recipe, which is not reachable from here)
  and keeps two guards: a document of a NEWER schema is never overwritten, and a document changed
  since it was read is MERGED — the on-disk document is re-read at write time and its additive
  parts (creation time, source list, and every top-level field this build does not know) are
  carried forward. `rename_entry` is merge-only for exactly that reason.
- An entry's member paths are UNTRUSTED input once an entry can be imported: every relative path
  out of `entry.json` goes through `entry_member_path`, which refuses anything but plain
  components, and an imported archive's members are size-capped and counted.
- REFERENCE-CROP INTAKE never decides the spread question itself: it builds the kind, refits, and
  accepts only `ModelConditioning::Separable`. Two crops on one background — or on backgrounds
  too close together — are refused with the measured levels and the background
  `suggested_background()` names. Alignment correlates GRADIENT MAGNITUDE, because raw luma flips
  sign between a white-background crop and a black-background one.
- AUTO-MATCH is shape-independent (`MarkSignature`) and additionally requires the footprint to
  agree, because `c`/`s` are per pixel. `ChapterMark::pinned_entry` is the user's explicit
  override and survives a rescan; `adopted_entry` is what is in effect and is rebuilt by every
  scan, which is why a scan releases adopted entries before pass 1 and re-applies them in pass
  1.5. The chapter's own crops are PARKED, never dropped, so an override is reversible.
- Tool registration is FOUR steps, none optional: `mod.rs` export, the `use` list in `tab.rs`, the
  `CleaningTabState::default` vector, and the matching index group array (`BRUSH_*`,
  `MASK_REMOVAL_*`, `AREA_EDIT_TOOL_INDICES`). An index missing from the group arrays is registered
  but never drawn.
- Mask generation is gated by the availability flags the tab pushes into the base: every source
  except PaddleOCR requires Torch (`RegionMaskGenerationMethod::requires_torch`), and the whole
  section requires a reachable backend.
- Tool pointer capture and zoom/scroll blocking are part of the canvas contract. An open region
  editor must block canvas zoom and capture pointer input inside its window.

## Editing map
- To add a new cleaning tool, implement `CleaningTool`, export it from `mod.rs`, and register it in
  `CleaningTabState::default`.
- To change common brush radius, scratch preview, stroke commit, or dirty-tile behavior, edit
  `BrushToolBase` in `base.rs`.
- To change region selection, composited-region loading, editor zoom/scroll, or apply behavior,
  edit `RegionEditToolBase` in `base.rs`.
- To change mask editor controls, mask generation (sources, watermark model catalog, streaming
  progress), sample-mask handling, or worker-run lifecycle, edit `RegionMaskInpaintToolBase` in
  `base.rs`.
- To change direct paint behavior, edit `zamazka.rs`; to change alt-version or current-page
  stamping, edit `stamp.rs`.
- To change local fill/inpaint algorithms, edit `gradient.rs` or `texture_synthesis.rs`.
- To change the standalone watermark tool (modes, tiling/threshold parameters, mask preview, its
  settings file), edit `watermark_removal.rs`; the shared model catalog, status query and progress
  bar it reuses live in `base.rs`.
- To change the chapter mode's UI, jobs, reports or overlay patches, edit `watermark_removal.rs`
  (`ChapterState` and the `run_chapter_*` workers); to change the maths behind them, edit
  `../watermark_chapter.rs`; to change what a stored watermark holds on disk, or how it is
  exported/imported, edit `watermark_library.rs` and `config::watermark_library_dir`.
- To change reference-crop intake (background measurement, alignment, the refusal wording) or the
  auto-match ranking, edit `watermark_entry.rs`; to change the library screen itself, edit
  `watermark_library_window.rs`.
- To change Python backend IPC method names, request/response blob layout, model selection, unload
  behavior, or model ensure logic, edit the relevant AI tool file and keep `ai_models.rs` as the
  model boundary.
