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

A tool whose mask is not the mask-inpaint one builds on `RegionEditToolBase` ALONE. That base
gives the selection, the loader, the editor window, zoom/scroll and Apply; everything the mask base
keeps in its private `RegionInpaintEditorState` — the run channel, the undo stack, the result
preview and the Escape handling — is then owned by the tool itself. `watermark_removal.rs` does
this because it has NO mask to paint: the network predicts its own.

A size contract is NOT something `RegionEditToolBase` can guarantee: `snap_selection_end` clamps
to the page edge AFTER snapping to the multiple, and `build_composited_region_image` re-derives
the crop by ratio from the DECODED page, whose dimensions may differ from the overlay's. A tool
or engine with a hard size contract must therefore re-validate the loaded region's own size on
its run path, and both consumers do (`ai_editor::hand_region_to_engine`, `Flux2KleinEngine::start`).

The region LOADER is shared, not copied. `spawn_region_loader_thread` + `RegionLoadRequest` /
`RegionLoadResult` are `pub(super)` so any tool in this subtree can hand a page path, a source
rect and a clean-overlay chunk to one worker and get the composited region back off the GUI
thread; the decoded page is cached per worker and through `CleanOverlaysModel`. An owner MUST
send `None` and join the handle on drop, or the thread and its decoded page outlive the tool.

The region-editor WINDOW is not the only shape a region tool can take. `region_edit_v2/` is the
alternative: a selection FRAME that lives on the canvas for the whole editing session, with its
own handles, N mask layers, a result preview and a chrome of its own, driven from
`CleaningTool::draw_overlay_ui` instead of a floating window. It replaces the window flow for the
tools built on it; `RegionEditToolBase` and every tool on it stay exactly as they are. Its first
and so far only consumer is `ai_editor/`, which also carries the UI split the framework assumes:
a compact part in «Выбранный инструмент» (`draw_ui`) and a main part in its own dock panel
(`draw_main_panel`).

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
  `RegionMaskGenerationState`, and region loader worker. It also owns the overlay<->scene POINT
  mapping (`scene_pos_to_overlay_pos` / `overlay_pos_to_scene_pos`) and re-exports the dense-overlay
  colour solver `overlay_pixel_for_final_color` from `crate::tools::overlay_pixel`, so the whole
  subtree keeps reaching it as `base::overlay_pixel_for_final_color`.
- `zamazka.rs`: primary paint/erase/eyedropper/rectangle tool for direct clean-overlay edits.
- `stamp.rs`: copies pixels into clean overlays either from `project/alt_vers/<name>` or from the
  current page image/clean overlay using a Photoshop-like source point, with lazy background
  source-page loading where file decode is needed. Defaults to the current-page mode with
  «Исходник + клин» as the sampled layer. A background-loaded source page must match BOTH overlay
  dimensions; an expected dimension of `0` means the overlay size is not known yet, not "unchecked".
  In current-page mode the committed overlay is DENSE, not minimum-alpha: the solver is
  `base::overlay_pixel_for_final_color` (shared, not a stamp-local function), which solves the pixel
  that reproduces the desired final colour over the original page and raises its alpha to at least
  the brush dab coverage, so a 100 %-hardness dab commits a fully opaque patch.
  The minimum-alpha solution is exact only at 1:1 sampling — page and overlay are separate
  `TextureOptions::LINEAR` quads, so colour and alpha are filtered independently and a
  stencil-shaped overlay ghosts the page's own content back through it.
  Current-page mode shows two canvas markers: the fixed anchor beacon and a moving indicator of the
  point currently sampled, whose position is derived from the sampling mapping (`stamp_source_xy`)
  and therefore cannot disagree with the pixels being copied. The stroke origin is runtime stroke
  state, so between strokes the offset is zero and the moving marker rests on the beacon.
- `gradient.rs`: local mask fill using Lab scanline estimation and smoothing. Its screened-Poisson
  L-channel consolidation calls the project's SHARED red-black SOR kernel
  `crate::tools::red_black_sor_sweeps`; the other consumer is `crate::tools::patch::membrane`.
- `texture_synthesis.rs`: local inpaint through the `texture-synthesis` crate, with optional
  sample mask limiting the texture source area.
- `lama.rs`: LaMa V2 backend inpaint, fixed supported model catalog, model scan, model ensure, and
  `inpaint.lama_v2` IPC calls (image+mask as concatenated request blob, result PNG in response
  blob). Exposes `lama_model_catalog`, `default_lama_model_filename`, and
  `ensure_lama_model_for_external` so other tools (SDXL 4-channel prefill) can reuse the catalog.
  `LamaModelSpec.file_name` is the persisted selection identity; `display_key` is an i18n catalog
  key resolved to a localized label via `LamaModelSpec::display_name()` at render time (the model
  name is display-only and is free to localize, `dev-docs/i18n_exclusions.md` §A5).
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
- `patch/`: this tab's HOST for the «Заплатка» tool — Photoshop's Patch Tool. A free-form or
  rectangular selection drawn straight onto the page canvas, dragged onto a clean source area; the
  copied pixels are colour-adapted to the destination's contour by a gradient-domain (Poisson)
  membrane and committed into the clean overlay as one undo step. The tool ITSELF — selection,
  gesture, ROI/refusal geometry, the membrane solve and the outline painting — lives in
  `crate::tools::patch`; what is here is the `CleaningTool` impl, the `PatchHost` impl (canvas
  geometry, the two region loads, the store step) and the tab registration. Built DIRECTLY on
  `CleaningTool` (a lasso has no radius, no hardness and no axis-aligned scratch rect, so
  `BrushToolBase` fits none of it), it rides the tab's ordinary stroke pipeline. Own
  `MODULE_README.md`.
- `region_edit_test.rs`: development-only mask-inpaint pipeline test tool; it is not exported by
  `mod.rs`.
- `region_edit_v2/`: the on-canvas region-editing FRAMEWORK (frame, mask layers, geometry,
  painting, input). It has its own `MODULE_README.md`; a tool built on it drives `RegionFrame`
  from `draw_overlay_ui` and must not duplicate the helpers `base.rs` already exposes.
- `ai_editor/`: the «ИИ-редактор области» tool — the framework's only consumer, and the only tool
  with a MAIN dock panel (`wants_main_panel`). It HOSTS the AI engines behind the `AiEngine` trait
  (`ai_editor/engine.rs`, `ai_editor/engines/`), which is where FLUX.2 klein lives; a model is an
  engine of this tool and not a `CleaningTool` of its own. Own `MODULE_README.md`, and a second one under
  `engines/`.

## Contracts and invariants
- Tools must mutate clean overlays through `CanvasView` APIs such as `replace_overlay_region*` and
  `commit_overlay_page_to_model`; they must not write `CleanOverlaysModel` storage directly.
- Region, mask, and output image dimensions must match before processing or applying a result.
  Empty images or empty masks should return the original region or a clear user-facing error.
  The one deliberate exception is the area editor's FLUX.2 klein engine, where an empty PAINTED
  mask is a working mode rather than missing input: it is turned into a solid all-`255` wire mask
  before the request is built, so what reaches the backend is never an empty mask either. The rule
  above is about the WIRE, and it still holds without exception there.
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
- `CleaningTool` carries three additive, defaulted methods for tools that place something on the
  canvas: `set_panel_rects` (pushed by the tab each frame after `canvas.draw` and before
  `draw_overlay_ui`, so a tool can cut the dock panels out of the viewport), `wants_main_panel`
  and `draw_main_panel`. `draw_main_panel` runs inside `CanvasView::draw` and therefore may
  mutate only the tool itself — a button there raises a flag consumed by the next
  `draw_overlay_ui`, the `CleaningDockOut` rule at tool scope.
- `capture_overlay_chunk`, `extract_overlay_chunk`, `overlay_rect_to_scene_rect`,
  `scene_pointer_to_image_px`, `scene_pos_to_source_xy`, `scene_pos_to_overlay_pos`,
  `overlay_pos_to_scene_pos` and `overlay_pixel_for_final_color` are reachable as `pub(super)` items
  of `base.rs` (the last one re-exported from `crate::tools::overlay_pixel`): the whole `tools`
  subtree reuses them, and a copy of any of them in a tool is a defect.
  `scene_pos_to_overlay_pos` / `overlay_pos_to_scene_pos` are the FRACTIONAL point pair and exact
  inverses of each other; `scene_pos_to_source_xy` is the rounding, non-`Option` variant that takes
  an explicit page rect and source size instead of resolving them from the canvas.
- An on-canvas tool must NOT return `true` from `block_canvas_zoom()`: that flag also disables
  the clean-overlay undo shortcuts (`tab.rs`), which is acceptable for a modal editor window but
  not for a surface that lives on the canvas for a whole session. Block precisely instead
  (`captures_canvas_pointer` over the surface, drag-scroll only while a drag is live).
- A tool that drives its gesture through the TAB'S STROKE PIPELINE must also leave
  `captures_canvas_pointer()` `false`: the tab ORs that flag into `canvas_pointer_occluded` and
  then calls `finish_stroke()` and returns, so a capturing tool receives no stroke, key or cursor
  callback at all. `region_edit_v2` can capture only because it senses everything through its own
  `Area` and sets `wants_primary_stroke() = false`; `patch/` does the opposite and therefore pins
  BOTH flags to `false` with tests.
- A surface that must stay visible while the pointer is elsewhere is painted from
  `draw_overlay_ui` through `ctx.layer_painter(LayerId::new(Order::Middle, …))`, never from
  `draw_cursor`, which is pointer-gated. A bare layer painter registers no interactable `Area`, so
  it neither occludes canvas input nor trips the tab's z-order check.
- On-canvas geometry is stored in PAGE pixels and re-projected every frame. A stored screen
  rectangle drifts the moment the canvas scrolls or zooms — the same rule `region_edit_v2` states
  for its frame, and the reason `patch/` keeps its polygon in page pixels.
- Gradient-domain / Laplace maths belongs to ONE kernel: `crate::tools::red_black_sor_sweeps`.
  A tool wanting a harmonic or screened-Poisson solve builds `lam`/`denom`/`u0` around it
  (`lam = 0` inside the region, a large `lam` to pin known values) and pads its region by at least
  one pixel, because the kernel never writes its border. A second SOR implementation is a defect.
- Polygon/lasso rasterization belongs to `crate::tools::fill_polygon_spans`, shared with the
  PS-editor's selection; its sampling rule (scanline centre, even-odd, inclusive clamped span ends)
  is a contract, so a tool that also tests "is this point inside the selection" must use the same
  even-odd rule or the two answers disagree at the edges.

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
- To change the patch tool's selection gesture or its ROI/refusal geometry, edit
  `crate::tools::patch`; to change the seamless-cloning maths (pyramid, sweep schedule, Dirichlet
  weight, blend modes, feather ramp), edit `crate::tools::patch::membrane`. To change how a patch is
  STORED in the clean overlay, or how its region is loaded, edit `patch/mod.rs` here. Read
  `patch/MODULE_README.md` and `crate::tools::patch`'s `MODULE_README.md` first.
- To change local fill/inpaint algorithms, edit `gradient.rs` or `texture_synthesis.rs`.
- To change the standalone watermark tool (modes, tiling/threshold parameters, mask preview, its
  settings file), edit `watermark_removal.rs`; the shared model catalog, status query and progress
  bar it reuses live in `base.rs`.
- To change FLUX.2 klein, edit `ai_editor/engines/flux2_klein/` — it is an engine of the
  «ИИ-редактор области» tool, not a tool; see `ai_editor/engines/MODULE_README.md`. The wire names
  of its methods live in `backend_ipc::protocol`.
- To change the chapter mode's UI, jobs, reports or overlay patches, edit `watermark_removal.rs`
  (`ChapterState` and the `run_chapter_*` workers); to change the maths behind them, edit
  `../watermark_chapter.rs`; to change what a stored watermark holds on disk, or how it is
  exported/imported, edit `watermark_library.rs` and `config::watermark_library_dir`.
- To change reference-crop intake (background measurement, alignment, the refusal wording) or the
  auto-match ranking, edit `watermark_entry.rs`; to change the library screen itself, edit
  `watermark_library_window.rs`.
- To change the on-canvas region frame (its geometry, lock rules, painting or input), edit
  `region_edit_v2/` and read its `MODULE_README.md` first. To change what the area editor DOES
  with that frame — its engine picker, its run path, its apply check or either of its panels —
  edit `ai_editor/`; where its main panel sits by default is `cleaning_default_dock_layout` in
  `../tab.rs`. Which ENGINES it offers, and what each of them requires of the frame, is
  `ai_editor/engines/`.
- To change Python backend IPC method names, request/response blob layout, model selection, unload
  behavior, or model ensure logic, edit the relevant AI tool file and keep `ai_models.rs` as the
  model boundary.
