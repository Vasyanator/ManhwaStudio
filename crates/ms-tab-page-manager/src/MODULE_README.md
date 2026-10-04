# Module: crates/ms-tab-page-manager/src

## Purpose
"Page manager" studio tab: an overview grid of the chapter's pages (thumbnails,
per-page badges) with multi-selection and STRUCTURAL page operations — insert
image files, create a blank page, reorder, delete. The tab never mutates the
chapter itself; it emits typed requests the app root executes through the
`crates/ms-page-ops/` engine.

This directory is the crate root of `ms-tab-page-manager`, re-exported by the binary from
`src/tabs/mod.rs` as `crate::tabs::page_manager`, so every existing
`crate::tabs::page_manager::…` path stays valid. Layer: near the top of the library stack —
above `ms-page-ops` / `ms-models` / `ms-widgets` and `ms-tab-ps-editor` (its viewport camera
drives the crop/split/stitch previews), below only `app.rs`. It must never name `app` or
`launcher`.

## Architecture
```
draw(ctx, ui, project, page_infos, textures, op_in_progress) -> Vec<PageManagerAction>
   |-- toolbar (top Panel)      structural buttons, disabled while an op runs
   |-- card grid (CentralPanel) show_viewport over grid_layout rows, selection, context menu
   |     |-- clean cards        clean_cards.rs: link gap + clean card under a page card
   |     `-- «Клин без страницы» clean_cards.rs: header row + unassigned clean cards (bind / delete)
   |-- status line (bottom)     totals: pages / with clean / bubbles / unassigned cleans
   |-- dialogs (Windows)        insert / create-blank / delete-confirm / stitch / split / crop
   `-- page viewer (Window)     viewer.rs: one page or clean at full resolution (double-click)
```

- `PageManagerAction::RequestOp(PageOpKind)` asks the app to quiesce writers,
  run the operation, reload the project, and then call `notify_pages_changed()`.
- `PageManagerAction::OpenPageIn { tab, page_idx }` asks the app to switch tabs
  focused on a page (the card context menu's "Open in: …" entries).
- A double-click on a page card, a bound clean card or an unassigned clean card opens the
  page viewer (`viewer.rs`, its own `viewer` slot, independent of `dialog`). The app lends
  its resident source-page textures to `draw` for it and asks `viewer_source_page` /
  `viewer_wants_nearest_source` which page (and filter) to keep resident for this tab.
- Shared models arrive through setters, mirroring the other tabs' wiring in
  `MangaApp::new`: `set_bubbles_model`, `set_overlays_model`, `set_layer_doc`.
- Badge data is cached and refreshed only when the source revision changes:
  bubble counts by `BubblesModel::revision`, clean-overlay presence by
  `CleanOverlaysModel::revision` (`is_overlay_virtual_absent`), layer counts by
  `LayerDoc::version` for resident pages plus a worker-side `layers.json` scan
  (unsaved manifest overrides saved) for everything else.
- All disk work runs on the worker thread in `thumbs.rs`: thumbnail decode +
  downscale (long side 192 px), page previews for the stitch window (long side
  ~1024 px) and for the split and crop windows (~2048 px, because one page must
  show a seam or a horizon sharply enough to place a cut or a crop edge on it),
  and the manifest scan. Thumbnails (page images and FILE-sourced clean cards) live
  in one LRU keyed by (path, mtime) that starts at 64 entries and grows, never
  shrinks, to twice the cards the grid draws in a frame
  (`ensure_visible_capacity`, fed by `GridLayout::card_count`), so a very large
  viewport cannot thrash it; previews live in a SEPARATE 6-entry LRU so a few megapixel-sized
  previews cannot evict the card grid's thumbnails. Both share the
  worker, the cancel flag, the epoch counter and the 8-job in-flight cap (whose key
  carries the job kind, so one page may have both pending).
  `notify_pages_changed` bumps a generation counter that forces mtime
  revalidation. Runtime reset also bumps a worker epoch so queued replies cannot
  upload stale textures; Drop cancellation abandons queued jobs before joining the
  worker.
- Clean thumbnails of pages whose clean lives in `CleanOverlaysModel` come from the
  MODEL, not from disk (dirty edits exist only in memory). The caller clones the
  page's `Arc<RgbaImage>` under a short lock only when `model_clean_thumb_wanted`
  says so; `thumbs.rs` never locks the model and never calls `delta_since` (change
  delivery belongs to the canvas). The worker downscales it exactly like a file thumbnail
  (long side 192 px, same sampler) and drops the `Arc` right after, because the
  model copy-on-writes any page still shared. Entries live in a SEPARATE LRU (same
  64-entry start and growth rule) keyed by page index, are valid only for the exact `revision()` they were
  taken at, and are stale-while-revalidate (the old texture stays drawable until
  the new one lands). At most one downscale per page is queued, counted in the
  shared cap. Being index-keyed, they are dropped by `clear_model_clean_thumbs`,
  which `notify_pages_changed` must call. Clean FILES (detached/problem) use the
  path pipeline, which keeps alpha; draw both over `ms_theme::checkerboard`.
- The native `rfd` file picker for "insert pages" is blocking and therefore
  runs on its own worker thread; the wasm build resolves it as a cancelled pick.
- `clean.rs` owns a second, serial worker for `clean_assign` inventory scans, image
  decoding, and every clean-file mutation. It receives immutable project snapshots
  (`CleanJobContext`: epoch, paths, pages), locks `CleanOverlaysModel` only briefly (never across
  encode, decode or disk I/O), and answers each mutating job with `CleanEvent::Finished {
  result, inventory }` — the inventory scanned right after the job. The GUI never reads clean
  files. Operations (all IMMEDIATE, committed tree, not undone by discard):
  - Unlink: one short lock captures the page's current `overlay_rgba` and calls
    `detach_page_overlay` (drops its undo history, bumps its detach generation); outside the lock
    the pixels are written to a new `<stem>[_<n>]_detached.png` (a virtual page's canonical file is
    moved there byte-exact instead, problem files included); then the page's canonical files are
    trashed. A failed write puts the pixels back with `replace_prepared_overlay` (dirty). Last, a
    page RE-materialized by an edit during the job is re-queued with `mark_overlay_needs_save`
    (its strokes are the new clean; the trash step may have removed their autosaved file).
  - Bind: an unassigned clean is renamed to the page's committed canonical name; a clean the page
    already has is unlinked first (kept, never lost). Only an exact-size file is decoded and
    handed to the model with `load_prepared_overlay` (not dirty); a mismatch is bound as-is and
    the page's link shows the problem (the loader skips it too).
  - Delete unassigned: both tree copies (staged removed, committed to `.pageop_trash`).
  - Attach / replace-from-file (legacy): `run_attach` writes the page to `_unsaved/clean_layers`
    through `save_overlay_snapshots_guarded` and trashes the source only after that write.
- `clean_link.rs` turns the model snapshot (`ModelPageClean`, refreshed per model revision) and
  the scanned `PageCleanEntry` into a per-page `PageCleanLink` (`None` / `Ok` / `Problem`), cached
  per (model revision, inventory epoch, page count).

## Files and submodules
- `mod.rs`: public contract (`PageManagerTabState`, `PageManagerAction`),
  setters, badge caches, toolbar, status line, per-frame orchestration.
- `grid.rs`: the virtualized card grid (`ScrollArea::show_viewport` driven by
  `grid_layout.rs`), card rendering, click/Ctrl/Shift selection
  (`selection_after_click`, unit-tested), the double-click that opens the page
  viewer, and the card context menu (with the "Open in: …" navigation). Card interactions use explicit ids (`("pm_card", idx)`), never
  auto ids.
- `grid_layout.rs`: GUI-free row table of the grid (unit-tested): the column
  formula, rows of VARIABLE height (page rows, optionally with a link gap and a
  clean-card slot; an optional bottom section of a header plus unassigned-clean
  rows) with prefix-summed tops, the viewport row range (binary search), and every
  card / gap / clean-card / header / link-widget rect relative to the content
  origin, the drawn card count of a row range, and the content scroll anchor
  (`anchor_for_offset` / `offset_for_anchor`). Contains no egui code; `grid.rs`
  converts its `LayoutRect`s.
- `dialogs.rs`: insert / create-blank / delete-confirm dialogs, the
  `InsertPosition -> at` resolution and the blank-page default-size rule
  (`default_blank_size`, unit-tested), the background file picker, and the
  `PageManagerDialog` enum every dialog (stitch, split and crop included) is
  dispatched through.
- `thumbs.rs`: worker thread + generic LRU `ThumbCache` (unit-tested) + the
  `layers.json` layer-count scan + the stitch/split/crop windows' page previews
  (`request_preview_if_needed` / `preview_state`, mirroring the thumbnail pair;
  `preview_state_cached` reads an entry WITHOUT promoting it, for a page the
  caller may not request a decode for) + model-sourced clean thumbnails
  (`model_clean_thumb_wanted` / `request_model_clean_thumb` / `model_clean_thumb`,
  cleared by `clear_model_clean_thumbs`) + `forget_path_thumb`, which forces a
  full re-decode of one file after a rename (a rename keeps the mtime, so a
  generation bump alone cannot notice it).
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
- `crop_layout.rs`: GUI-free core of the "crop page" feature (unit-tested): the
  `CropFrame` rect in ROTATED-CANVAS pixels and its invariants, the 8 resize
  handles plus the move region with their screen-constant grab rects and the
  corner-beats-edge-beats-move hit-test priority, the per-edge clamped drag
  (delta-driven, never inverting, never below `min_size`), aspect-ratio locking,
  the centred fit helpers, the rectangle INSCRIBED in the rotated page (the "no
  empty corners" fit, solved from the two half-plane constraints the rotation
  imposes), the translation that carries a frame from one rotated canvas into
  another, the rotation arithmetic that normalizes `(quarter_turns, angle_deg)`
  into the canonical pair, and the validation that mirrors the engine's crop
  preconditions. Contains no egui code and no I/O:
  screen geometry uses its own `ScreenRect` in points, which the window converts
  to `egui::Rect`. It deliberately does NOT compute the rotated-canvas size —
  that bounding box is the engine's (`page_ops::RotatedPage`) and arrives as a
  `canvas: [u32; 2]` parameter, exactly as its angle bound is imported from
  `page_ops` rather than restated.
- `crop.rs`: the "crop page" window — an `egui::Window` with the same
  `PsViewport` board as the split window, showing ONE page rotated by the chosen
  quarter turns plus a fine straightening angle, with a draggable crop frame,
  eight screen-constant handles and a move region over it, the region outside
  the frame veiled, an aspect-ratio preset row with a "fit inside the page"
  button, and the confirm that emits `PageOpKind::Crop`. Only draws and routes
  input: the frame math is `crop_layout.rs`'s and the canvas geometry is the
  engine's (`page_ops::crop_geometry::RotatedPage`). Its pure helpers are
  unit-tested
  (`screen_delta_to_canvas`, `page_quad_screen`, `aspect_ratio`,
  `quantize_angle`, `validate_state`, `confirm_enabled`, `build_crop_op`).
- `stitch.rs`: the "stitch pages" window — an `egui::Window` with a zoomable,
  pannable board of draggable page rectangles (camera: `PsViewport` from
  `tabs/ps_editor/viewport.rs`), the arrangement / fit / background strip, and
  the confirm that emits `PageOpKind::Stitch`. Only draws and routes input; all
  geometry decisions live in `stitch_layout.rs`.
- `clean.rs`: clean worker protocol and jobs (`run_unlink` / `run_bind` / delete / attach,
  unit-tested against temp chapters), inventory install + link-cache refresh, the entry points
  the clean-card UI calls (`request_unlink`, `request_bind`, `request_delete_unassigned`,
  `page_clean_link`, `unassigned_cleans`, `clean_bind_targets`, `clean_bind_fit`,
  `clean_mutation_blocked`), and the clean-operation confirmations (`CleanDialog`: replace from
  file, unlink from a menu, delete unassigned, bind at a mismatched/unknown size).
- `clean_cards.rs`: drawing of everything clean in the grid — the clean card under a page card,
  the dashed link with its status label, the two-step in-gap unlink control, and the
  «Клин без страницы» section (header, unassigned cards, "Привязать к …" submenu, delete). Pure
  helpers (unlink arm state machine, bind-menu grouping/labels, fit warning rule, problem
  tooltip) are unit-tested. It decides nothing about link state or file operations.
  Per-page paths come from `clean_assign::PageCleanPaths`, names from `clean_assign` — never a
  hand-built `<stem>.png`.
- `viewer.rs`: the page viewer — an `egui::Window` (a dialog window, not a panel: transient,
  per image and never persisted, so the panel-dock rule of `egui-docs/01-app-shell.md` §3.1 does
  not apply) with its own `ViewerCamera` board
  (fit on open, wheel zoom around the cursor, drag pan, "Fit" button, zoom and size readout).
  The camera is not `PsViewport` because its minimum zoom follows the fit zoom (a very tall
  strip must fit a window at its minimum height; `PsViewport`'s floor is a private constant).
  Pages are drawn from the app's lent `PageTexture` tiles by a TEMPORARY adapter
  (`paint_source_page`; removal: canvas source pages migrated to `egui-large-image`, plan
  step 5). Cleans are prepared into `egui_large_image::PreparedTiles` on the tab's single
  `ViewerWorker` thread (owned by `PageManagerTabState`, started lazily, reused by every
  viewer) and drawn as a `TiledTexture`, alone over the checkerboard or «поверх страницы»
  (only an OK link of exactly the page's size). The rules (`resolve_target`,
  `over_page_available`, `clean_request_due`, `reply_is_current`, `same_model_pixels`,
  `newest_job` / `run_viewer_worker`, the camera fit / zoom, texture reuse) are unit-tested.
- `clean_link.rs`: GUI-free, I/O-free page <-> clean link state (`page_clean_link`, table-tested)
  and the "bind to …" menu split (`bind_targets`: pages without a clean / with one that will be
  replaced) plus `bind_fit` for the as-is mismatch warning.

## Contracts and invariants
- The tab is NOT a `CanvasView` and must not become one; it OWNS no page
  textures beyond its own thumbnails, the bounded preview cache and the page
  viewer's single clean. A source page is never decoded here: the viewer shows it
  at full resolution by BORROWING the app's resident `PageTexture` tiles (lent to
  `draw` for the frame; an evicted tile is re-uploaded from its kept RGBA under
  the viewer's per-frame upload budget, never decoded), and the app keeps exactly
  that page in its source-page residency window while the tab is active
  (`viewer_source_page`, plus `viewer_wants_nearest_source` for the NEAREST
  tiles above 200 % zoom). At most ONE clean decode runs at a time per tab, on the
  tab's single viewer worker (never the thumbnail FIFO, where a full-resolution
  decode would stall every thumbnail): it drops every queued job but the newest
  before starting one; a decode already running when superseded (viewer replaced
  or closed) runs to completion — `image::open` cannot be interrupted — and its
  reply is discarded by epoch. Model pixels are an `Arc` clone under a short lock,
  dropped on the worker right after the split. The clean's CPU tiles and textures
  are freed when the viewer closes or is replaced, and while the tab is not drawn
  (`release_hidden_viewer_clean`, called by the app every frame before `draw`;
  the clean is prepared again when the viewer is drawn next). The stitch board
  therefore paints DOWNSCALED previews and REQUESTS at most as many of them as
  the preview LRU holds (`stitch.rs::MAX_LIVE_PREVIEWS`, defined from
  `thumbs.rs::PREVIEW_CACHE_CAPACITY`); a page that misses out is drawn as a
  numbered placeholder with NO caption — never the "loading" one, which would
  promise an image that is never coming — but an already-cached texture is still
  drawn (read without touching LRU order), so a rank swap during a pan does not
  blink the image away. The stitched RESULT is composed by `crates/ms-page-ops/` from
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
- The crop board's WORLD space is the ROTATED CANVAS' pixel space, not the source
  page's. The crop frame is therefore axis-aligned on screen at every rotation —
  which is what `crop_layout`'s `ScreenRect` handle geometry and hit test require
  — and what is drawn rotated is the PAGE, as a textured `epaint::Mesh` quad whose
  four corners come from the engine's own `RotatedPage::map_point`. The preview
  and the emitted rect can therefore never disagree about where the page sits,
  and the screen->canvas conversion of a drag is a plain division by the zoom:
  the rotation is carried by the world basis and must not be applied a second
  time.
- Crop frame coordinates are ROTATED-CANVAS pixels, never preview pixels — the
  same rule the split window's cut coordinates follow, for the same reason. A
  frame drag applies the pointer's DELTA from the frame the drag STARTED on,
  never its absolute position. The grab region is decided ONCE, on
  `drag_started`, by `crop_layout::hit_test`; a board drag that started on no
  handle is a pan. The hit test is deliberately not delegated to nine
  overlapping `ui.interact` rects, because egui would then resolve the
  corner-beats-edge priority by registration order instead of by the table
  `crop_layout` states and tests.
- A rotation of whole quarter turns is LOSSLESS (an integer pixel permutation)
  and the confirm strip stays silent about it. A non-zero fine angle is not: the
  page is resampled, bubbles' text rectangles degrade to the bounding boxes of
  their rotated selves, and detection data is discarded. That case carries a
  visible warning, gated on `angle_deg == 0.0` — the same exactness rule the
  engine's `PageRotation::is_identity` uses.
- Only a QUARTER TURN (or an aspect-preset change) rebuilds the crop frame, and
  only a quarter turn re-fits the camera: it transposes the canvas, so the old
  coordinates mean something else and the board's aspect flips. A change of the
  FINE angle must PRESERVE both — it only grows or shrinks the canvas around a
  page that stays centred in it, so the frame and the camera are translated by
  half the size change (`crop_layout::recentre_frame`) and stay over the same
  page content. Re-fitting there would make straightening by eye at working zoom
  impossible, which is the entire purpose of that control. Every rotation change
  goes through `crop_layout::normalize_rotation`, so straightening past ±45°
  rolls into the next quarter turn instead of leaving an angle the engine
  refuses, and through `quantize_angle`, so the slider and the spin box store one
  precision instead of one silently coarsening the other.
- The crop board needs an EXPLICIT pan affordance, and it is the middle button.
  Once the frame's screen rect covers the board, `crop_layout::hit_test` answers
  `Move` at every pixel — which the DEFAULT full-canvas frame does at any working
  zoom — so "drag where the frame is not" leaves no pannable pixel and every pan
  attempt would silently shift the crop instead. A middle-button drag therefore
  pans from anywhere, a drag outside the frame still pans, and the cursor (hand
  vs. resize arrow) says which a press will do.
- A straightening tool owes the user a "fit inside the page" affordance: a frame
  spanning the whole rotated canvas always contains the transparent wedges the
  rotation leaves. `crop_layout::largest_inscribed_frame` is the largest frame
  guaranteed to contain page pixels everywhere, honouring a locked ratio.
- The crop confirm, like the split one, is refused while the page PREVIEW failed
  to decode: the operation is immediate and is not undone by discarding unsaved
  changes, so it is never offered over a page the user cannot see.
- `PageOpKind` indices always refer to the CURRENT page order at request time;
  move semantics follow `page_ops/mod.rs` (`to` indexes the NEW order; UI
  position P maps to `to = P - 1`).
- No I/O or image decode on the GUI thread; shared-model locks are short and
  snapshot-out (counting happens after unlock).
- `notify_pages_changed` must be called by the app after every structural op or
  project reload; it clears the selection, any open dialog and the page viewer
  because page indices may have shifted. The viewer also re-resolves its target
  every frame (links / inventory) and closes itself when the page index no
  longer exists or the clean went away (unlinked, bound, deleted). Model cleans
  are keyed by the model's GLOBAL revision (the model has no per-page counter);
  a bump that left the page's `Arc` untouched (checked through a `Weak` identity
  token, sound because the model changes pixels only via `Arc::make_mut` or
  replacement) only re-keys the entry. New pixels (or a renamed file) are
  re-prepared; a result of the same size is swapped into the EXISTING
  `TiledTexture` (`mark_all_dirty`, re-sent in place under the upload budget) so
  the old pixels keep drawing — only a size change builds new textures. Replies
  are epoch-tagged so a superseded one is dropped. A dialog that holds page indices (delete, stitch)
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
- A clean job or probe sets a local in-flight flag until its worker reply. The gate is MUTUAL:
  every clean mutation is refused while `clean_mutation_blocked()` (= in flight, OR the frame's
  `op_in_progress` structural op / save, OR `overlays_loading` — the app's overlay loader has not
  delivered every page yet, and its late `load_prepared_overlay` would overwrite a bind/unlink);
  the app root refuses `start_page_op` / `request_save_to_project` while `clean_op_in_flight()` —
  a clean worker holds page indices and an Arc of the current overlays model, which a
  reload/merge would invalidate. The app root calls `poll_clean_events` every frame before those
  gates, on every tab, so the flag cannot stay stale while the page manager is not drawn.
- The model snapshot treats a page index beyond the model's page count as NOT materialized
  (`clean_link::model_page_materialized`); `is_overlay_virtual_absent` alone answers `false` there.
- Clean jobs carry FILE NAMES and page indices, never GUI-scanned paths: the worker re-resolves
  every file (staged over committed) at execution time.
- Inventory scans are epoch-tagged (same pattern as the layers scan). Rescan triggers:
  `notify_pages_changed`, the refresh button, `request_clean_rescan` (the app calls it after a
  successful save-to-project), tab re-activation (a gap in `ctx.cumulative_frame_nr()` between
  two draws), and every finished clean operation (its `Finished` carries the fresh inventory,
  installed when its epoch is still current, in every outcome including partial failure). Every
  install bumps the thumbnail generation (mtime revalidation); a mutating install also
  `forget_path_thumb`s every clean path of the old and new inventories, because a rename keeps
  the mtime.
- Link-state rule: a MATERIALIZED model page is `Ok { Model }` whatever the disk holds (known
  limitation: a mismatched committed file under it is overwritten at the next save); otherwise
  the page's canonical file resolved in `LOADER_CLEAN_SCOPE` decides (`None` / bound `Ok` /
  `Problem`). Detached names (`<stem>[_<n>]_detached.png`) never bind, so an unlinked clean is
  listed as unassigned.
- Clean-card UI: a page row reserves the link gap + clean-card slot iff ANY of its pages has a
  link other than `None` (`grid::page_has_clean_link`); only such pages draw a line, label and
  card. OK = green dashed line + "клин ✓"; `Problem` = yellow line + circled "!" + "Проблема"
  whose tooltip names the problem (both sizes for a mismatch). The page card no longer shows the
  clean state (only a weak "no clean" marker when there is none). The gap is hovered by a pure
  pointer query (`rect_contains_pointer`, no hitbox, never steals clicks); the unlink control
  sits INSIDE the gap so hovering it keeps the gap hovered. Unlink is two-step: the arm
  (`unlink_armed`, one page) is dropped whenever that gap is not drawn under the pointer, on
  `notify_pages_changed`, and a confirm needs a plain single click (neither `double_clicked` nor
  `triple_clicked`) landing at least twice egui's `max_double_click_delay` after the arm
  (`UnlinkArm::armed_at`, `unlink_transition`), so no multi-click burst both arms and confirms.
  The unlink entries of the page-card and clean-card menus give their disabled reason. Binding
  an unassigned clean needs a confirmation only when `clean_bind_fit` is not an exact match
  (bound as-is, not loaded until sizes match); replacing a page's clean needs none (the old one
  is kept as unassigned). Every mutating control is disabled with a reason while
  `clean_mutation_blocked()`. Clean thumbnails are drawn over `ms_theme::checkerboard::CANVAS`
  and requested only for cards of visible rows (plus the grid's one prefetch row). A linked
  unreadable clean and an unreadable unassigned file both read «Проблема», the error in the
  hover. The status line's "with clean" count uses the same links as the cards
  (`linked_clean_count`: OK and problem).
- Grid scroll stability: row heights depend on the clean links, so (1) until the first
  inventory of the current pages is installed (`grid_awaits_clean_inventory`: none installed and
  a scan in flight) the grid draws a loading placeholder and NO ScrollArea — a ScrollArea that
  is not shown keeps its persisted offset, one laid out with guessed short rows would clamp it
  (the app may rebuild the tab after a page op; the egui state survives); (2) the viewport top
  is remembered as a content anchor (`grid::GridScrollMemo`: first item of the top row +
  intra-row offset, with the layout it was taken in) and, only on a frame whose layout differs,
  re-applied through `ScrollArea::vertical_scroll_offset`, so user scrolling is never fought.
- "Replace clean from file" probes the picked file on the worker (header dimensions -> real
  `AttachFit`) before showing the confirmation dialog, so the dialog warns about scaling and an
  incompatible image is rejected with a localized error instead of being silently resized.
- Worker failures distinguish partial success (`CleanOpError`): attach source cleanup / persist,
  unlink leftovers (clean kept as unassigned but an old canonical file remains),
  bind-failed-after-unlink (names the detached file), bind source-twin cleanup, and bind model
  load — each reports exactly what was applied.

## Editing map
- To add a toolbar operation: `mod.rs` (`draw_toolbar`) and, if it needs
  confirmation/input, a dialog in `dialogs.rs`.
- To change card visuals/badges or selection behavior: `grid.rs`.
- To change where rows and cards go (row heights, spacing, the viewport range,
  column count): `grid_layout.rs` — never positions computed in `grid.rs`.
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
- To change crop frame math (handle geometry and hit-test priority, drag
  clamping, aspect-ratio locking, the fit helpers, rotation normalization,
  validation): `crop_layout.rs` — never the drawing code.
- To change how the crop window looks or reacts (the rotated preview quad, the
  veil, the handles, the rotation and ratio controls, the emitted op): `crop.rs`.
- To change what a rotated canvas IS (its bounding box, the point mappings, crop
  legality): `page_ops/crop_geometry.rs` — both the window and the engine import
  it, and neither may restate it.
- To change clean content operations (unlink / bind / delete): `clean.rs` jobs; naming and the
  never-replace file primitives live in `ms_models::clean_assign`.
- To change what a page's clean link state is: `clean_link.rs` — never the drawing code.
- To change how clean cards, the link, the unlink control or the unassigned section look or
  react: `clean_cards.rs` (sizes of the gap / header rows: `grid.rs` constants).
- To change the page viewer (entry targets, the board, the clean worker, the «поверх
  страницы» rule, the page-tile adapter): `viewer.rs`; its double-click entry points are in
  `grid.rs` and `clean_cards.rs`.
- To change what the app must execute: extend `PageManagerAction` (coordinate
  with the app-root integration and `crates/ms-page-ops/`).
