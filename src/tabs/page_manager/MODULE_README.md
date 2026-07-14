# Module: src/tabs/page_manager

## Purpose
"Page manager" studio tab: an overview grid of the chapter's pages (thumbnails,
per-page badges) with multi-selection and STRUCTURAL page operations — insert
image files, create a blank page, reorder, delete. The tab never mutates the
chapter itself; it emits typed requests the app root executes through the
`src/page_ops/` engine.

## Architecture
```
draw(ctx, ui, project, page_infos, op_in_progress) -> Vec<PageManagerAction>
   |-- toolbar (top Panel)      structural buttons, disabled while an op runs
   |-- card grid (CentralPanel) virtualized rows, selection, context menu
   |-- status line (bottom)     totals: pages / with clean / bubbles
   |-- orphan-clean section     worker-scanned invalid clean files and attachment candidates
   `-- dialogs (Windows)        insert / create-blank / delete-confirm
```

- `PageManagerAction::RequestOp(PageOpKind)` asks the app to quiesce writers,
  run the operation, reload the project, and then call `notify_pages_changed()`.
- `PageManagerAction::OpenPageIn { tab, page_idx }` asks the app to switch tabs
  focused on a page (double-click / context menu navigation).
- Shared models arrive through setters, mirroring the other tabs' wiring in
  `MangaApp::new`: `set_bubbles_model`, `set_overlays_model`, `set_layer_doc`.
- Badge data is cached and refreshed only when the source revision changes:
  bubble counts by `BubblesModel::revision`, clean-overlay presence by
  `CleanOverlaysModel::revision` (`is_overlay_virtual_absent`), layer counts by
  `LayerDoc::version` for resident pages plus a worker-side `layers.json` scan
  (unsaved manifest overrides saved) for everything else.
- All disk work runs on the worker thread in `thumbs.rs`: thumbnail decode +
  downscale (long side 192 px) and the manifest scan. Thumbnails live in an LRU
  cache (64 entries) keyed by (path, mtime); `notify_pages_changed` bumps a
  generation counter that forces mtime revalidation. Runtime reset also bumps a
  worker epoch so queued replies cannot upload stale textures; Drop cancellation
  abandons queued jobs before joining the worker.
- The native `rfd` file picker for "insert pages" is blocking and therefore
  runs on its own worker thread; the wasm build resolves it as a cancelled pick.
- `clean.rs` owns a second, serial worker for `clean_assign` scans, image decoding/resizing,
  and destructive clean-file operations. It receives immutable project snapshots, locks
  `CleanOverlaysModel` only after decode, reports completion through `mpsc`, and triggers a
  fresh orphan scan after each operation. The GUI only does candidate arithmetic from known
  page dimensions; it never reads clean files or decodes images.

## Files and submodules
- `mod.rs`: public contract (`PageManagerTabState`, `PageManagerAction`),
  setters, badge caches, toolbar, status line, per-frame orchestration.
- `grid.rs`: the virtualized card grid (`ScrollArea::show_rows`), card
  rendering, click/Ctrl/Shift selection (`selection_after_click`, unit-tested),
  double-click navigation, and the card context menu.
- `dialogs.rs`: insert / create-blank / delete-confirm dialogs, the
  `InsertPosition -> at` resolution and the blank-page default-size rule
  (`default_blank_size`, unit-tested), and the background file picker.
- `thumbs.rs`: worker thread + generic LRU `ThumbCache` (unit-tested) + the
  `layers.json` layer-count scan.
- `clean.rs`: clean worker protocol, attachment-candidate ordering and persistence-path helpers
  (unit-tested), orphan section, and clean-operation confirmations.

## Contracts and invariants
- The tab is NOT a `CanvasView` and must not become one; it holds no page
  textures beyond its own thumbnails.
- `PageOpKind` indices always refer to the CURRENT page order at request time;
  move semantics follow `page_ops/mod.rs` (`to` indexes the NEW order; UI
  position P maps to `to = P - 1`).
- No I/O or image decode on the GUI thread; shared-model locks are short and
  snapshot-out (counting happens after unlock).
- `notify_pages_changed` must be called by the app after every structural op or
  project reload; it clears the selection and any open dialog because page
  indices may have shifted.
- All user-visible strings are `page_manager.*` keys present in BOTH
  `crates/ms-i18n/locales/en.json` and `ru.json`; `.pageop_trash` and
  `layers.json` are persistence identifiers (i18n-exempt), surfaced only via
  placeholders.
- A clean attach/detach/delete/probe sets a local in-flight flag until its worker reply. The
  gate is MUTUAL: clean buttons and dialog confirmation are disabled while `op_in_progress`
  (structural op / save), and the app root refuses `start_page_op` / `request_save_to_project`
  while `clean_op_in_flight()` is true — a clean worker holds page indices and an Arc of the
  current overlays model, which a reload/merge would invalidate.
- Orphan scans are epoch-tagged (same pattern as the layers scan): `notify_pages_changed`, the
  refresh button, and every finished clean operation bump the epoch; a scan result from a
  superseded epoch is dropped. A completed clean operation always rescans (in every outcome,
  including partial failure), so a retained size-mismatched committed clean is visible both as
  an orphan and as a card warning badge.
- "Replace clean from file" probes the picked file on the worker (header dimensions -> real
  `AttachFit`) before showing the confirmation dialog, so the dialog warns about scaling and an
  incompatible image is rejected with a localized error instead of being silently resized.
- Worker failures distinguish partial success (`CleanOpError`): an attach whose source cleanup
  failed and a detach whose file trashing partially failed report exactly what was applied.

## Editing map
- To add a toolbar operation: `mod.rs` (`draw_toolbar`) and, if it needs
  confirmation/input, a dialog in `dialogs.rs`.
- To change card visuals/badges or selection behavior: `grid.rs`.
- To change thumbnail decoding, caching, or the layer-count scan: `thumbs.rs`.
- To change orphan clean discovery or content operations: `clean.rs` and the GUI-free
  `models/clean_assign.rs` contract.
- To change what the app must execute: extend `PageManagerAction` (coordinate
  with the app-root integration and `src/page_ops/`).
