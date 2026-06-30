# Module: src/tabs/ps_editor

## Purpose
Standalone Photoshop-like, single-page, layered editor exposed as the `AppTab::PsEditor`
("PS-подобный редактор") tab. Unlike Translation/Cleaning/Typing it is **not** a `CanvasView`: it
owns its own pan/zoom camera, layer stack, selection, tool set, and tiled GPU cache.

## Architecture
Data flow for one page:

```
project page -> page_loader (worker) -> LoadedPage (source + clean ColorImage
                                         + DecodedPagePayload: user raster/text/group nodes,
                                           decoded LOCK-FREE via LayerDoc::decode_page_payload)
             -> poll_loader: brief doc lock to insert_decoded_page (NO decode under lock)
             -> LayerStack (Source + Clean base layers); sync_view_from_doc materializes the
                user raster layers + text + bands from the shared LayerDoc (source of truth)
             -> per-layer TiledTexture cache (sized to the layer image, budgeted upload)
             -> draw_canvas: composite bottom->top, each layer mapped local->page->screen
                via its LayerTransform + ViewTransform (rotated/scaled tile meshes)
tool input  -> active PsTool::interact -> ToolOutcome (dirty rect / selection change)
            -> tile invalidation; marquee drawn from Selection::outline_loops
```

PAGE-SWITCH DECODE IS OFF-THREAD. On a page switch the worker now decodes BOTH base layers AND the
persisted user-layer payload (raster PNGs + text + groups + legacy `text_info.json` migration) via the
PURE `LayerDoc::decode_page_payload` (it holds no doc `Arc`, only the dirs + the FULL chapter page-size
map). `poll_loader` takes the shared doc lock ONLY to `insert_decoded_page` (a cheap move, never a
decode) — so the doc lock is never held across a multi-MB PNG decode and the GUI stays responsive. The
raster PNGs are decoded exactly ONCE (the former double decode — `load_persisted_into_stack` plus the
doc's own decode — is gone; `load_persisted_into_stack` was removed and the stack is built from the
doc by `sync_view_from_doc`). A worker decode failure leaves the page un-inserted; it still opens with
its two base layers. GPU texture creation / `render_cache` / `node_generations` reset stay on the GUI
thread (textures cannot be created off-thread).

The two base layers mirror existing shared state **read-only**:
- `Исходник` (source) comes from `CleanOverlaysModel::cached_page_rgba` (worker-decoded, cached).
- `Клин` (clean) comes from `CleanOverlaysModel::overlay_rgba`; absent overlay → transparent.

Base layers are locked: they can be hidden but never deleted, reordered, painted on, or
transformed; they stay page-sized with the identity `LayerTransform`. Any number of user `Raster`
layers stack above them. Raster layers may be **smaller than the page** ("incomplete") and carry an
affine `LayerTransform` (center/rotation/uniform scale) so they can be moved, rotated, and scaled.
User raster layers are preserved per page via `saved_raster` (in-memory, session) and persisted to
disk via the shared `models::layer_model` (`{chapter}_unsaved/layers/layers.json` + per-layer PNGs,
merged into `{chapter}/layers/` on "save to project"). They are written when leaving a page
(`persist_current_page`, called from `request_page`) and on save (`flush_layers`), and reloaded on
first visit to a page (`load_persisted_into_stack`). After a successful flush, `persist_current_page`
calls `LayerStack::mark_rasters_persisted` to clear each raster's `pixels_dirty` flag: the base PNG is
now on disk, so a later flush treats the raster as clean and preserves a non-destructive effects chain
the typing tab may have added in between — leaving the flag set would re-run the dirty path, rewrite
the base, and silently drop those effects.

ASYNC PERSISTENCE: layer writes are now OFF-THREAD via the doc's background saver (`models/layer_model/
saver.rs`). The per-edit `route_to_doc` flush calls `doc.enqueue_page_save`; text edits call
`doc.enqueue_page_text_save`; the effects poll (`apply_ps_raster_effects_result`) calls
`doc.enqueue_raster_effects`. `persist_current_page` stays NON-redundant: it reads the PS `self.stack`
(not the doc) and carries the EXPLICIT `removed_uids` from `self.deleted_raster_uids` (the doc's
whole-page enqueue passes an empty removed set, which would PRESERVE a deleted raster as "another
tab's" → resurrection), so it builds an owned `saver::PageSaveJob` (raster part + explicit removed set,
no effects reconcile — mirroring the sync `save_page_rasters` that preserves another tab's effects) and
enqueues it through `doc.saver_handle()`, falling back to a synchronous `save_page_rasters` when no
saver is enabled. The just-enqueued bytes are guaranteed on disk by the save-to-project merge-worker
barrier and the app-close drain. `layers_dirty` tracks whether the current page has an edit not yet
enqueued (set on a deferred `edit_doc_node`, cleared on any enqueue/flush); the tab-switch
`flush_layers` (in `app.rs`) only runs when it is set (conservative — flush when in doubt). Base layers
are never persisted — they mirror
`src/` and `clean_layers/`. See `models/layer_model/` for the on-disk schema and the unified
layer-model roadmap (groups, text layers, effects, typing-tab sync).

## Files and submodules
- `mod.rs`: `PsEditorTabState` orchestration — panels (page switch / toolbar / layers), canvas
  input routing, render-cache sync, the dashed selection marquee, the selection right-click
  copy/cut menu (`clip_into_new_layer`), layer/group save+load via `models::layer_model`,
  `merge_down` (`composite_to_page` flattens a raster onto the raster directly beneath it by unified
  BAND-Z — `raster_below_by_band_z` / `raster_below_uid`, NOT the layer-stack neighbour, so a manual
  reorder merges the visually-below pair; base layers are never a target), and tab-local hotkeys
  (`B`/`M`/`L`, `Ctrl+D`).
  - **Unified layers panel** (`draw_layers_panel` → `layers_panel_body`): a Photoshop-like tree of
    compact rows (visibility eye + name + group indent), built each frame by `tree::build_unified_tree`
    into an owned snapshot so the render loop can mutate `panel_selection` without borrowing the
    stack. Collapsible/movable groups may mix rasters and texts; text overlays are interleaved by Z in
    the same tree (no separate bottom section). Multi-select: plain = replace, Ctrl/Cmd = toggle,
    Shift = range (`select_row`). A right-click menu groups the selection (`GroupOp` → `apply_group_op`
    → `persist::save_page_grouping`): create / move-to-existing / ungroup / delete. Per-row detail
    (opacity, fx, merge, delete, pin, rasterize) lives in the **active-layer controls strip** at the
    bottom (`draw_active_controls`, keyed on `panel_primary`). Reordering routes through the unified
    band order: `build_unified_order` produces a contiguous order (groups pulled to their lowest
    member Z), `move_band_one` / `move_group_block` swap a band / a whole group block. Group
    collapse/visibility/opacity are stack-only (folded live in `draw_composite`, persisted on
    page-leave). The two grouping axes: `Layer.group`/`group_uid` (unified PS tree) vs typing's
    `layer_idx` text groups — kept independent so the typing tab is untouched.
- `viewport.rs`: `PsViewport` camera (pan/zoom/fit/100%) and the per-frame `ViewTransform`
  (image↔screen mapping). Independent of the shared canvas engine.
- `text_layers.rs`: `PsTextLayer` — display of the typing tab's overlays, projected from the shared
  `LayerDoc` (the source of truth). `sync_view_from_doc` builds/reconciles one text layer per doc Text
  node (image + geometry + group from the node); `load_page_text_layer_meta` seeds PS-owned pin /
  pinned_by_group / text-group `layer_idx` from the `layers.json` text nodes (NOT `text_info.json`).
  PS reads NO `text_info.json` (the doc owns text). Rendering mirrors the typing tab: a deformed
  overlay draws its textured `cols`×`rows` mesh (absolute page-pixel control points mapped through the
  viewport), otherwise a plain affine quad. Deformed overlays are skipped by PS affine drag (edit them
  in typing). With the Transform tool the user can drag a text layer (translate); on release the new
  placement routes through the shared doc (`edit_doc_node` → `set_transform`) and is persisted by the
  doc's inline text flush to `layers.json` (`flush_text_page`). Kept out of `LayerStack` so raster
  tools/invariants are untouched.

Cross-tab sync: both tabs hold the shared in-memory `LayerDoc` (`set_layer_doc`, created in `app.rs`),
the source of truth for per-page layer MODEL state. Raster/text MODEL edits route through it
(`route_to_doc` / `edit_doc_node`). The band-order / grouping / pin ops persist their band order to
disk (legitimate persistence) and ALSO mirror the same change onto the doc in-memory — group
create/assign/ungroup/delete via `add_group`/`set_group`/`remove_group`, a single-band intra-group
move via `reorder_node_one`, a group-block move via `reorder_group_block`, and any wider reorder
(ungrouped block-hop, grouping reorder, pin/unpin) via `set_z_order` over the expanded node order
(`expand_order_to_node_uids`). Either way the doc's monotonic `version` is bumped (no disk round-trip
needed for cross-tab sync). Each frame `refresh_view_if_doc_version_changed` re-projects the current page (via
`sync_view_from_doc`, preceded by `reload_overlays_view` to pick up the disk-truth text-layer runtime)
when the version advanced. The old disk-revision counter / app bridge are gone. Limitation: editing is
tab-switch-driven (the idle tab isn't mid-edit); the same node is not edited live in both tabs at once.
- `layers.rs`: `LayerStack`, `Layer`, `LayerKind`, `LayerTransform` (center/rotation/scale + local↔
  world helpers); base-layer invariants, add/remove/reorder, `add_raster_layer_image` (paste a
  composited region at a transform), `active_transformable_mut`, and per-page raster stash/restore.
  Also `LayerGroup` and single-level grouping: a raster layer carries an optional `group`; a hidden
  group hides its members and its opacity multiplies theirs (`layer_visible` / `layer_opacity`
  resolve this at composite time). Groups are stashed/restored per page alongside raster layers and
  persisted in `layers.json` (`group_uid` per node + a `groups` list).
- `selection.rs`: `Selection` binary mask plus boundary `outline_loops` for the marquee; rectangle
  + polygon (lasso) construction; `bounds` accessor.
- `tools/`: `PsTool` trait + context (carries the frame `ViewTransform`); rectangle/lasso selection,
  the color brush (paints in layer-local space), and `transform.rs` (move/rotate/scale gizmo).
- `tree.rs`: pure builder for the unified layers panel. `build_unified_tree(stack, text_layers,
  bands)` joins raster layers + text overlays + groups into one `Vec<TreeItem>` (group headers +
  indented leaves) ordered top-to-bottom by the unified Z, with the same tiebreak as `draw_composite`
  so panel order == composite order. A group is a maximal contiguous same-`group_uid` run (the
  contiguity invariant is enforced at write time in `persist::save_page_grouping`).
- `layer_render.rs`: `TiledTexture` — per-layer tile grid (sized to the layer image), dirty
  tracking, budgeted upload, and transform-aware mesh draw.
- `page_loader.rs`: background worker producing the two base-layer images for a page.

## Contracts and invariants
- GUI thread never decodes images or holds the model lock across decode: that is `page_loader`'s
  job; the model lock is released before `image::open`.
- Non-destructive raster effects render off the GUI thread. `apply_effects_to_raster` parses the
  chain, clones the pre-effects base ColorImage (dropping the stack borrow first), and spawns
  `render_ps_raster_effects`, which runs the expensive `apply_effects_to_color_image`. A render
  already in flight stashes the latest request in `pending_raster_effects` (latest-wins).
  `poll_ps_raster_effects_jobs` (called once per frame from `draw`) consumes the result and does the
  cheap GUI-side apply — recenter anchoring, `edit_doc_node` routing (swap base/display/effects +
  bump generation), reversible `persist::update_raster_effects` (base PNG untouched), `render_cache`
  drop — then re-dispatches any stashed request. This mirrors the typing tab's
  `apply_raster_effects_edit` / `render_raster_effects` / `poll_raster_effects_jobs` trio. The base
  PNG is never rewritten by effects; only the `_fx` rendered PNG is, so the chain stays reversible.
- Base layers (`LayerKind::Source` / `Clean`) are never written back to `CleanOverlaysModel`. Tools
  only mutate the active editable raster layer (`LayerStack::active_editable_mut`).
- Selection copy/cut (`clip_into_new_layer`) composites the chosen layers bottom-to-top within the
  mask into a new raster layer **cropped to the selection bounds** and placed at the matching page
  position (so clip results are "incomplete", movable layers). Layers are sampled through their
  transforms (`sample_layer_world`), so partial/rotated/scaled sources contribute correctly. A
  **cut** also clears the selected pixels from every chosen layer **except** `LayerKind::Source`,
  which is immutable and can never be cut from; the `Clean` overlay and raster layers are cuttable.
- The selection is shown as a thin black-and-white dashed marquee drawn from
  `Selection::outline_loops` (no translucent fill, never blue), matching the in-progress
  drag preview in `tools/select.rs`. "Выделить слой полностью" sets the selection to the active
  layer's footprint (page rect for a base/page-sized layer, transformed quad polygon otherwise);
  when the panel's primary row is a TEXT layer (not in `LayerStack`) it uses that overlay's
  `PsTextLayer::footprint_polygon` instead. Clicking any layer row also requests this selection so
  the marquee follows the active/primary layer immediately.
- The transform tool (`tools/transform.rs`) mutates only the active raster layer's `LayerTransform`
  (no pixels), so it needs **no** tile re-upload — `draw` re-evaluates the transform each frame.
  Base layers are not transformable (`Layer::is_transformable`).
- Tools mutate the in-memory stack/selection only — no GPU, file, model, or backend access. The tab
  translates `ToolOutcome::dirty` into `TiledTexture::mark_dirty_rect` (in layer-local pixels).
- `TiledTexture` is sized to each layer's own image; `sync_render_cache` rebuilds a layer's cache
  when its image is resized (e.g. a freshly cropped clip). Base layers stay page-sized.
- This tab is excluded from the canvas source-page residency window in `app.rs`; it manages its own
  page residency via `page_loader`.
- View sync with `CanvasView` (driven from `app.rs` on tab transitions, "в доступных пределах"):
  `sync_view_from_canvas` mirrors the canvas's current page, zoom, and page-local camera center on
  entry, and `current_page`/`camera` feed them back so the canvas follows on exit. Zoom is clamped
  to each side's own limits, so the freer PS editor honors a canvas zoom as far as it can.
  - The synced camera is parked in `pending_camera` and applied only once its target page finishes
    its async load, because the load refits the camera (`viewport.invalidate`).
  - Clean-overlay sync is one-way (model → tab): `sync_view_from_canvas` compares
    `CleanOverlaysModel::revision` against `last_overlay_revision` and reloads the page (preserving
    raster layers) when it changed, so the `Клин` base layer reflects edits made on other tabs.

## Editing map
- To change the camera (pan/zoom/fit), edit `viewport.rs`.
- To change layer rules (base locking, ordering, add/remove) or the per-layer transform math, edit
  `layers.rs` (`LayerTransform`, `local_to_world`/`world_to_local`/`world_corners`).
- To change move/rotate/scale behavior or the gizmo, edit `tools/transform.rs`.
- To change "select layer fully", edit `select_active_layer_fully` in `mod.rs`.
- To change the layers tree (rows, indent, collapse), edit `tree.rs` + `layers_panel_body` /
  `draw_group_row` / `draw_leaf_row` in `mod.rs`. To change grouping ops, edit `apply_group_op` +
  `persist::save_page_grouping`. To change reorder behavior, edit `build_unified_order` /
  `move_band_one` / `move_group_block`. To change multi-select, edit `select_row`.
- To add a tool, implement `PsTool` in `tools/` and register it in `PsEditorTabState::default`.
- To change brush painting or selection geometry, edit `tools/brush.rs` / `tools/select.rs` and
  `selection.rs`.
- To change the marquee look, edit `draw_dashed_marquee` (`mod.rs`) and `draw_dashed_preview`
  (`tools/select.rs`); the boundary loops come from `Selection::outline_loops`.
- To change the copy/cut menu or its compositing/cut rules, edit `draw_selection_menu`,
  `clip_op_submenu`, `clip_layer_picker`, and `clip_into_new_layer` in `mod.rs`.
- To change the raster effects pipeline (off-thread render, recenter, persist), edit
  `apply_effects_to_raster`, `render_ps_raster_effects`, `poll_ps_raster_effects_jobs`, and
  `apply_ps_raster_effects_result` in `mod.rs`.
- To change GPU upload/compositing, edit `layer_render.rs` and `mod.rs::draw_canvas`.
- To change how base layers are sourced, edit `page_loader.rs`.
