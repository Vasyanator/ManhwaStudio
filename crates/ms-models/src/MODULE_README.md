# Module: crates/ms-models/src (crate `ms-models`)

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

`TextMaskModel` stores detector mask alpha planes by page index with source and mask dimensions,
plus the optional detector text boxes (`TextMaskPage.blocks`) that describe the same detection.
Writers replace whole pages (mask + blocks) or use closure-based in-place edits; readers track the
model revision to refresh local mask caches. Autoclean reads `blocks` to build a box-based mask
candidate.

## Files and submodules
- `autosave_gate.rs`: `AutosaveGate`, the per-project (`Arc`) decision of WHEN held autosave work
  is flushed to `_unsaved`. A monotonic `flush_epoch` advances when the policy interval has passed
  since the FIRST pending action, when the action count reaches the threshold, or on
  `force_flush`; each writer keeps its own `seen_epoch` and is due iff `poll() != seen_epoch`. The
  policy is read from `ms_config::autosave_policy` on every call (live settings); tests inject one
  with `AutosaveGate::with_policy_fn` instead of touching the process globals.
- `bubbles_model.rs`: shared bubble list, revision tracking, canvas settings, and
  coalesced background saving. The bubbles document is written only through
  `ms_docstore::write` (`write_bubbles_snapshot_to`) and its staging existence is
  probed with `ms_docstore::exists` — never a raw file write. The `DocRef` comes from
  `ms_page_ops::chapter_docs::chapter_doc_for_write`, so a NEW staging document is
  created in the chapter's format (`.json` / `.db`, docstore rule B.3), never the
  process default when the chapter already has a document.
- `clean_assign.rs`: the worker I/O half of the page <-> clean binding owner (the pure rule is
  `ms_page_ops::clean_binding`, re-exported here). `PageCleanPaths` + `probe_page_clean` ->
  `PageCleanResolution` resolve one page's clean in a `CleanTreeScope` (the app's overlay loader
  uses `LOADER_CLEAN_SCOPE` = `StagedOverCommitted`: staged shadows committed, the same view the
  save-merge produces; `CommittedOnly` — what a saved chapter holds alone — has no production
  consumer yet and is exercised by the resolver tests); `scan_clean_inventory` -> `CleanInventory` (`PageCleanEntry` per page,
  `UnassignedClean` for non-canonical names) scans both trees once, and `scan_orphan_cleans` is
  its projection (`orphans_from_inventory`). Also: the `*_detached` naming convention
  (`detached_clean_name` / `allocate_detached_clean_path`: `<stem>[_<n>]_detached.png`, free in
  BOTH trees and never a page's canonical name), the page manager's immediate file operations
  (`write_new_clean_png`, `move_clean_file`, `delete_unassigned_clean`; typed `CleanFileOpError`),
  `attach_fit` / `load_clean_for_attach` (OPERATION policy for attaching a picked file — 1% aspect,
  rescales — not the binding rule), and moving committed files to trash. Consumers: the app
  loader, the page manager (every operation), the Cleaning tab's «Клин» status area
  (`scan_clean_inventory`: `PageCleanEntry::resolve` in `LOADER_CLEAN_SCOPE` for "not loaded",
  `orphans_from_inventory` for unassigned files) and the typing export's disk fallback
  (`probe_page_clean` in `LOADER_CLEAN_SCOPE` + `loadable_file`), so these agree with the canvas
  on which file binds; so do the page manager's clean cards (`PageCleanEntry::resolve` in
  `LOADER_CLEAN_SCOPE`).
  The scan compares found names with `clean_binding::clean_name_key` (ASCII-case-insensitive on
  Windows, as the loader's open of `<stem>.png` is there).
- `clean_overlays_model.rs`: shared clean overlay images, undo/redo history, dirty
  tracking, autosave snapshots, and cached decoded page images.
- `text_mask_model.rs`: shared text detector masks keyed by page index.
- `page_view.rs`: source-page view model shared by the app shell, the canvas and the tabs —
  page geometry with its load state (`PageImageInfo` / `SourcePageLoadState`) plus the tiled GPU
  residency of a decoded page (`PageTexture` / `TextureTile`). Producing and evicting the
  textures stays in `app.rs`; only the types live here.
- `lib.rs`: module declarations for the shared model layer.

## Crate boundary
`ms-models` sits ABOVE `ms-project` (these models load and save the chapter domain model) and
BELOW `ms-canvas` and the tabs. egui appears here ONLY as a pixel format (`ColorImage` /
`Color32`): an `egui::Ui` or `egui::Painter` in this crate is a layer violation. The crate may
not name `ms-canvas`, `ms-widgets` or anything in `src/`.

The three-way canvas-defaults agreement test (`canvas_defaults_agree_across_the_three_mirrors`)
therefore lives with the mirror it cannot reach from here — `CanvasState::default` in
`crates/ms-canvas/src/types.rs` — and not next to `SharedCanvasSettings::default`.

Four accessors of `layer_model/layer_doc.rs` (`LayerDoc::ensure_page_loaded`, `LayerDoc::node`,
`LayerNode::is_text`, `LayerNode::display_image`) are test-only and reach dependent crates'
tests through this crate's `test-support` feature rather than `#[cfg(test)]`; the binary enables
it from its `[dev-dependencies]`, so no production build carries them.

## Contracts and invariants
- Every `clean_assign` public function that accesses images or files is synchronous and must run
  on a worker thread. Unreadable clean images remain visible to callers as diagnostic orphans.
- The page <-> clean binding (canonical `<stem>.png` name, exact-size fit) is decided only by
  `ms_page_ops::clean_binding` + `clean_assign`. Clean writers here name files through
  `OverlaySaveSnapshot::file_name` / `clean_overlay_file_name` and derive stems with
  `writer_clean_stem`; never restate the name or the fit rule. Only the exact canonical name binds:
  a same-stem file of another extension (`001.webp`) belongs to no page (`NoMatchingPage`).
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
    releases exactly one level; at zero the held snapshot returns to the normal (gate) decision. The
    barrier itself first force-writes whatever the autosave gate was holding, then acks. A boolean hold was broken
    by a second concurrent holder releasing someone else's. Shutdown during a hold waits for every
    holder, then persists the held snapshot — it must never drop it silently.
    Never call it on the GUI thread; the one exception is `shutdown_saver`, which uses it internally
    on the exit path where the drain is bounded and the process is ending anyway. Its "flushes
    everything enqueued before this call" guarantee is exact only for the FIRST holder: a barrier
    nested inside an active hold acks immediately while pre-barrier snapshots still sit in
    the worker's `pending` snapshot. Safe today (only `shutdown_saver` nests, and Shutdown waits + persists), but do
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
- **An autosave "action" is one enqueued save gesture** (stroke commit, text edit, bubble change),
  reported via `AutosaveGate::note_action` at the enqueue funnel — never per pixel mark or per
  frame, or the action threshold would degenerate into immediate mode. The gate does no I/O and
  holds its lock only for counter updates.
- **Autosave hold — the three writers** (each takes `Option<Arc<AutosaveGate>>`; `None` = immediate
  mode, today's write-on-drain, used by tests/tools): the layer saver (`LayerDoc::enable_background_saver`)
  keeps its coalescing bucket across drains; the bubbles saver (`BubblesModel::new(.., gate)`) keeps
  its latest snapshot; the clean-overlay autosave keeps the model's dirty set
  (`CleanOverlaysModel::set_autosave_gate` + `ms_canvas::spawn_overlay_autosave_thread(.., gate)`).
  Each writes when the gate epoch moves past the epoch it last acted on. ORDER CONTRACT: an action
  is reported AFTER its job/snapshot is sent (or its dirty mark inserted), and a writer polls the
  gate BEFORE draining — so a flush epoch never covers work the pass does not contain (reversing
  either side can strand a job with no open window). Barriers, `BarrierAndHold`, non-discarding
  shutdown and `FlushAndStop` always write what is held; discard drops it. Held layer jobs keep
  their epochs unacknowledged, so `has_pending_saves()` stays true. Memory: held layer jobs carry
  pixels only for `pixels_dirty` rasters + dirty/missing text renders of pending pages (which stay
  resident) + the PS active page's raster set; bubbles hold one `Arc`; clean holds nothing extra.
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
- `TextMaskPage.blocks` are detector text boxes as `[x1, y1, x2, y2]` covering integer rects in
  **source-page pixel space** (same space as `source_size`, NOT `mask_size`); mask-space consumers
  must apply the source→mask scale themselves. Writers pass the boxes matching what the mask
  contains (raw boxes for the raw glyph mask, resolved/expanded/merged boxes for a rasterized-box
  mask). `blocks == None` means "no detector boxes known"; the model never stores `Some(vec![])`.
  A manual mask edit (`edit_page_mask` closure reporting a change) invalidates `blocks` to `None`,
  because a hand-edited mask no longer matches the detector boxes.
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
  `mark_overlay_needs_save` re-queues a page's current pixels for saving without recording an edit
  (for a worker that removed the page's staged file while an edit may have re-materialized it).
- `clean_assign`'s file operations never replace an existing file: a new file is written to a
  hidden same-directory temp (`ms_docstore::temp_path_for`, `.{name}.{pid}.tmp`; crash leftovers
  are skipped by the inventory scan via `ms_docstore::is_temp_artifact`), fsynced, and renamed only
  after re-checking the
  destination is free; a move is check-then-rename (a cross-device staged -> committed move falls
  back to copy + remove). The check-then-rename is not atomic by itself — it relies on the page
  manager's single serial clean worker plus the app refusing page ops / saves while it runs.
  `delete_unassigned_clean` removes BOTH tree copies of a name (a surviving one would resurface).
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
