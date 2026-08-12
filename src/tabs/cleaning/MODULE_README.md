# Module: src/tabs/cleaning

## Purpose
This directory implements the Cleaning tab. It provides the canvas-facing UI for editing
per-page clean overlays, quick text-mask cleanup, save/history controls, and a floating
tool panel backed by reusable cleaning tools.

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

This tab HOSTS the shared panel dock (`src/widgets/panel_dock`). It declares exactly one tab —
the canvas' own «Лента» (`canvas::CANVAS_RIBBON_TAB`, body `CanvasView::draw_ribbon_tab_body`) —
and its default arrangement is `canvas::ribbon_only_dock_layout` — one panel at the dock area's left
edge, the same `fn` item «Перевод» registers, handed to the dock both by `app.rs::restore_panel_dock`
and by `ensure_default_layout`. The tab itself is declared through `canvas::declare_ribbon_tab`, the
canvas' one declaration of it. The dock state is NOT owned here: it is app-owned
(`MangaApp::panel_dock`, one per studio window) and lent in for the frame through
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
- `tab.rs`: tab state, canvas orchestration, floating panels, mask loading, save jobs,
  quick text-clean job orchestration, and history hotkeys.
- `autoclean.rs`: quick text-clean image engine. GUI-free core (`run_autoclean_engine`)
  clusters the text mask, then per cluster runs: `has_text_structure` gate -> two candidates
  (A = strokes via `fill_holes`+dilate, B = detector-box union / cluster bbox) ->
  `evolve_mask_to_homogeneous` on both in parallel (`rayon::join`) -> coverage/area selection
  -> universal `clip_fill_to_bubble_interior` -> conditional background-only padding ->
  `final_sanity_trim`. The thin `autoclean_page` wrapper is the only egui-touching part; it
  rasterizes the winning `RegionFill`s into the overlay patch. Includes synthetic pipeline and
  characterization tests. Detector boxes arrive from `tab.rs` already in page-pixel space.
- `tools/`: cleaning tool trait, brush/region-edit bases, local fill tools, stamp tool, and
  AI-backed inpaint tools. See `tools/MODULE_README.md`.
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
- Text-mask GPU cache eviction must not mutate `TextMaskModel`, loaded mask data, quick-clean jobs,
  or committed clean-overlay edits.
- Canvas zoom, drag-scroll, and context menus must respect active tool capture/blocking signals.
- `panel_rects` is a SAME-FRAME list, cleared once per frame after `canvas.draw`. Every floating
  surface that may swallow a tool click has to be in it — the dock's panels included, which is why
  their rects are carried out of the hook rather than pushed straight into the field.
- The default dock layout is the DICTIONARY of this tab's dock tabs: a `TabId` missing from
  `canvas::ribbon_only_dock_layout` is dropped from the user's stored arrangement on every load. A
  tab this program tab alone would declare therefore needs a default layout builder of its own,
  since that one is shared with «Перевод».

## Editing map
- To change top-level cleaning UI, save behavior, history, or quick-clean orchestration,
  edit `tab.rs`.
- To change which dock tabs this program tab declares or where its panels start, edit
  `CleaningHooks::draw_canvas_overlay_top_left` in `tab.rs`; the «Лента» tab's own content, sizes,
  title, declaration (`canvas::declare_ribbon_tab`) and shared default arrangement
  (`canvas::ribbon_only_dock_layout`) live in `src/canvas/`.
- To change quick text-clean pixel classification, mask evolution (grow/shrink), candidate
  selection, bubble-interior clipping, or conditional padding, edit `autoclean.rs`; keep
  worker/job coordination, mask resize, and detector-box source->page scaling
  (`scale_blocks_source_to_page`) in `tab.rs`. The engine core must stay GUI-free; only the
  `autoclean_page` boundary and `paint_patch_from_mask` may touch egui.
- To change brush, stamp, inpaint, or fill behavior, edit the relevant file under `tools/`.
- To change text-mask loading or tiled mask drawing, start in `tab.rs` and check
  `TextMaskModel` contracts in `src/models/`.
- To change committed overlay mutation/history semantics, use `CanvasView` overlay APIs and
  `CleanOverlaysModel`; do not mutate shared overlay storage directly from tools.
