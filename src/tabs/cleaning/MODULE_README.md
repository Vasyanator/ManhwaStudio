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

## Editing map
- To change top-level cleaning UI, save behavior, history, or quick-clean orchestration,
  edit `tab.rs`.
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
