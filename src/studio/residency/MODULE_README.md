# Module: src/studio/residency

## Purpose
The studio's source-page residency pipeline, owned by `StudioApp` as one `PageResidency` per
session: background decode of source pages and clean overlays, strictly ordered promotion of
decoded pages, the frame-budgeted tiled GPU upload, the follow-up page-cache loader, the page
geometry and resident textures lent to the drawing tab, and the memory-pressure eviction of what
it owns. It knows no tab type.

## Architecture
```text
PageResidency::spawn(project, cache_pages, memory)
  -> loader::spawn_loader_thread   one pool, jobs sorted (page idx, source before overlay),
                                   workers park beyond DECODE_AHEAD_WINDOW past the frontier
       LoaderEvent        -> poll_decoded: page_infos updated on arrival, decoded_pending_by_idx,
                             promote_decoded_pages_in_order -> decoded_queue -> budgeted upload
                             -> textures (PageTexture); frontier published to the workers
       OverlayLoaderEvent -> poll_decoded: CleanOverlaysModel::load_prepared_overlay on arrival
  -> poll_page_cache: once every page is uploaded and every overlay delivered (and the canvas
     `cache_pages` setting — read from its owner, CleanOverlaysModel::cache_pages_enabled — and the
     memory budget allow), loader::spawn_page_cache_loader_thread fills the shared page RGBA cache
     of CleanOverlaysModel
StudioApp glue (studio.rs): page window + pixel inspection -> trim_gpu; pressure tick + window ->
  evict_for_pressure (source GPU, then the overlay model caches), then the tabs' own caches
```
Inputs that depend on tabs (`active_source_page_window`, the pixel-inspection flag, tab GPU
caches) stay in `studio.rs` and are passed in as values. The canvas `cache_pages` setting is not a
tab fact: `spawn` takes the project's open-time value, and `poll_page_cache` reads the live one
from `CleanOverlaysModel` (Settings and every canvas write it there), never from a canvas copy.

## Files and submodules
- `mod.rs`: `PageResidency` (fields, `spawn`, `poll_decoded`, `poll_page_cache`, `lend_mut`,
  `page_center`, `loaded_pages_count`, `load_errors`, `background_loads_pending`,
  `overlays_loading`, `shutdown`), in-order promotion and the tiled upload; promotion tests over
  synthetic `LoaderEvent`s (no egui context).
- `loader.rs`: the two worker pools, their event types, the look-ahead window
  (`decode_idx_within_window`), worker count, initial page-cache seeding decision, the overlay
  "staged over committed" resolution (`loadable_clean_overlay`) and job lists from `ProjectData`.
- `decode.rs`: worker-side decode (`decode_page` tiles, `decode_overlay`, `decode_page_rgba`), the
  env opt-in ffmpeg GPU decode, tile row copy.
- `memory.rs`: `trim_gpu`, `poll_pressure` / `PressureTick`, `evict_for_pressure`, the source-page
  GPU snapshot and eviction (reports merged with `CacheEvictionReport::append`).

## Contracts and invariants
- Source pages are promoted strictly in page order; a failed page is skipped, never waited on.
  Overlays bypass promotion and are applied as they arrive.
- The GUI thread only polls channels (bounded per frame) and uploads prepared tiles under
  `UPLOAD_TILE_BUDGET_PER_FRAME` / `UPLOAD_BYTES_BUDGET_PER_FRAME`; decode never runs on it.
- Deadlock freedom of the look-ahead window rests on the job sort order and on the GUI draining
  both channels every frame (`spawn_loader_thread` comment). `DECODE_AHEAD_WINDOW` must be >= 1.
- `shutdown` must be called when the GUI stops draining (from `StudioApp::shutdown_blocking`,
  before the layer flush), or parked workers leak.
- `Finished` is sent by each pool only after every worker returned; a worker panic is logged by
  the joiner and does not suppress `Finished`.
- `page_infos` survive GPU eviction (layout stays stable); eviction drops GPU handles only, and
  every dropped texture is reconstructable from the kept tile bytes.
- The per-frame call order is decided by `StudioApp::ui`: `poll_decoded`, then the page manager's
  clean polling, then `poll_page_cache`; the trims and the pressure poll run after the active tab
  drew.

## Editing map
- Decode speed, tile size, ffmpeg path: `decode.rs`.
- Pool shape, look-ahead, worker count, overlay resolution: `loader.rs`.
- Promotion, upload budgets, what is lent to tabs: `mod.rs`.
- Trim and pressure policy: `memory.rs` (budgets and ordering themselves: crate `ms-memory`).
- Which pages the active tab needs, tab GPU caches: `active_source_page_window` /
  `evict_tab_gpu_caches` in `src/studio.rs`.
