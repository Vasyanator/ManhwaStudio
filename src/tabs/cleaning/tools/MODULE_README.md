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
preview and the Escape handling — is then owned by the tool itself. Two tools do this, for opposite
reasons: `watermark_removal.rs` has NO mask to paint (the network predicts its own), while
`flux2_klein.rs` paints a mask with the INVERSE meaning — the mask base's mask says "remove what is
under it", and FLUX.2 klein's says "you MAY change what is under it", so reusing that base would
have inverted the contract rather than shared it.

`RegionEditToolBase` also carries three optional selection LIMITS, set by additive builders on top
of `new(window_id, selection_multiple)` and left off for every tool that does not ask for them:
`with_min_selection(px)`, `with_max_selection_area(px2)` and `with_max_aspect_ratio(ratio)`. They
are checked in `end_selection`, i.e. on release rather than during the drag, so the rubber band
stays visible and a refused selection is explained through `load_error` (drawn in red by
`draw_ui_hint`) instead of silently failing to appear. They are NOT a guarantee about the loaded
region: `snap_selection_end` clamps to the page edge AFTER snapping to the multiple, and
`build_composited_region_image` re-derives the crop by ratio from the decoded page — a tool with a
hard size contract must therefore re-validate `editor.image.size` on its own run path.

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
- `flux2_klein.rs`: the «Редактирование области (FLUX.2 klein)» tool (IPC methods
  `inpaint.flux2_klein` streaming, `.status`, `.estimate`, `.unload`, and the six
  `.prompt_cache.*` methods below). The user selects a region,
  PAINTS the area the model is allowed to change (brush/eraser with an adjustable radius, plus
  clear-all and fill-all), writes a prompt and gets that area regenerated; everything outside the
  painted mask must survive untouched, which is why it is built on `RegionEditToolBase` and not on
  the mask base. A checkbox in the mask section (`whole_region`) switches the tool to editing the
  WHOLE selected region instead — see the contract below. Selection contract: multiple of 16, shortest side >= 128 px, area <= 1 MP, aspect
  not steeper than 8:1 — declared through the three base limit builders AND re-checked against the
  actual `editor.image.size` before a run. The prompt field is doubled: an optional user-language
  field plus a Google/Yandex/DeepL picker and a «↓» button fill in the ENGLISH field that is the
  only one sent, reusing the translation tab's own `translate_texts_via_translator` on a worker
  thread. That English field is never empty: `FLUX2_DEFAULT_PROMPT` is substituted both for a new
  settings file and for one whose prompt is missing or blank, because an empty prompt blocks the
  run gate. Under it sits the PROMPT CACHE block: `.status` answers `prompt_cached` for the prompt
  it was asked about (an optional field — an absent one reads as "not known", never as "not
  cached"), and the line above the buttons is green/amber/neutral accordingly, shown only while
  that answer still describes the prompt in the field. «Кэшировать» runs the streaming
  `.prompt_cache.build` on the SAME progress bar as a generation (so neither can start while the
  other runs — reading the ~16 GB Qwen3 encoder takes ~106 s, against ~6 s for a cached prompt).
  The saved caches form a LIBRARY that lives backend-side (`prompt_cache/`, one folder per encoder
  family): `.prompt_cache.list` fills a `WheelComboBox` of named entries, `.save`/`.load` take a
  NAME (the name is typed in an inline field beside the button, following the watermark library and
  the typing presets rather than a modal of its own), and `.export`/`.import` carry one entry
  through a `.msprompt` file with the same non-blocking picker the model paths use. An imported
  file of a foreign family is stored under that family and reported as such; the tool shows a
  warning, because such an entry never appears in this family's list and the backend refuses to
  load it. `.status` and `.prompt_cache.list` are re-armed through the same one-shot flags as the
  memory forecast, so editing the prompt cannot turn a keystroke into a request. GENERATION
  WITHOUT A LOCAL TEXT ENCODER is supported — see the contract below. Model paths (Qwen3 text encoder folder, transformer file or diffusers folder, VAE) are
  entered by hand or through a non-blocking native picker and persist to
  `flux2_klein_settings.json` (`config::flux2_klein_settings_path`). Memory placement is chosen
  through four BUILT-IN presets («Максимум скорости» / «Сбалансированный» / «Минимум RAM» /
  «Минимум VRAM»); «Пользовательский» is never selectable — it is what the picker reports when the
  SEVEN fields a preset owns match none of them (placement, `low_cpu_mem_usage`, VAE tiling/slicing,
  `unload_transformer_before_vae`, `unload_text_encoder_after_encode`, `text_encoder_fp8`). The last
  two are the text-encoder memory controls: the Qwen3 encoder is ~16 GB and is needed exactly once
  per generation, so every economical preset drops it right after the prompt is encoded, while
  `text_encoder_fp8` is `false` in EVERY preset — quantizing costs embedding quality and is the
  user's decision alone. There is no negative prompt and there must not be one: the checkpoint is
  distilled (4 steps, guidance 1.0). The RAM/VRAM forecast is COMPUTED BY THE BACKEND (`.estimate`,
  peak = max over PHASES: prompt encoding, denoise, VAE decode); this side only formats it and warns
  when `fits` is false. The `breakdown` peaks are looked up by name and each one is optional, so a
  backend that does not report a phase simply loses that line of the tooltip. `.status` and `.estimate` both carry the normalized `params`: the backend
  answers about the paths in the REQUEST, falling back to those of its last successful generation
  when they are absent, so a query without them reports "nothing is configured" for the paths the
  user has just entered. The single progress bar is shared by every run of the tool and is claimed
  by GENERATION, so a cancelled run cannot move or clear the bar of the run that replaced it;
  cancelling also stops the backend through `CallHandle::cancel` instead of only dropping the
  answer.
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
- FLUX.2 klein's `whole_region` is a WORKING MODE, not a memory profile: no `MemoryPreset` owns it,
  choosing a preset must not move it, and `MemoryPresetValues` must not gain a field for it. The
  request blob keeps its shape in this mode — a mask is still sent, a SOLID one (every byte `255`,
  built by `Flux2SessionState::mask_for_run`), because the backend refuses `whole_region = true`
  unless the mask really is uniform. The painted mask is never overwritten by the mode: the
  painting controls are hidden, the overlay is not drawn and pointer input over the preview is
  ignored, so clearing the checkbox restores the user's work byte for byte. Backend-side
  `mask_dilate_px` is ignored here (the slider is faded to say so) while `mask_feather_px` keeps
  working and softens the join between the region and the page.
- FLUX.2 klein RUNS WITHOUT A LOCAL TEXT ENCODER when the prompt is already cached: the denoise and
  the VAE decode never read the encoder, so a `.msprompt` carried to a machine that never downloaded
  the 16 GB Qwen3 is enough. The run gate (`flux2_run_block_reason`) therefore waives the
  `text_encoder_path` requirement on `prompt_cached == Some(true)` and ONLY on that — `None` is "not
  known" (or a backend too old to report the field, which could not generate without an encoder
  anyway). The transformer, the VAE, the tokenizer and the scheduler are never waived, and the
  backend keeps the final say (`_first_unavailable_reason`); this gate exists to explain a refusal
  before the click, never to duplicate the decision. Three optional `.status`/`.prompt_cache` fields
  carry the state, all parsed as three-state `Option<bool>` where an absent field means "not known"
  and never `false`: `.status.text_encoder_available` (an empty path and a path that does not exist
  are the same `false` — the second is what a settings file copied from another machine looks like),
  `.prompt_cache.load.encoder_verified` (whether the encoder fingerprint was compared or the file's
  metadata was taken on trust) and `.prompt_cache.list.text_encoder_available`. The UI consequences:
  an amber warning line beside the cache status (a warning, not an error — ready caches still work),
  «Кэшировать» and «Сохранить кэш» disabled with their own tooltip (they are the only two library
  operations that need the encoder, and the backend refuses both), «Загрузить»/«Экспорт»/«Импорт»
  untouched, and a ONE-OFF notice in the prompt-cache warning slot after a load whose
  `encoder_verified` was `false`. With no encoder there is no ACTIVE family, so `.prompt_cache.list`
  answers an EMPTY top-level `family` and lists every family at once; each entry then carries its
  own `family` and the combo shows it as `<family> / <name>`. That label is DISPLAY-ONLY — the wire
  identifies an entry by NAME alone, and a name present in two families is a backend error rather
  than an arbitrary choice.
- FLUX.2 klein's request blob is `region.png ++ mask.png` with the mask an L8 PNG of EXACTLY the
  region size; the response header's `image_len` is validated with STRICT equality against the blob
  length and the decoded PNG must be exactly the region size. The response also carries
  `oom_recovered` and an `applied` object of FIVE memory flags (`unload_transformer_before_vae`,
  `vae_tiling`, `vae_slicing`, `unload_text_encoder_after_encode`, `text_encoder_fp8`): when the
  backend recovers from an out-of-memory failure during the VAE decode it retries the decode with
  cheaper flags, and the tool writes those flags back into its settings (so the next run starts
  economical) and says so in the editor status line. A partial `applied` object — including one
  carrying only the three older flags — is ignored wholesale rather than half-applied.
- The two placement-derived settings flags (`unload_transformer_before_vae`,
  `unload_text_encoder_after_encode`) default from the PLACEMENT, which serde's per-field default
  cannot see. `settings_from_json` derives them for a settings file written before the field
  existed — `false` under `full_gpu`, `true` for every economical placement — and leaves a file that
  carries them untouched. Add any further placement-dependent flag there, not in a serde default.
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
- To change FLUX.2 klein (model paths, the painted edit-permission mask, the prompt translator, the
  prompt-cache block, the memory presets or the RAM/VRAM forecast), edit `flux2_klein.rs`; to change
  the selection size limits it declares, edit the three `with_*` builders on `RegionEditToolBase` in
  `base.rs`; the wire names of its methods live in `backend_ipc::protocol`.
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
