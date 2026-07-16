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
flows. The model owns the saver's `JoinHandle`, and the saver is quiescable: it takes a barrier
(hold/resume, reference-counted), a pause gate, and an explicit shutdown. Those are the only
mechanisms that make "the bytes are on disk" true — see the contracts below.

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
- `clean_assign.rs`: worker-thread filesystem API for discovering orphan clean images,
  checking attachment fit, decoding/resizing attachments, and moving committed files to trash.
- `clean_overlays_model.rs`: shared clean overlay images, undo/redo history, dirty
  tracking, autosave snapshots, and cached decoded page images.
- `text_mask_model.rs`: shared text detector masks keyed by page index.
- `mod.rs`: module declarations for the shared model layer.

## Contracts and invariants
- Every `clean_assign` public function that accesses images or files is synchronous and must run
  on a worker thread. Unreadable clean images remain visible to callers as diagnostic orphans.
- Do not hold model locks during long operations or disk I/O. Clone snapshots first.
- To read a single bubble (or its `extra` map) by id, use `BubblesModel::with_bubble` /
  `extra_of` instead of `snapshot()`; they look up via `bubble_index_by_id` and avoid cloning
  the whole list. The saver channel carries `BubblesSaverMessage`, whose snapshot variant holds an
  `Arc<Vec<Bubble>>`, so publishing a save shares the snapshot rather than deep-cloning it.
- **Saver quiescence — the durability contract.** Coalescing means an enqueued edit is not yet on
  disk; only these make it so, and each caller uses a different one deliberately:
  - `barrier_and_hold_blocking` — flush + HOLD, for save-to-project (taken before the merge, so the
    merge cannot copy a staging file the saver has not written yet, and the saver cannot re-create
    staging after the merge deleted it). Holds are **reference-counted**: each guard's `Resume`
    releases exactly one level and the held snapshot is written at zero. A boolean hold was broken
    by a second concurrent holder releasing someone else's. Shutdown during a hold waits for every
    holder, then persists the held snapshot — it must never drop it silently.
    Never call it on the GUI thread; the one exception is `shutdown_saver`, which uses it internally
    on the exit path where the drain is bounded and the process is ending anyway. Its "flushes
    everything enqueued before this call" guarantee is exact only for the FIRST holder: a barrier
    nested inside an active hold acks immediately while pre-barrier snapshots still sit in
    `held_snapshot`. Safe today (only `shutdown_saver` nests, and Shutdown waits + persists), but do
    not build a new caller on the flush semantic under concurrency without fixing that.
  - `pause_saver_for_page_op` — PERMANENT pause: waits for the in-flight write, then drops all later
    publications and makes shutdown drain-and-join **without writing**. Safe for page-ops only
    because they reload the project afterwards, and used by DISCARD because it must not write at all.
  - `resume_saver_after_failed_discard` — the ONLY resume. Exists solely because a failed discard
    hands a still-running app back to the user; do not generalize it into a `resume(scope)` (a
    post-remap resume would write obsolete page indices).
  - `shutdown_saver` — drain + join at exit.
- **The discard path must not flush.** `start_exit_cleanup` pauses the saver before deleting the
  staging dir. Otherwise a write landing after the delete re-creates `_unsaved/`
  (`write_bubbles_snapshot_to` does `create_dir_all`) and the next launch offers to restore exactly
  what the user discarded. Deletions stay eager; anti-resurrection never depends on a flush point.
- `mark_saved_to_project` probes staging EXISTENCE rather than clearing the dirty flag outright, so
  an edit accepted while the save was running is correctly still reported as unsaved afterwards.
- The saver is not respawned on demand (the model owns its handle). If the thread ever dies, each
  dropped publication is logged and persistence stops until `shutdown_saver` surfaces the error at
  exit.
- Model revisions and dirty sets are the synchronization contract with canvas/runtime
  subscribers; update them whenever visible shared state changes.
- Bubble ids are the stable identity for updates. Maintain the id index whenever the stored bubble
  list changes.
- Bubble autosave writes the latest snapshot to the unsaved staging path and must preserve
  explicit project-save semantics.
- A structural page operation pauses the bubble saver under its write gate, takes its shared
  snapshot, and writes that snapshot synchronously before remapping page indices. Do not add a
  bypass writer that can race this quiescence boundary.
- RGBA image buffers must match `width * height * 4`; mask buffers must match
  `width * height`.
- PNG/export-facing clean overlay buffers must be straight-alpha RGBA. Convert from
  `Color32` with `to_srgba_unmultiplied()` before writing to `RgbaImage`.
- Undo/redo for clean overlays uses `ms_actions::ActionHistory<CleanOverlayDiffOp>`: each committed
  edit is a tiled, zstd-compressed, reversible straight-RGBA `RasterDiff`, bounded by a 128-step
  count cap AND a per-memory-profile COMPRESSED byte budget (`MemoryBudget::clean_overlay_undo_bytes`,
  pushed via `set_memory_profile`). Applying a diff (`apply_raster_diff`) mutates the straight-RGBA
  cache first, then re-derives the `ColorImage` over the changed rects with `from_rgba_unmultiplied`
  so both representations stay byte-consistent. Region/brush construction is bounded and runs inline;
  the full-page construction path (`apply_overlay_snapshot`: clear / quick-clean / large region apply)
  still scans+compresses synchronously on the caller's thread (parity with prior behavior; off-thread
  is a planned Phase 2c follow-up). Because `RasterDiff` works in straight-alpha space, a synced
  `ColorImage` pixel can differ from a directly-blitted one by at most premultiplication rounding for
  partial alpha; the save/export RGBA cache is bit-exact.
- `detach_page_overlay` (page-manager clean management) selectively removes the page's undo/redo
  entries (per-page raster diffs are independent across pages, so other pages keep their history)
  and bumps the page's detach generation. `OverlaySaveSnapshot` carries that generation: writers
  that persist snapshots WITHOUT holding the model lock must use `save_overlay_snapshots_guarded`
  (or the autosave's guarded path), which skips stale pages before writing and removes a file
  written for a page detached mid-write, so an in-flight save can never resurrect a detached
  clean layer. `restore_dirty_save_snapshots` likewise skips stale snapshots on failure restore.
- `clean_assign::trash_clean_file` accepts only paths that stay strictly inside one of the two
  managed clean folders after lexical component validation (no `..`/root re-anchoring); anything
  else is rejected with a typed `TrashCleanError` before any filesystem access.
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
