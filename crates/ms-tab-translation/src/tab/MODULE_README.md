# Module: crates/ms-tab-translation/src/tab

## Purpose
The Translation tab's state and orchestration: `TranslationTabState` (declared in the parent
`tab.rs`) and the behaviour that drives the tools, OCR, machine translation, the text detector and
the bubble footer metadata over the tab's `CanvasView` and the shared models. The program-tab
wrapper that the studio root holds is `host.rs` (`TranslationTab`); the panels it draws are in
`panels/`; the workers and controllers are `ocr.rs`, `machine_translation.rs`, `text_detector/`.

## Architecture
- `tab.rs` (parent) owns the shared runtime types (`OcrToast`, `OcrPendingBubbleInsert`,
  `TextDetectorOcrTask`, the detector line / mask edit states, mask texture tiles,
  `TextDetectionStorageEvent`, `TranslationSettingsSaveRequest`, ...), the constants, the
  `TranslationTabState` struct, its lifecycle (`new`, `Default`, `Drop` joining the settings saver)
  and the host-facing API (input frame hand-off, brush input, host setters, GPU cache eviction).
- Every child adds `impl TranslationTabState` blocks (and free helpers) for one concern. Children
  are descendants of `tab`, so they read the struct's private fields directly; a method or helper
  that another child (or a test module) calls is `pub(super)`, everything else stays private.
- The per-frame orchestrator is `CanvasHooks::draw_canvas_overlay_top_left` (`canvas_hooks.rs`):
  polling (OCR, detector, storage, MT, AI API, request preview, downloads), footer / settings
  flushes, the pending Composition rebuild, then the panel dock (`dock.rs`). While the tab is
  hidden, `background.rs` runs the SAME poll methods.

## Files and submodules
- `ocr_routing.rs`: per-engine OCR load states, backend health copy and probe, external model
  downloads (OCR + Surya detector), route availability through
  `ms_local_ai::local_ai::AiRouteKind::availability`, the torch requirement status.
- `characters.rs`: footer character names (roster + chapter names + built-ins, the
  `characters.json` watch), footer tracking and debounced patches, recent-character shortcuts,
  the image-bubble file pick.
- `ocr_flow.rs`: OCR controller events, the image-crop selection, advanced recognition and
  manual / quick OCR on a selection, the OCR request and original-text builders.
- `detection_storage.rs`: detector controller events and the `text_detection` disk storage
  (lazy load, Save, worker IO, directory policy); the file format is `text_detector/storage.rs`.
- `detection_mask.rs`: detector status and edit modes, the sync with the shared `TextMaskModel`,
  mask painting, mask / line overlays, detector geometry and the tiled mask textures.
- `detection_ocr.rs`: detected-block queries, detector runs (page / all), the sequential
  detector-block OCR batch (dispatch, delayed retry, finish, abort).
- `mt.rs`: MT and AI API events, MT scopes and starts, AI MT options, the request preview worker
  and window, image-bubble MT inputs.
- `canvas_hooks.rs`: `impl CanvasHooks for TranslationTabState`, the image / hint footer halves,
  canvas toasts, image-bubble file and crop helpers.
- `settings_io.rs`: title settings hydration (OCR, MT, composition, text detector), the coalescing
  saver thread and its `ms_docstore::update` write, settings key parsers.
- `tool_panels.rs`: the five tools (`TranslationTool`, `ToolToggles`), the toolbar, the per-tool
  dock bodies, restore / toggle / focus and persistence of the on-set.
- `dock.rs`: the tab's panel dock (`translation_default_dock_layout`, re-exported by `tab.rs` for
  `host.rs`; `draw_panel_dock` declaring all eight tabs; «Последние персонажи»).
- `canvas_input.rs` (+ `canvas_input/tests.rs`): canvas pointer drivers from the input frame —
  the claimed rect selections, the lines editor, the mask brush.
- `detector_region.rs`: an OCR selection's region detection, its merge into both detector stores,
  and the Save snapshot of the shown masks (`detection_pages_for_save`).
- `block_bubbles.rs`: detector block + recognized text -> bubble (block OCR, NeuroLate page texts).
- `neurolate.rs`: the ONE NeuroLate key panel step and `neurolate_params`.
- `route_inputs.rs` (desktop-only): the non-blocking refresh of the OCR / detector routing inputs.
- `background.rs`: the off-draw drain while the tab is hidden (`tab_hidden_last_frame`).
- `scrollbar_marks.rs`: translation-status scrollbar marks and their cross-frame cache.
- `browser_extension.rs` (+ `browser_extension/tests.rs`, `pub(crate)`): the MT panel's
  browser-extension block, bridge start and install workers.
- Tests: `tests.rs` (footer character data), `detection_storage_tests.rs` and
  `settings_persistence_tests.rs` (desktop-only), `ocr_model_settings_tests.rs`,
  `ocr_model_download_wiring_tests.rs`; the other children keep their tests inline.

## Contracts and invariants
- The GUI thread never blocks: storage IO, settings writes, OCR / detector / MT runs, the AI MT
  preview and route-input reads run on workers; the tab only polls.
- The title `settings.json` is written only through `ms_docstore::update`, touching the tab's four
  sections; a malformed file is never overwritten; retired detector params are stripped on write.
- `draw_panel_dock` must run on every frame the tab is active (`PanelDock::end` keeps detached
  viewports alive). `translation_default_dock_layout` is the dictionary stored tab keys resolve
  against; it is registered in the studio root and in `draw_panel_dock`.
- Detector overlays draw while `detector_overlay_visible()` (`detector_region.rs`): Detector tool
  on, or the selected OCR engine's "create mask" parameter on.
- `SessionCapabilities`: without `title_documents` the character roster is empty with no I/O;
  without `chapter_storage` there is no detector storage load and no Save.
- The footer character field is fed `footer_autocomplete_candidates` (the sentinel-free slice).
- Toast and detector status colours come only from `ms_theme::Severity`.
- cfg twins (`gate_over_ocr_route_inputs` native / wasm) stay together in `ocr_routing.rs`; the
  desktop-only items (`route_inputs`, `OCR_ROUTE_INPUTS_CACHE_TTL`) keep their wasm gating.

## Editing map
- OCR engine status, downloads, route gating: `ocr_routing.rs` (+ `route_inputs.rs`).
- Footer, character names, recent characters: `characters.rs`; the footer UI: `canvas_hooks.rs`
  (`build_bubble_footer`) and `panels/bubbles.rs`.
- Quick / advanced OCR on a selection and request building: `ocr_flow.rs` (window: `adv_rec.rs`).
- Detector results on disk: `detection_storage.rs`; mask / lines editing and drawing:
  `detection_mask.rs` + `canvas_input.rs`; detector runs and block OCR: `detection_ocr.rs`.
- Machine translation and the AI MT preview: `mt.rs`.
- Settings load / save and key parsing: `settings_io.rs`.
- Tools bar, tool bodies: `tool_panels.rs`; dock layout and «Последние персонажи»: `dock.rs`.
- A new shared runtime type or a struct field: `tab.rs`.
