# Module: src/tabs/cleaning

## Purpose
This directory implements the Cleaning tab. It provides the canvas-facing UI for editing
per-page clean overlays, quick text-mask cleanup, save/history controls, and the tool picker
backed by reusable cleaning tools. All of that UI lives in dock tabs; the tab owns no floating
surface of its own.

## Architecture
`CleaningTabState` owns a dedicated `CanvasView`, the active `CleaningTool`, cleaning UI state,
and optional shared models injected by `MangaApp`. The tab routes pointer, keyboard, wheel,
and overlay-window events into the active tool. Tools edit canvas overlay scratch state and
commit through `CanvasView`, which synchronizes committed pages into `CleanOverlaysModel`
and its diff-based undo/redo history.

Text mask data flows from `TextMaskModel` when available, or from `text_detection/` files through
a background load job. The tab uploads mask tiles to egui textures and exposes them to canvas via
`CanvasHooks::draw_canvas_mask_overlay_on_page`. Those mask textures are a reconstructable display
cache with memory snapshots and eviction; the underlying mask data stays in `TextMaskModel` or on
disk.

Quick text cleanup builds per-page jobs from source pages plus text masks, runs page processing in
workers, and applies prepared `ColorImage` patches into `CleanOverlaysModel` as results arrive.
Save operations collect overlay snapshots from the shared model and write `clean_layers/` in a
worker thread.

This tab HOSTS the shared panel dock (`src/widgets/panel_dock`), and every floating surface it has
is a dock tab. It declares FIVE: the canvas' own «Лента» (`canvas::CANVAS_RIBBON_TAB`, body
`CanvasView::draw_ribbon_tab_body`, declared through `canvas::declare_ribbon_tab` — the canvas' one
declaration of it) plus four of its own — «Клин» (`cleaning.clean`: layer visibility, clear/save,
the quick-clean toggle and the save status), «Инструменты клина» (`cleaning.tools`: the tool picker,
rows wrapping to the panel width), «Выбранный инструмент» (`cleaning.active_tool`:
`CleaningTool::draw_ui`) and «Быстрый клин найденного текста» (`cleaning.quick_clean`: the
quick-clean parameters, its two run buttons and its progress). Its default arrangement is
`cleaning_default_dock_layout` — five panels, handed to the dock both by
`app.rs::restore_panel_dock` and by `ensure_default_layout`. A tab body cannot mutate the tab: the
dock runs inside `CanvasView::draw`, so a body only raises a flag on `CleaningDockOut` and
`CleaningTabState::apply_dock_out` performs every mutation after that call returns, in the order the
three floating surfaces these tabs replaced performed theirs. The dock state is NOT owned here: it is
app-owned (`MangaApp::panel_dock`, one per studio window) and lent in for the frame through
`CleaningDrawParams::panel_dock`, which `tab.rs` passes on to `CanvasDrawParams`. The dock runs in
`CleaningHooks::draw_canvas_overlay_top_left` — inside `CanvasView::draw`, so a «Лента» edit still
lands before `publish_canvas_settings` — and it must run on EVERY frame this tab is active,
because the dock's detached OS windows are immediate viewports that only exist while
`PanelDock::end` shows them. The panels it drew are collected into `CleaningHooks::dock_panel_rects`
and folded into `panel_rects` after `canvas.draw` returns (the tab clears that list there), or the
active tool would paint under a panel. `PanelDockOutput::drawn_panels` reports MAIN-WINDOW panels
only, so a panel the user detached into a sub-window cannot enter that list — its rect is in that
window's own frame and would blank out this window's top-left corner.

Long-running AI, image processing, mask loading, and save work runs on worker threads.
The GUI thread polls job receivers and applies already prepared results.
AI-backed tools receive backend health/Torch availability from the tab, then run model checks and
backend requests inside tool worker paths. App-managed inpaint weights must be resolved through
`src/ai_models.rs` before calling Python backend endpoints.

## Files and submodules
- `tab.rs`: tab state, canvas orchestration, the dock tabs and their default arrangement, mask
  loading, save jobs, quick text-clean job orchestration, and history hotkeys.
- `autoclean.rs`: quick text-clean image engine. GUI-free core (`run_autoclean_engine`)
  clusters the text mask, then per cluster runs: `has_text_structure` gate -> two candidates
  (A = strokes via `fill_holes`+dilate, B = detector-box union / cluster bbox) ->
  `evolve_mask_to_homogeneous` on both in parallel (`rayon::join`) -> coverage/area selection
  -> universal `clip_fill_to_bubble_interior` -> conditional background-only padding ->
  `final_sanity_trim`. The thin `autoclean_page` wrapper is the only egui-touching part; it
  rasterizes the winning `RegionFill`s into the overlay patch. Includes synthetic pipeline and
  characterization tests. Detector boxes arrive from `tab.rs` already in page-pixel space.
- `watermark_chapter.rs`: GUI-free chapter-level watermark decomposition engine — the exact,
  AI-free counterpart of the neural watermark path. A semi-transparent mark composites as
  `I = c + s*B` (`c = alpha*W`, `s = 1 - alpha`), constant across every occurrence of one mark, so
  observing it over different backgrounds determines `c`/`s` and removal is the division
  `B = (I - c)/s`. Stages, all per `WatermarkKind` (a chapter may carry several distinct marks,
  and two of them may share their artwork pixel for pixel): `validate_calibration_sample` (ring
  flatness -> calibration target vs template-only) -> `estimate_model` (least squares over
  separated flat samples; Theil-Sen against per-pixel background estimates; otherwise the graded
  deposit-exact fit) -> `discover_anchors` (the anchor SET, coarse pyramid scan then full-res
  refinement) -> `find_occurrences` / `scan_page` / `scan_chapter` (anchor-band NCC, then the
  per-pixel-background gain test) -> `remove_occurrence` / `remove_occurrences_on_page`.
  `refit_with_refined_backgrounds` is the estimated-background refinement loop, with a fixed,
  named iteration count. Design and the measurements it rests on:
  `dev-docs/watermark_chapter_decomposition_plan.md`. Consumed by the «По главе (точное
  вычитание)» mode of `tools/watermark_removal.rs`; `mod.rs` keeps an `allow(dead_code)` for the
  refinement surface the tool deliberately does not use (see the comment there).
- `tools/`: cleaning tool trait, brush/region-edit bases, local fill tools, stamp tool, AI-backed
  inpaint tools, and the watermark tool that hosts the chapter-decomposition UI plus its on-disk
  watermark library, the library management window and the reference-crop intake that builds an
  entry from the mark supplied on two known uniform backgrounds. See `tools/MODULE_README.md`.
- `mod.rs`: module wiring and public re-export of `CleaningTabState`.

## Contracts and invariants
- The cleaning tab uses shared clean-overlay visibility from `CleanOverlaysModel`; typing
  tab visibility toggles must not change this state.
- Tool operations must not block the GUI thread. CPU-heavy or AI-backed work must use
  background jobs and report explicit errors.
- App-managed cleaning/inpaint model checks and downloads must stay inside tool worker paths
  and go through `ai_models.rs` before Python backend requests.
- Overlay edits must validate page index, dimensions, and region bounds before mutating
  shared state.
- Shared model locks must be short-lived and released before image processing or file I/O.
- Text-mask overlays are display state only until quick-clean applies explicit overlay patches.
- Watermark decomposition never emits a model it cannot justify, and `ModelConditioning` is a
  GRADED verdict rather than a binary one. With all calibration samples on one exactly known
  background level the deposit `D = B - I` is still measured exactly, so a model IS produced and
  removal at that level is exact; only the alpha scale is an assumption, and the verdict carries
  the levels, their spread and an `AlphaUncertainty` (percent plus the LSB cost, including on dark
  backgrounds) together with the sample that would collapse it. `estimate_model` refuses — no
  model, and `WatermarkKind::refit` drops any previous one — only when not even the deposit was
  measured. Its `c`/`s` are per pixel PER CHANNEL: per channel is mandatory for `c`, while alpha
  measured channel-neutral on both chapters and the graded fit deliberately ties the channels
  together. Removal is licensed only for occurrences the gain test verified: a correlation-only
  accept is refused, because subtracting a mark that is not there injects an inverse mark.
- Watermark KIND identity is `MarkSignature` (deposit chroma plus opacity gain), never the
  template's shape: a colour mark and its greyscale twin can be pixel-identical in shape and
  still need different `c`/`s`. A catalog must resolve a new sample with `find_matching_kind`,
  and the same rule governs matching an open chapter against the on-disk library — with the
  footprint required to agree on top of it, because `c`/`s` are per pixel.
- A mark's anchor is a SET of columns discovered from the data (`discover_anchors` ->
  `MarkTemplate::set_anchors`), not the one column the picked sample sat at, and anything that
  keys or persists a model must include `MarkTemplate::anchor_key`. The accept rule additionally
  requires the occurrence to sit within `ANCHOR_TOLERANCE_PX` of an anchor and to reach
  `FALSE_ACCEPT_GAIN_FLOOR`; no `DetectionParams` value can widen past either.
- Text-mask GPU cache eviction must not mutate `TextMaskModel`, loaded mask data, quick-clean jobs,
  or committed clean-overlay edits.
- Canvas zoom, drag-scroll, and context menus must respect active tool capture/blocking signals.
- `panel_rects` is a SAME-FRAME list, cleared once per frame after `canvas.draw`. Every floating
  surface that may swallow a tool click has to be in it, and they are all dock panels now, which is
  why their rects are carried out of the hook rather than pushed straight into the field. A rect is
  never pushed into it directly: it arrives through `PanelDockOutput::drawn_panels`, which answers
  for the MAIN window alone.
- The default dock layout is the DICTIONARY of this tab's dock tabs: a `TabId` missing from
  `cleaning_default_dock_layout` is dropped from the user's stored arrangement on every load, so
  adding a dock tab here means adding it to that builder too. It is this tab's OWN builder — every
  canvas program tab has one, there is no shared ribbon-only builder to fall back on — and
  it is registered under `AppTab::Cleaning.key()` in `app.rs::restore_panel_dock` as well; a builder
  wired in only one of the two places silently resets the stored arrangement.
- A dock tab body runs INSIDE `CanvasView::draw`. It edits the state its own widgets own — the
  active tool's UI mutates that tool, exactly as it did inside the tool window — but it may not
  perform, or invalidate the inputs of, anything the tab defers: the canvas' overlay edits, the job
  starters (`start_save_job`, `start_text_mask_load_job_if_needed`, `start_quick_text_clean_job`)
  and the tool switch (`activate_tool`) all need `&mut CleaningTabState` and would land
  mid-canvas-frame. Those are raised as flags on `CleaningDockOut` and run by `apply_dock_out`
  after the canvas draw returns, in the order the surfaces they came from ran them. The worker/job
  code itself never moves into a body.
- Anything a tab body READS is polled BEFORE `canvas.draw`, not after it: `CleaningHooks` snapshots
  the save state and the active tool index when it is built, so `poll_save_job` and
  `ensure_active_tool_available` run at the top of `CleaningTabState::draw`. Polling after the draw
  showed the previous frame's answer for one frame — a spinner outliving its save, a tool that had
  just become unavailable still drawn selected — with nothing requesting the correcting repaint.
- The tool buttons' captions are resolved at DRAW time from `CleaningTool::title()`, never cached in
  the tab state: a cached caption keeps the language the app started in.
- Every tab whose width is caption-driven («Инструменты клина», «Клин», «Быстрый клин найденного
  текста») derives its `min_size` — and the last two their `initial_size` too — from the captions
  they are about to draw, per frame and therefore per locale. The dock never re-measures a tab's
  WIDTH (it stores the width the panel ASKED for), so a hardcoded width is permanent, and one sized
  for Russian opens the French panel on a horizontal scrollbar.
- `quick_text_mask_panel_open` is the ONE source of truth for the quick-clean tab: it is the tab's
  `.visible(..)` and it gates the canvas' text-mask overlay, so it must never be forked into a
  second flag. The tab is DECLARED on every frame regardless — a hidden tab keeps its slot in the
  layout and only its panel is skipped, while an undeclared one would be re-seeded a fresh panel on
  the next open. Its only affordance is the «Быстрый клин найденного текста» button in «Клин»,
  which is drawn `selected` while the tab is open: the `egui::Window` it replaced had a title-bar
  ✕, and a dock tab has no close affordance by design (a tab is only ever MOVED).

## Editing map
- To change top-level cleaning UI, save behavior, history, or quick-clean orchestration,
  edit `tab.rs`.
- To change which dock tabs this program tab declares or how big they start, edit
  `CleaningHooks::draw_canvas_overlay_top_left` in `tab.rs`; where their panels sit by default is
  `cleaning_default_dock_layout` in the same file, and both places have to agree. The «Лента» tab's
  own content, sizes, title and declaration (`canvas::declare_ribbon_tab`) live in `src/canvas/`.
- To change what a dock tab SHOWS, edit `draw_clean_tab_body` / `draw_tools_tab_body` /
  `draw_active_tool_tab_body` / `draw_quick_clean_tab_body` in `tab.rs`; to change what a click
  there DOES, add a field to `CleaningDockOut` and apply it in `apply_dock_out`.
- To change how the tool buttons are laid out or how narrow the tool panel may get, edit
  `draw_tool_button_rows` (rows wrap automatically; a caption never wraps) and the pair
  `cleaning_tool_button_width` / `cleaning_tools_tab_min_width`.
- To change quick text-clean pixel classification, mask evolution (grow/shrink), candidate
  selection, bubble-interior clipping, or conditional padding, edit `autoclean.rs`; keep
  worker/job coordination, mask resize, and detector-box source->page scaling
  (`scale_blocks_source_to_page`) in `tab.rs`. The engine core must stay GUI-free; only the
  `autoclean_page` boundary and `paint_patch_from_mask` may touch egui.
- To change watermark decomposition — sample validation, the `c`/`s` fit, the conditioning
  verdict and its alpha uncertainty, mark identity, anchor discovery, detection thresholds or the
  removal/residual maths — edit `watermark_chapter.rs`. Its named constants carry their own
  rationale; change one only against the measurements in
  `dev-docs/watermark_chapter_decomposition_plan.md`, and note that the two chapters measured
  there disagree on several of them, so a constant is source-evidence, not a universal. The engine
  must stay GUI-free: the mark catalog, the region editor, the jobs, `CanvasView` patches, i18n
  and the watermark library belong to `tools/watermark_removal.rs`,
  `tools/watermark_library.rs`, `tools/watermark_entry.rs` and
  `tools/watermark_library_window.rs`. In particular, whether a set of samples separates the
  model is answered by `estimate_model`'s own verdict; no caller may re-derive it from a copied
  threshold.
- To change brush, stamp, inpaint, or fill behavior, edit the relevant file under `tools/`.
- To change text-mask loading or tiled mask drawing, start in `tab.rs` and check
  `TextMaskModel` contracts in `src/models/`.
- To change committed overlay mutation/history semantics, use `CanvasView` overlay APIs and
  `CleanOverlaysModel`; do not mutate shared overlay storage directly from tools.
