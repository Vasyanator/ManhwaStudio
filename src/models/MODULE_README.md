# Module: src/models

## Purpose
This directory contains shared runtime models used by multiple tabs and canvas instances.
The models own user-editable chapter state that must be synchronized between GUI views,
background workers, autosave, and export code.

## Architecture
Models in this directory are usually wrapped in `Arc<Mutex<_>>` by `MangaApp` and passed
to tabs through typed setter methods. GUI code should take short snapshots from these
models and release locks before doing rendering, image processing, file I/O, or callbacks.

`BubblesModel` owns the shared bubble list, a lookup index by bubble id, canvas settings
snapshots, and monotonic revisions. Runtime bubble writes are coalesced through a background saver
and go to the unsaved staging path; the main project file is updated only by explicit project save
flows.

`CleanOverlaysModel` keeps both `egui::ColorImage` for canvas/UI upload and
`image::RgbaImage` for disk/export when an overlay is materialized. Pages with no clean layer stay
virtual (`None`) until a tool edit needs pixels; fully transparent overlays loaded from disk may
also stay virtual because they are equivalent to absence for canvas/export behavior. The
`ColorImage` side uses egui's internal premultiplied color representation; the `RgbaImage` side is
straight-alpha RGBA and is the only format that should be written to PNG or used by export
composition. The model also owns the optional decoded source-page cache so tools can share heavy
page images across tabs. That cache is reconstructable and is bounded by explicit byte/item policy,
LRU order, and optional page-window pins.

`TextMaskModel` stores detector mask alpha planes by page index with source and mask dimensions.
Writers replace whole pages or use closure-based in-place edits; readers track the model revision
to refresh local mask caches.

## Files and submodules
- `bubbles_model.rs`: shared bubble list, revision tracking, canvas settings, and
  coalesced background saving.
- `clean_overlays_model.rs`: shared clean overlay images, undo/redo history, dirty
  tracking, autosave snapshots, and cached decoded page images.
- `text_mask_model.rs`: shared text detector masks keyed by page index.
- `mod.rs`: module declarations for the shared model layer.

## Contracts and invariants
- Do not hold model locks during long operations or disk I/O. Clone snapshots first.
- Model revisions and dirty sets are the synchronization contract with canvas/runtime
  subscribers; update them whenever visible shared state changes.
- Bubble ids are the stable identity for updates. Maintain the id index whenever the stored bubble
  list changes.
- Bubble autosave writes the latest snapshot to the unsaved staging path and must preserve
  explicit project-save semantics.
- RGBA image buffers must match `width * height * 4`; mask buffers must match
  `width * height`.
- PNG/export-facing clean overlay buffers must be straight-alpha RGBA. Convert from
  `Color32` with `to_srgba_unmultiplied()` before writing to `RgbaImage`.
- Undo/redo deltas for clean overlays operate on `Color32`'s internal representation,
  while the save/export cache must be updated with straight-alpha RGBA after each delta.
- Page cache eviction and population must not make canvas/export results depend on whether caching
  is enabled; it is a performance cache only.
- Decoded source-page cache entries are always clean and reconstructable from page files; memory
  pressure may evict them by LRU as long as page-window pins are respected.
- Dirty or materialized clean overlay CPU data is user-editable state, not a normal cache entry.
  Memory pressure APIs may report its estimated bytes but must not evict unsaved overlay data.

## Editing map
- To change bubble persistence or shared canvas settings, edit `bubbles_model.rs`.
- To change clean overlay painting storage, saving, autosave, or undo/redo, edit
  `clean_overlays_model.rs`.
- To change decoded source-page cache behavior for tools, edit `clean_overlays_model.rs`.
- To change detector mask storage, dimensions, allocation, or revisioning, edit
  `text_mask_model.rs`.
