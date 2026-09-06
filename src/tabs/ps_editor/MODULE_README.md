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
map). `request_page` also threads a GATED legacy `text_images/` dir into the request so a never-migrated
legacy chapter's text (the doc is the PS editor's ONLY text source) is visible: it is `Some(text_images_dir)`
only when `migrate::manifest_has_inline_text(layers_dir)` is false, computed once per chapter and cached in
`doc_legacy_text_dir_cache` (keyed by the committed layers dir) so the GUI thread never re-parses
`layers.json` on a page switch; a migrated chapter passes `None` so a stale `text_images/text_info.json`
cannot resurrect a deleted overlay. `poll_loader` takes the shared doc lock ONLY to `insert_decoded_page` (a cheap move, never a
decode) — so the doc lock is never held across a multi-MB PNG decode and the GUI stays responsive. The
raster PNGs are decoded exactly ONCE (the former double decode — `load_persisted_into_stack` plus the
doc's own decode — is gone; `load_persisted_into_stack` was removed and the stack is built from the
doc by `sync_view_from_doc`). A worker decode failure leaves the page un-inserted; it still opens with
its two base layers. GPU texture creation / `render_cache` / `node_generations` reset stay on the GUI
thread (textures cannot be created off-thread).

The two base layers project existing shared state:
- `Исходник` (source) comes from `CleanOverlaysModel::cached_page_rgba` (worker-decoded, cached) and
  is **read-only** in every respect.
- `Клин` (clean) comes from `CleanOverlaysModel::overlay_rgba` (absent overlay → transparent) and is
  **read-write**: its pixels are editable, and every edit is written straight back to the shared
  model. See «The `Клин` write-back path» below.

Base layers are STRUCTURALLY locked — never deleted, reordered, grouped or transformed, always
page-sized with the identity `LayerTransform` — and both can be hidden. Pixel editability is a
separate axis (`Layer::can_edit_pixels`): `Исходник` is immutable, `Клин` accepts paint, cut and
merge-into. In the panel `Клин` is listed ABOVE `Исходник`, matching the composite. Any number of
user `Raster` layers stack above them. Raster layers may be **smaller than the page** ("incomplete") and carry an
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
are never part of the LAYER persistence (`persist_current_page` filters to `LayerKind::Raster`);
they project `src/` and `clean_layers/`, and a `Клин` edit is persisted by the shared clean-overlay
model's own autosave / save-to-project path instead. See `models/layer_model/` for the on-disk schema and the unified
layer-model roadmap (groups, text layers, effects, typing-tab sync).

## Files and submodules
- `mod.rs`: `PsEditorTabState` orchestration — the five dock tabs (see «Panels» below), canvas
  input routing, render-cache sync, the dashed selection marquee, the selection right-click
  copy/cut menu (`clip_into_new_layer`), layer/group save+load via `models::layer_model`,
  `merge_down` (`composite_to_page` flattens a raster onto the layer directly beneath it by unified
  BAND-Z — `merge_candidates_by_band_z` / `raster_below_by_band_z` / `raster_below_uid`, NOT the
  layer-stack neighbour, so a manual reorder merges the visually-below pair; the bottom-most raster
  merges INTO `Клин`, while `Исходник` is never a target and no base layer is ever the upper
  participant), and tab-local hotkeys
  (`B`/`M`/`L`/`V`, `Ctrl+D`, and undo/redo — the «Горячие клавиши» tab lists the same set,
  built next to `handle_hotkeys` so the two cannot drift).
  - **Unified layers panel** (the «Слои» dock tab → `layers_panel_body`): a Photoshop-like tree of
    compact rows (visibility eye + name + group indent), built each frame by `tree::build_unified_tree`
    into an owned snapshot so the render loop can mutate `panel_selection` without borrowing the
    stack. Collapsible/movable groups may mix rasters and texts; text overlays are interleaved by Z in
    the same tree (no separate bottom section). Multi-select: plain = replace, Ctrl/Cmd = toggle,
    Shift = range (`select_row`). A right-click menu groups the selection (`GroupOp` → `apply_group_op`
    → `persist::save_page_grouping`): create / move-to-existing / ungroup / delete. Per-row detail
    (opacity, fx, merge, delete, pin, rasterize) lives in the **active-layer controls strip** at the
    bottom (`draw_active_controls`, keyed on `panel_primary`). The body has NO scroll area and no
    hand-computed height reserve of its own: the dock already draws every tab body inside a bounded
    `ScrollArea::both` and sizes the panel from the CONTENT's measured height, so a nested
    fixed-height scroll area would fight that measurement. Reordering routes through the unified
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
- `selection.rs`: `Selection` binary page-sized mask, its tight `bounds`, and the boundary
  `outline_loops` the marquee is drawn from. Rectangle + polygon (lasso) geometry is combined
  through `SelectionOp` (`Replace`/`Add`/`Subtract`/`Intersect`, Photoshop's four modes) by
  `apply_rect` / `apply_polygon`; `set_rect` / `set_polygon` are the `Replace` wrappers. The
  loops are TRACED FROM THE MASK along pixel edges (a staircase on the integer grid, one closed
  loop per connected component and one per hole, collinear runs merged), NOT the raw input path —
  so the marquee states exactly which pixels are selected. Mask writes stay allocation-free: the
  "mask is zero outside `bounds`" invariant lets every op touch only the affected bbox, and
  `Intersect` clears the complement by streaming the spans `fill_polygon_spans` emits in
  increasing `y`/`sx` rather than building a second page-sized buffer.
- `tools/`: `PsTool` trait + context (carries the frame `ViewTransform`, the frame modifiers, and
  the Esc / Backspace edges); rectangle + lasso selection, the color brush (paints in layer-local
  space), and `transform.rs` (move/rotate/scale gizmo). `PsToolSection` groups the tools into the
  toolbar's Кисти / Выделение / Манипуляция sections at DRAW time — `self.tools` is never
  reordered, because `active_tool_idx` indexes it.
- `tree.rs`: pure builder for the unified layers panel. `build_unified_tree(stack, text_layers,
  bands)` joins raster layers + text overlays + groups into one `Vec<TreeItem>` (group headers +
  indented leaves) ordered top-to-bottom by the unified Z, with the same tiebreak as `draw_composite`
  so panel order == composite order. The two base leaves close the list (they are the composite's
  bottom) but are emitted in REVERSE stack order, so `Клин` sits above `Исходник` — the `LayerStack`
  vector itself is never reordered, because its raw order is load-bearing for `draw_composite`.
  A base leaf deliberately carries no `RowSel`: that is the STRUCTURAL lock that keeps delete /
  ▲▼ / grouping / the control strip away from base rows, rather than a guard a future edit could
  forget. A group is a maximal contiguous same-`group_uid` run (the
  contiguity invariant is enforced at write time in `persist::save_page_grouping`).
- `layer_render.rs`: `TiledTexture` — per-layer tile grid (sized to the layer image), dirty
  tracking, budgeted upload, and transform-aware mesh draw.
- `page_loader.rs`: background worker producing the two base-layer images for a page.
- `edit_op.rs`: undo/redo operations on the generic `ms-actions` engine. `PsEditOp` is a
  `ReversibleAction<Ctx = PsEditorTabState>` with four variants (real `match`, no `_ =>`, so every
  variant is handled everywhere): `RasterPixels` (brush stroke as a tiled+zstd `RasterDiff`, Part A),
  `CleanPixels` (the same delta against the `Клин` base layer, carrying NO uid — base-layer uids are
  regenerated on every page load, so the target is resolved by `LayerKind::Clean`),
  `LayerLifecycle` (add/delete a whole raster layer, retaining `Box<Layer>` + its `z` for re-add), and
  `FieldPatch` (one metadata/geometry field — `LayerFieldPatch::{Visibility,Opacity,Transform,Deform}`
  — carrying `before` + `after`). Pure, GUI-free cores unit-tested here: `apply_raster_diff_to_layer`
  (diff → `Layer.image` + `base_image` mirror), `copy_region_premul` (region-local buffer), and
  `apply_field_patch_to_layer` (drives a `Layer` field to a patch's `after`; also the no-doc fallback).

## Panels: five dock tabs over a full-area canvas
This tab hosts the app-owned panel dock (`src/widgets/panel_dock/`) and is its first NON-canvas
consumer. There are no static `egui::Panel`s: the canvas fills the whole program-tab area as the
BACKGROUND and five floating dock tabs sit over it.

| tab id (stable, non-localized) | caption key | body |
|---|---|---|
| `ps_editor.main` | `ps_editor.tab.main` | page switch, «Вписать» / «100%», zoom readout, load / effects spinners, `load_error`, and the «Панели…» menu |
| `ps_editor.tools` | `ps_editor.tab.tools` | the tool selector grouped by `PsToolSection::ORDER`, plus «Выделить слой полностью» / «Снять выделение» |
| `ps_editor.active_tool` | `ps_editor.tab.active_tool` | the active tool's `options_ui`, or `ps_editor.active_tool.no_options` when `PsTool::has_options` is false |
| `ps_editor.hotkeys` | `ps_editor.tab.hotkeys` | the active tool's `hotkey_rows`, then `ps_editor_common_hotkey_rows` |
| `ps_editor.layers` | `ps_editor.tab.layers` | `layers_panel_body` + `draw_active_controls` |

The tab ids are PERSISTENCE identities (they key the panel inside `user_config.json` and inside
`PanelDockState`), so they stay non-localized literals — a §A9 i18n exclusion
(`dev-docs/i18n_exclusions.md`).

**Default arrangement** (`ps_editor_default_dock_layout`, registered in
`app.rs::panel_dock_default_layout_builders`): two columns. Left — «PS редактор» on the viewport's left
edge, «Инструменты» below it, «Выбранный инструмент» below that. Right — «Слои» on the viewport's
right edge, «Горячие клавиши» below it. No size is pinned there; each tab declares its own
`min_size` / `initial_size` per frame. The builder must name EVERY tab this program tab can
declare: `panel_dock::persist` resolves stored tab keys against it.

**Visibility.** Four booleans, all defaulting to `true`, live in the MAIN tab's `TabExtras` bag
under `panels.tools` / `panels.active_tool` / `panels.hotkeys` / `panels.layers`. They are read off
the dock state BEFORE the tabs are declared (`PsEditorPanelVisibility::read`) — reading them from a
body would show a hidden panel for one frame — and written back from the main tab's body, which is
the only one declared with `show_with_extras`. That write is what raises `changed` → `dirty` → the
persistence write; no other machinery is involved. All five tabs are declared on EVERY frame
regardless: a hidden tab keeps its slot, a skipped declaration would lose it. «PS редактор» is never in
the menu — the menu lives in it.

**Frame order** (`PsEditorTabState::draw`, which takes the app's `&mut PanelDockState` as a lent-in
parameter): resolve ONE `area_rect` (used as both the `DockArea` rect and the canvas rect) →
`ensure_default_layout` → read the visibility flags → build `PsEditorDockCx` → declare the five
tabs → `dock.end` → store `PanelDockOutput::drawn_panels` rects into `panel_rects` and apply the
deferred `PanelActions` → draw the effects-editor window → draw the canvas LAST over `area_rect`.
The canvas still ends up UNDERNEATH: panels live on `Order::Foreground` areas while the canvas
paints into the `Ui`'s `Order::Background` layer and egui composites by layer order, not by call
order (`egui-docs/06-overlays.md`). Drawing it last is what lets the input gate use THIS frame's
panel rects instead of the previous frame's.

`PsEditorDockCx` lends `&mut PsEditorTabState` as a whole rather than disjoint field borrows: every
body needs the tab state, and the dock's own frame-long borrow is of the LENT-IN `PanelDockState`,
so the two are disjoint by construction. The layers body still defers its mutations through the
tab's existing `PanelActions`, applied after `dock.end`.

**Input gating.** `canvas_pointer_occluded` = an open popup, or `pointer_in_any_panel` over this
frame's `panel_rects`, or anything above `Order::Background` at the pointer. It gates `hovered`,
and through it the wheel, the zoom anchor and the routing to the active tool. Panning is
deliberately NOT gated: the gate answers where the pointer is right now, and a pan begun on bare
canvas must survive the pointer crossing a panel.

The same gate hides the ACTIVE TOOL'S CURSOR PREVIEW: `overlay_pointer` (pure, unit-tested) drops
`draw_overlay`'s pointer when it is occluded and `PsTool::gesture_in_flight` is false, on top of the
`rect.contains` filter it keeps. Gating input alone left the brush circle painted under a panel over
a canvas that refused the click. The in-flight term is asked of the TOOL and must never be replaced
by `input.primary_down`: a press that BEGINS on a panel holds the button down too and the tools
correctly refuse it, so that substitution reintroduces the bug in exactly the reported case.

**The hint grid is a deliberate local duplicate.** `draw_hotkey_rows_grid` re-implements the
canvas' `CanvasScene::draw_hint_rows_grid` in ~10 lines. That helper is private to the canvas impl,
this tab is not a canvas tab, and lifting it into a shared widget would be a refactor of another
subsystem for no gain here. The duplication is recorded here so it stays deliberate.

## Undo/redo (brush strokes + structural/metadata ops)
- The tab owns a per-page `ActionHistory<PsEditOp>` (`history`) bounded by `PS_EDITOR_UNDO_LIMIT`
  steps AND a compressed byte budget (`MemoryBudget::ps_editor_undo_bytes`). No live `MemoryProfile`
  handle is wired into this tab yet, so a fixed Medium-profile cap is used; a profile-driven budget is
  a follow-up.
- **Per-page-session scope**: the history is CLEARED on every page switch (`request_page`, after
  `persist_current_page`). A recorded diff is only valid while its page's layer image buffers are
  resident, and each page rebuilds the stack from scratch, so cross-page undo is intentionally not
  attempted here.
- **"Before" comes for free from `base_image`**: during a brush stroke `paint_line_color` mutates only
  `layer.image`; `base_image` keeps the pre-stroke pixels until the release commit. So at the commit
  site `record_brush_stroke` builds the reversible diff from `base_image` (before) vs `image` (after)
  over the stroke's accumulated dirty union (`brush_stroke_dirty`), avoiding any stroke-start snapshot
  of the (up to ~800×19000) ribbon image. `record` is observer-style (the forward edit was already
  applied live) — it never re-applies the paint.
- **Alpha convention**: diffs are built from and applied to the PREMULTIPLIED `Color32` bytes directly
  (`ColorImage::as_raw`/`as_raw_mut`), consistently for build and apply. No separate straight-alpha
  buffer exists here (unlike the clean-overlay model). The signed-delta round-trip is correct for any
  consistent RGBA8 buffer.
- Apply path (`apply_ps_raster_edit`): finds the resident raster by uid on the matching page, mutates
  `image` + `base_image`, marks only the touched `render_cache` tiles dirty, and routes the reverted
  pixels to the shared doc via `set_raster_pixels` (same path as forward edits) so cross-tab consumers
  and the next `sync_view_from_doc` reprojection agree.
- **Clean apply path** (`apply_ps_clean_edit`, for `PsEditOp::CleanPixels`): same shape, but the
  target is resolved by `LayerKind::Clean` and NEVER by uid — base-layer uids are regenerated on
  every page load — and the reverted pixels go back through `write_clean_region_to_model` instead of
  the doc, so an undo is as durable and as cross-tab-visible as the forward stroke.
  **Known asymmetry, accepted deliberately:** `replace_region` records its OWN reversible diff in
  `CleanOverlaysModel`'s cross-tab history, so a PS-side undo lands there as an additional FORWARD
  edit. State never diverges; the only oddity is that a later Ctrl+Z on the cleaning tab can re-apply
  what PS just reverted.
  The `history` field holds ops whose `Ctx` is the whole tab, so `undo`/`redo` use the clean-model
  take-and-restore idiom (`take_history` + unconditional restore) to avoid a self-borrow.
- **Direction convention** (uniform across variants): `apply` always drives toward the op's RECORDED
  end state — a `FieldPatch` applies its `after`; a `LayerLifecycle` realizes its `dir` (`Added` ⇒
  present, `Removed` ⇒ absent). `inverse()` swaps a `FieldPatch`'s before/after and flips a
  `LayerLifecycle`'s dir. So `record` pushes the forward op as-is (the mutation already happened live),
  `undo` runs `inverse()`, `redo` re-runs the original — matching the engine's Koharu-style contract.
- **Structural apply** (`apply_ps_layer_lifecycle`): `Added` rebuilds the doc node from the retained
  `Layer` (`pixels_dirty=true` so the pruned base PNG is rewritten; preserves the deform mesh) and
  `add_node_at_z`s it at the captured Z; `Removed` `remove_node`s by uid. Both update
  `deleted_raster_uids` (so the next persist drops/keeps the on-disk PNG) then `sync_view_from_doc`
  rebuilds the stack + prunes/creates the `render_cache` — the SAME projection the forward add/delete
  paths use.
- **Metadata apply** (`apply_ps_field_patch`): drives the doc setter
  (`set_visibility`/`set_opacity`/`set_transform`/`set_deform`) to `after` then `edit_doc_node`
  re-projects (no `render_cache` invalidation — these fields don't change pixels, compositing re-reads
  them each frame); falls back to `apply_field_patch_to_layer` on the stack when no doc page is
  resident.
- **Persistence tail** (`finish_history_step`): on a real change, calls `persist_current_page` (reads
  the reconciled `self.stack`, carries the explicit `removed_uids`, PNG encode off-thread) — NOT a bare
  doc enqueue — because a lifecycle delete/undo must drop or keep the on-disk raster correctly (the
  doc's own `enqueue_page_save` passes an empty removed set and would resurrect a deleted raster).
- **Recording sites** (observer style, `history.record` AFTER the live mutation; skip if unchanged):
  add-layer + delete-layer in `apply_panel_actions` (`LayerLifecycle`); visibility toggle there
  (`FieldPatch::Visibility`); opacity SLIDER recorded ONCE per drag gesture via `opacity_gesture`
  (snapshot pre-drag value on the first change, record on the first idle frame); transform/deform
  recorded ONCE per pointer gesture via `transform_gesture_before` / `deform_gesture_before` (snapshot
  at press, record at release if the same layer actually changed). All three gesture snapshots are
  cleared on page switch.
- **Not yet undoable** (deferred): cut/clip (including cut-from-`Клин`), merge-down (need a Batch op
  — a later part), base-layer visibility (view-only state, deliberately not recorded), and z-reorder
  / grouping (`move_band_one` / `apply_group_op` write the band order to disk synchronously with a
  "band order written LAST" requirement that the unified persist tail cannot reproduce without a
  dedicated per-op persistence hook). Rename has no UI, so no `LayerFieldPatch::Name`.
- Hotkeys (`handle_hotkeys`): Ctrl/Cmd+Z = undo, Ctrl/Cmd+Shift+Z or Ctrl/Cmd+Y = redo (respecting the
  existing focus early-return). `handle_hotkeys` takes `&ProjectData` so undo/redo can persist.

## Contracts and invariants
- GUI thread never decodes images or holds the model lock across decode: that is `page_loader`'s
  job; the model lock is released before `image::open`.
- **Default layer/group names are persisted, so they must stay non-localized literals.** A raster
  layer's `name` and a group's `name` round-trip through `layers.json` (`persist_current_page` →
  `saver::OwnedRasterLayer`/`GroupMeta`; reloaded by `sync_view_from_doc`). The default-name literals
  — `format!("Слой {n}")` (`layers.rs::add_raster_layer`), `format!("Группа {n}")`,
  `format!("Запечён: {name}")`, and the clip names `"Копия"`/`"Вырезка"` — are therefore left as
  stable Russian literals and NOT routed through the `t!`/`tf!` UI catalog (a §A-class i18n exclusion;
  see `dev-docs/i18n_exclusions.md` §A). Base-layer names (`"Исходник"`/`"Клин"`) are display-only —
  `persist_current_page` filters to `LayerKind::Raster` — so they ARE localized.
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
- **The `Клин` write-back path.** `LayerKind::Clean` is the ONLY base layer whose pixels change, and
  every such change goes through the single helper `write_clean_region_to_model(page_idx, x, y, w, h)`
  — used by the brush commit, the selection cut and `merge_down`, and again by undo/redo through
  `apply_ps_clean_edit`. Its obligations:
  1. Push only the given rect via `CleanOverlaysModel::replace_region`. A ribbon page can be
     ~800×19000 px, so the full buffer is never sent; the brush passes the stroke's accumulated
     `brush_stroke_dirty` union, the cut passes the selection bounds, and a merge passes the upper
     layer's page footprint (`page_footprint_rect`) — the only region a merge can change, because
     `composite_to_page` reproduces `Клин` unchanged wherever the upper layer samples transparent.
     That bound matters: the whole-page write it replaces meant two full-page RGBA copies plus a
     tiled zstd diff of both, on the GUI thread and under the model lock the autosave worker shares.
     An upper layer that genuinely covers the page still costs a whole page — inherent to the merge.
  2. The `chunk` is a raw crop of the premultiplied `Layer::image`; the model converts to straight
     RGBA itself.
  3. Write to the model FIRST; commit local state (`base_image` mirror, `pixels_dirty`) only after
     the model accepted. A rejected write must not leave the edit alive only in this tab's stack,
     where the next reload kills it unnoticed. Every failure path logs with page and rect.
  4. Re-establish `base_image == image` over the same rect — the undo "before" is read from
     `base_image`.
  5. Refuse the write when `CleanOverlaysModel::overlay_size(page_idx)` differs from the stack's
     page size: `replace_region` normalizes to the model's size and SCALES the chunk into it,
     landing a stroke misaligned. That legacy mismatch is the state `page_loader` answers with a
     transparent page-sized `Клин`, so it is reachable. `overlay_size` reports the REMEMBERED size
     rather than a materialized buffer's, so a detached page (no buffer, size still remembered and
     still driving the rescale) is covered too.
  6. Adopt `last_overlay_revision` from the model IN THE SAME LOCK SCOPE, but ONLY when it still
     equals the revision read immediately before our own `replace_region`. `sync_view_from_canvas`
     reloads the page whenever the model revision differs from ours, so adopting our own bump keeps
     a stroke from costing a redundant full-page reload — but adopting UNCONDITIONALLY also swallows
     any foreign bump that landed while this tab was away (the page manager's clean-attach worker
     writes from a thread), permanently suppressing the reload that would surface it. The tab
     resyncs only on tab entry, so that window is the whole session, not one frame — which is why
     `canvas::overlay_runtime::replace_overlay_region` can adopt unconditionally and this cannot.
     Declining to adopt never costs the user's work: the model already holds the write and the
     reload restores it from `overlay_rgba`.
  Disk persistence is NOT this tab's job: the app-level autosave worker and save-to-project own it.
  `Исходник` is never written back under any circumstance.
- Base-layer VISIBILITY is view-only session state: neither base layer is a doc node and neither is
  persisted (`persist_current_page` filters to `LayerKind::Raster`), so `apply_panel_actions` writes
  `Layer::visible` straight onto the stack — no doc route, no `FieldPatch` history entry.
  `draw_composite` honours the flag for base layers, which makes that sufficient.
- Selection copy/cut (`clip_into_new_layer`) composites the chosen layers bottom-to-top within the
  mask into a new raster layer **cropped to the selection bounds** and placed at the matching page
  position (so clip results are "incomplete", movable layers). Layers are sampled through their
  transforms (`sample_layer_world`), so partial/rotated/scaled sources contribute correctly. A
  **cut** also clears the selected pixels from every chosen layer **except** `LayerKind::Source`,
  which is immutable and can never be cut from; the `Clean` overlay and raster layers are cuttable.
  Cut-from-`Клин` routes its cleared region through `write_clean_region_to_model`, so it survives a
  page switch and reaches the other tabs — like the merge and unlike the raster path, it is not
  undoable.
- The selection is shown as a thin 1px dashed marquee in alternating black and white, drawn from
  `Selection::outline_loops` (no translucent fill, never blue), matching the in-progress drag
  preview in `tools/select.rs`. Because a traced loop is a pixel staircase whose segments are far
  shorter than one dash, the dash phase is carried along the whole loop by CUMULATIVE ARC LENGTH
  in screen space (`walk_dash_runs`) — dashing each segment on its own restarts the phase every
  pixel and degenerates into a solid line. Dash length is in screen pixels, so it is
  zoom-invariant; the marquee does not animate. "Выделить слой полностью" sets the selection to the active
  layer's footprint (page rect for a base/page-sized layer, transformed quad polygon otherwise);
  when the panel's primary row is a TEXT layer (not in `LayerStack`) it uses that overlay's
  `PsTextLayer::footprint_polygon` instead. Clicking any layer row also requests this selection so
  the marquee follows the active/primary layer immediately.
- **Tool-gesture lifecycle is the TAB's job, not the tool's.** A tool cannot see the frames it is
  not routed on, so the tab drives two hooks. `set_active_tool` is the ONLY writer of
  `active_tool_idx` and `reset()`s the outgoing tool; `reset_active_tool` does the same on a page
  switch (next to the selection/history clear) and on Esc while panning. Without this a pending
  lasso polygon outlives the switch and later commits over unrelated work, with a combination mode
  sampled minutes earlier and, across a page switch, in the previous page's coordinates. The other
  hook is `freeze()`, called on the frames routing is suppressed (space-pan, text drag): it lets the
  next routed frame treat the swallowed release as a finish instead of an interrupted drag, so
  panning mid-lasso keeps the outline. It arms only while a gesture is live and is consumed by the
  single frame that reads it, so a genuine interrupt (focus loss, another widget grabbing the
  button) still aborts.
- **A page selection is `Some` only when a pixel is actually set.** `PsToolContext::normalize_selection`
  (tools) and `set_selection` / `non_empty_selection` (tab) enforce it at every write. An all-zero
  mask is not a harmless nuisance: the marquee hides itself (`!any()`) while the brush, which only
  tests `Option::is_some`, silently refuses to paint until Ctrl+D.
- The transform tool (`tools/transform.rs`) mutates only the active raster layer's `LayerTransform`
  (no pixels), so it needs **no** tile re-upload — `draw` re-evaluates the transform each frame.
  Base layers are not transformable (`Layer::is_transformable`) — `Клин` included, even though it is
  paintable: the shared overlay model is page-aligned, so the layer keeps the identity transform.
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
  - Clean-overlay sync is TWO-way. Model → tab: `sync_view_from_canvas` compares
    `CleanOverlaysModel::revision` against `last_overlay_revision` and reloads the page (preserving
    raster layers) when it changed, so the `Клин` base layer reflects edits made on other tabs.
    Tab → model: `write_clean_region_to_model` (see the contracts section) pushes this tab's own
    `Клин` edits. That helper adopts the model's fresh revision in the same lock scope ONLY when no
    foreign bump happened in between (contract 6 above) — adopting it always would hide a background
    writer's edit from the reload for the rest of the session; adopting it never would reload the
    whole page after every stroke.

## Editing map
- To change the camera (pan/zoom/fit), edit `viewport.rs`.
- To change layer rules (base locking, ordering, add/remove) or the per-layer transform math, edit
  `layers.rs` (`LayerTransform`, `local_to_world`/`world_to_local`/`world_corners`). Which layers
  accept pixel edits is `Layer::can_edit_pixels`; which are structurally locked is
  `LayerKind::is_base` plus `remove_layer` / `reorder_rasters` / `set_layer_group` /
  `is_transformable`.
- To change how a `Клин` edit reaches the shared model (bounds, revision handling, `base_image`
  mirroring), edit the ONE helper `write_clean_region_to_model` in `mod.rs`. Its callers are the
  brush release commit, `perform_clip`, `merge_down` and `apply_ps_clean_edit`; do not add a fifth
  hand-written copy.
- To change which layer a merge targets, edit `merge_candidates_by_band_z` (it reserves band-Z 0 for
  `Клин` and shifts the rasters up by one) and `merge_down` in `mod.rs`.
- To change base-layer visibility handling, edit the `toggle_visible_raster` block in
  `apply_panel_actions`.
- To change move/rotate/scale behavior or the gizmo, edit `tools/transform.rs`.
- To change "select layer fully", edit `select_active_layer_fully` in `mod.rs`.
- To change which panels exist, where they start, or what the «Панели…» menu offers, edit
  `ps_editor_default_dock_layout` / `PsEditorPanelVisibility` / `ps_editor_panels_menu` in `mod.rs`
  (and register the builder in `app.rs::panel_dock_default_layout_builders`).
- To change what a panel SHOWS, edit its `*_tab_contents` method in `mod.rs`
  (`main_tab_contents` / `tools_tab_contents` / `active_tool_tab_contents` / `hotkeys_tab_contents`
  / `layers_panel_body`).
- To change the panel's row ORDER (including the `Клин`-above-`Исходник` base tail), edit
  `build_unified_tree` in `tree.rs` — never the `LayerStack` vector order.
- To change the layers tree (rows, indent, collapse), edit `tree.rs` + `layers_panel_body` /
  `draw_group_row` / `draw_leaf_row` in `mod.rs`. To change grouping ops, edit `apply_group_op` +
  `persist::save_page_grouping`. To change reorder behavior, edit `build_unified_order` /
  `move_band_one` / `move_group_block`. To change multi-select, edit `select_row`.
- To add a tool, implement `PsTool` in `tools/` and register it in `PsEditorTabState::default`.
- To change brush painting or selection geometry, edit `tools/brush.rs` / `tools/select.rs` and
  `selection.rs`.
- To change the marquee look, edit `walk_dash_runs` / `decimate_to_screen` /
  `draw_selection_marquee` (`mod.rs`) and `draw_dashed_preview` (`tools/select.rs`); the boundary
  loops come from `Selection::outline_loops`.
- To change the selection combination modes or the boundary tracer, edit `selection.rs`
  (`SelectionOp`, `apply_rect` / `apply_polygon`, the pixel-edge tracer). To change the lasso
  gesture (Photoshop modes at press, Alt straight segments, Esc / Backspace), edit
  `SelectTool::step` in `tools/select.rs`. To change the tool sections, edit `PsToolSection` /
  `PsToolId::section` (`tools/mod.rs`) and `tools_tab_contents` (`mod.rs`).
- To change a TAB-LEVEL shortcut, edit `handle_hotkeys` AND `ps_editor_common_hotkey_rows` next to
  it in `mod.rs` — they are deliberately adjacent so the dispatched keys and the displayed list
  cannot drift. A TOOL's shortcuts belong to its own `PsTool::hotkey_rows`.
- To change the copy/cut menu or its compositing/cut rules, edit `draw_selection_menu`,
  `clip_op_submenu`, `clip_layer_picker`, and `clip_into_new_layer` in `mod.rs`.
- To change the raster effects pipeline (off-thread render, recenter, persist), edit
  `apply_effects_to_raster`, `render_ps_raster_effects`, `poll_ps_raster_effects_jobs`, and
  `apply_ps_raster_effects_result` in `mod.rs`.
- To change GPU upload/compositing, edit `layer_render.rs` and `mod.rs::draw_canvas`.
- To change how base layers are sourced, edit `page_loader.rs`.
