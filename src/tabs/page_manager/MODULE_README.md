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
   `-- dialogs (Windows)        insert / create-blank / delete-confirm / stitch / split
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
  downscale (long side 192 px), page previews for the stitch window (long side
  ~1024 px) and for the split window (~2048 px, because one page must show a seam
  sharply enough to place a cut on it), and the manifest scan. Thumbnails live in
  an LRU cache (64 entries) keyed by (path, mtime); previews live in a SEPARATE 6-entry LRU so a few
  megapixel-sized previews cannot evict the card grid's thumbnails. Both share the
  worker, the cancel flag, the epoch counter and the 8-job in-flight cap (whose key
  carries the job kind, so one page may have both pending).
  `notify_pages_changed` bumps a generation counter that forces mtime
  revalidation. Runtime reset also bumps a worker epoch so queued replies cannot
  upload stale textures; Drop cancellation abandons queued jobs before joining the
  worker.
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
  (`default_blank_size`, unit-tested), the background file picker, and the
  `PageManagerDialog` enum every dialog (stitch and split included) is dispatched
  through.
- `thumbs.rs`: worker thread + generic LRU `ThumbCache` (unit-tested) + the
  `layers.json` layer-count scan + the stitch/split windows' page previews
  (`request_preview_if_needed` / `preview_state`, mirroring the thumbnail pair;
  `preview_state_cached` reads an entry WITHOUT promoting it, for a page the
  caller may not request a decode for).
- `stitch_layout.rs`: GUI-free layout core of the "stitch pages" feature
  (unit-tested): `EditPlacement` and its engine-shaped field tuple, bounding box
  and `normalize` to a (0,0) origin, edge/alignment snapping during a drag,
  row/column arrangements, and the fit modes gated by `layout_kind`. Contains no
  egui widget code and no I/O; see `dev-docs/stitch_pages_plan.md` for the
  coordinate contract it implements. Its canvas/scale bounds are the ENGINE's,
  imported from `page_ops` rather than restated, so the dialog can never enable a
  confirm the engine refuses.
- `split_layout.rs`: GUI-free core of the "split page" feature (unit-tested):
  cut coordinates -> parts, the part order and its SWAP semantics, the drop mask
  that marks a part as discarded, cut insertion and removal that keep `order` a
  permutation and `deleted` aligned with it without disturbing the user's chosen
  order, drag clamping, and the validation that mirrors the engine's
  `PageOpKind::Split` preconditions. Axis-agnostic: everything is expressed along
  ONE axis as an extent in source pixels.
- `split.rs`: the "split page" window — an `egui::Window` with the same
  `PsViewport` board as the stitch window, showing one page with parallel cut
  lines (all horizontal XOR all vertical), a grab handle per line that carries a
  delete button, a per-part order picker (`WheelComboBox` placed at an absolute
  rect through `Ui::new_child`) whose list ends with a "Delete" entry, a veil
  over every part that entry marked, and the confirm that emits
  `PageOpKind::Split`. Only draws and routes input; all math lives in
  `split_layout.rs`. The picker PLACEMENT is itself pure and unit-tested
  (`order_widget_rects`), as is the handle drag (`dragged_cut_value`).
- `stitch.rs`: the "stitch pages" window — an `egui::Window` with a zoomable,
  pannable board of draggable page rectangles (camera: `PsViewport` from
  `tabs/ps_editor/viewport.rs`), the arrangement / fit / background strip, and
  the confirm that emits `PageOpKind::Stitch`. Only draws and routes input; all
  geometry decisions live in `stitch_layout.rs`.
- `clean.rs`: clean worker protocol, attachment-candidate ordering and persistence-path helpers
  (unit-tested), orphan section, and clean-operation confirmations.

## Contracts and invariants
- The tab is NOT a `CanvasView` and must not become one; it holds no page
  textures beyond its own thumbnails and the bounded preview cache. A
  full-resolution page is never decoded or uploaded here. The stitch board
  therefore paints DOWNSCALED previews and REQUESTS at most as many of them as
  the preview LRU holds (`stitch.rs::MAX_LIVE_PREVIEWS`, defined from
  `thumbs.rs::PREVIEW_CACHE_CAPACITY`); a page that misses out is drawn as a
  numbered placeholder with NO caption — never the "loading" one, which would
  promise an image that is never coming — but an already-cached texture is still
  drawn (read without touching LRU order), so a rank swap during a pan does not
  blink the image away. The stitched RESULT is composed by `src/page_ops/` from
  the untouched originals — the preview resolution never reaches it.
- Cut coordinates of the split window are SOURCE pixels, never preview pixels:
  the board's world space IS the page's pixel space, so the preview resolution
  limits only what the user can SEE, never the precision of what is emitted. A
  cut handle stores ONLY its perpendicular coordinate — it is drawn at the
  viewport centre along its line, which is what makes it slide back to the middle
  instead of needing an along-line position. A handle drag applies the pointer's
  DELTA, never its absolute position: at a ribbon's fit zoom one screen point is
  tens of source pixels, so snapping the line to the pointer would throw away the
  grab offset as a jump of hundreds of pixels.
- EVERY part of the split board carries an order picker, at every zoom. The
  picker keeps a fixed SCREEN size and may overhang a part narrower than itself
  (on a webtoon ribbon at fit zoom the page is a few dozen points wide, so a
  picker sized to the part would never appear at all — and the window offers no
  other way to reorder). Along the cut axis the pickers form a non-overtaking
  sequence whose pitch shrinks until they all fit the board, so a later picker
  can never fully cover an earlier one.
- A split part is targeted by TWO parallel arrays, the same pair the engine's
  request carries: `order` stays a permutation of `0..parts` over ALL parts
  (deleted ones included) and `deleted[k]` says whether part `k` becomes a page.
  A deleted part therefore keeps its position, so un-deleting it restores its own
  place for free, and every cut edit stays on the permutation math it was tested
  against: `insert_cut` gives the new half its parent's drop flag, `remove_cut`
  keeps the flag of the part whose POSITION survived. A part's PAGE NUMBER is its
  rank among the SURVIVORS (`survivor_rank`), never its raw `order` value, so
  deleting a part renumbers the rest with no further bookkeeping. Deleting EVERY
  part is refused (`SplitLayoutError::AllPartsDeleted`, confirm disabled, engine
  refuses it too); keeping exactly ONE is legal and is how a crop is expressed.
  The confirm strip counts SURVIVORS and warns, whenever any part is marked, that
  the discarded parts' bubbles, layers and clean overlay go with them.
- The order picker's "Delete" entry is reachable by CLICK ONLY. It is why the
  picker is built from `WheelComboBox::show_ui_with_wheel` and not from
  `show_index`: `show_index` cycles its WHOLE list on a wheel notch — even over a
  CLOSED picker — and `cycle_wrapped_index` wraps, so one stray notch past the
  last rank would discard a part's content without a click. The wheel's decision
  is `split_layout::wheel_choice` — GUI-free and unit-tested precisely because it
  is safety-critical: it walks the numeric ranks alone and can never yield
  Delete, and a deleted part holds no rank, so a notch over it does nothing.
  A cut-line removal that merges two parts keeps the deletion mark only when BOTH
  halves carried it: a deleted part shows "Delete" instead of a page number, so a
  rule keyed on the halves' positions would discard content unpredictably.
- The split board's wheel and its order pickers are mutually exclusive: a
  `WheelComboBox` cycles its selection on a wheel notch even while CLOSED, and
  egui reports the board as `hovered` underneath it (a click-only widget over a
  `click_and_drag` one leaves the board in `hits.drag`, and `hovered` is the
  union). The board therefore refuses the wheel over any picker rect — otherwise
  one notch would zoom AND silently swap two parts, emitting an order the user
  never chose.
- The split confirm is refused while the page PREVIEW failed to decode, even
  though the page size is known from `page_infos`: the operation is immediate and
  is not undone by discarding unsaved changes, so it is never offered over a page
  the user cannot see.
- `PageOpKind` indices always refer to the CURRENT page order at request time;
  move semantics follow `page_ops/mod.rs` (`to` indexes the NEW order; UI
  position P maps to `to = P - 1`).
- No I/O or image decode on the GUI thread; shared-model locks are short and
  snapshot-out (counting happens after unlock).
- `notify_pages_changed` must be called by the app after every structural op or
  project reload; it clears the selection and any open dialog because page
  indices may have shifted. A dialog that holds page indices (delete, stitch)
  must also re-validate them on EVERY frame: `clamp_selection` silently drops
  out-of-range indices after a reload, so a selection of two can become one
  under an open window. The stitch and split windows close themselves with a
  localized error in that case.
- A board that reads the RAW wheel delta (both windows do, because the wheel unit
  is not a distance) must skip its wheel reaction while a combo popup is open —
  `widgets::combo_popup_open`, the guard of `egui-docs/04-widgets.md` §2 — or the
  board zooms underneath an open order picker.
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
- To change thumbnail/preview decoding, caching, or the layer-count scan: `thumbs.rs`.
- To change stitch placement math (snapping, arrangements, fit modes, canvas
  size): `stitch_layout.rs` — never the drawing code.
- To change how the stitch window looks or reacts (board input, previews,
  settings strip, the emitted op): `stitch.rs`.
- To change split cut/part/order math (validation, insertion, drag bounds, the
  drop mask, survivor ranks, the resulting page numbers): `split_layout.rs` —
  never the drawing code.
- To change how the split window looks or reacts (cut lines, handles, order
  pickers, the emitted op): `split.rs`.
- To change orphan clean discovery or content operations: `clean.rs` and the GUI-free
  `models/clean_assign.rs` contract.
- To change what the app must execute: extend `PageManagerAction` (coordinate
  with the app-root integration and `src/page_ops/`).
