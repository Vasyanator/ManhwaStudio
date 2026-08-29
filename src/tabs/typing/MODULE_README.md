# Module: src/tabs/typing

## Purpose
This directory implements the `Text` tab. It combines a read-only `CanvasView`,
text/image overlay placement, text rendering, overlay deformation, clipping masks,
auto-typing, import/export, and the floating panels used to create or edit text.

The module is a tab-level integration layer. It must keep long rendering, file I/O,
image decoding, export, mask filling, and auto-detection work off the GUI thread.

## Terminology
The two kinds of on-page objects have a stable naming convention that this document,
the code, and the user use slightly differently — they refer to the SAME things:

- **Text layer** (RU «текстовый слой») is the current user/UI-facing name for an
  editable text object. Historically these were called **text overlays**, and that
  is still the canonical name throughout this module's code and doc comments
  (`TypingTextOverlayLayer`, `TypingOverlayRuntime`, `self.overlays`, "text overlay"
  in comments). Treat "text overlay" and "text layer" as SYNONYMS: when the user says
  «текстовый слой», they mean an overlay. Image overlays are the same object kind
  carrying an image instead of text. The code name is not being renamed; only the
  user-facing wording moved from "overlay" to "layer".
- **Raster layer** (RU «растровый слой», `TypingRasterLayer`) is an imported/painted
  raster image layer. This name is the same in code, docs, and user speech — NOT an
  overlay (see "External images are raster layers, not overlays" below).

At the UI level the «Слои» (Layers) panel lists text layers, image layers, and raster
layers together as one unified, band-Z ordered list.

## Architecture
`TypingTabState` in `tab.rs` owns the tab runtime and implements the canvas extension
through `TypingHooks`. The canvas remains the common viewer/input surface; typing adds
extra page overlays, selection handles, deform tools, mask preview/input, and top-left
floating UI.

Every floating PANEL of this tab is a tab of the shared panel dock (`src/widgets/panel_dock`,
`dev-docs/dockable_panels_plan.md`). This tab does NOT own the dock state: the `PanelDockState` is
app-owned (`MangaApp::panel_dock`, one per studio window) and lent in for the frame through
`TypingDrawParams::panel_dock`, which the borrow checker sees as disjoint from `top_panel` /
`text_overlays` / `mask_layer` — the split `PanelDock::begin` needs. `tab.rs` passes that borrow
straight on to `CanvasDrawParams::panel_dock`, and the canvas hands it back as a parameter of
`TypingHooks::draw_canvas_overlay_top_left`, which runs `top_panel.begin_frame` → the dock →
`top_panel.end_frame`. One dock state per window is a hard constraint, not a preference:
sub-window `ViewportId`s are derived from a per-state index counter, and the persisted window list
is global (`src/widgets/panel_dock/MODULE_README.md`, «Sub-windows in the file»).

Nine tabs in seven default panels (`typing_default_dock_layout`), two columns. The first is not
this tab's own: `canvas.ribbon` is the CANVAS' controls tab, declared by all three canvas program
tabs through the one canvas-owned helper `canvas::declare_ribbon_tab` (this tab only tells it where
its dock context keeps the canvas and the page count), and its body, sizes and title live in
`src/canvas/`.

| tab id | caption | body | default panel |
|---|---|---|---|
| `canvas.ribbon` | «Лента» | `CanvasView::draw_ribbon_tab_body` | `#0`, at the dock area's left edge, content-sized |
| `typing.preview` | «Превью текста» | `TypingTopPanelState::draw_preview_tab_body` | `#1`, under panel `#0` and slightly right of it, pinned size |
| `typing.params` | «Параметры» | `…::draw_params_tab_body` | `#2`, on the dock area's right edge a little below its top, pinned size |
| `typing.effects` | «Эффекты» | `…::draw_effects_tab_body` | `#2` |
| `typing.actions` | «Действия» | `…::draw_actions_tab_body` | `#3`, under panel `#1`, pinned size |
| `typing.layers` | «Слои» | `TypingTextOverlayLayer::draw_layers_tab_body` | `#3` |
| `typing.mask` | «Маска обрезки» | `TypingMaskLayer::draw_mask_tab_body` | `#4`, under panel `#2` |
| `typing.deform` | «Режим деформации» | `TypingTextOverlayLayer::draw_deformation_tab_body` | `#5`, under panel `#3` |
| `typing.layout_editor` | «Редактирование раскладки» | `TypingTextOverlayLayer::draw_layout_editor_tab_body` | `#6`, under panel `#5` |

Which tabs are drawn is ONE pure rule set, `TypingDockTabVisibility::resolve`: «Превью текста» in
«Создание» mode, «Маска» while the mask editor is open, «Режим деформации» while an overlay is in
transform mode, «Редактирование раскладки» while that editor owns the canvas, and «Слои» everywhere
EXCEPT while it does (its body is a no-op then). A panel with no visible tab drops out of the
frame's chain and its dependants inherit its anchor, so each column closes over the hole — «Действия/
Слои» rises into the preview's place while editing, and the layout editor sits directly under it
because deformation and layout editing are never active together. The conditional panels are
anchored to DISTINCT targets for the same reason: the SOLVER is a total function and lays two
panels sharing target+edge+align on top of each other, so a hand-written default must chain them
instead — the docking GESTURE avoids that on its own (`drag::resolve_slot`), a default cannot.

The three unconditional panels carry a pinned `size_override` (`TYPING_DEFAULT_*_PANEL_SIZE_PX`),
transcribed from a hand-tuned arrangement; the conditional ones stay content-sized and are
therefore the first to give when a column does not fit the dock area.

Position, size, collapse and tab activation of every panel belong to the dock layout — no typing
state owns them. Because several bodies need `&mut TypingTopPanelState`, three need
`&mut TypingTextOverlayLayer`, one needs `&mut TypingMaskLayer` and «Лента» needs the hook's own
`&mut CanvasView`, they reach their state through `TypingDockCx`, the per-frame dock context, not
through captured borrows.

That layout is PERSISTED (`widgets::panel_dock::persist`), and the whole persistence cycle is
app-level now: `app.rs::restore_panel_dock` installs the stored arrangement before the first frame,
polls `PanelDockState::take_dirty_layouts` right after this tab draws and again in `on_exit`. What
this tab still owns is `typing_default_dock_layout`, handed to the app as a `fn` pointer: it is the
DICTIONARY of this tab's tab keys, so a new `TabId` must be added to it or the stored arrangement
will drop that tab on every load.

Any of these tabs can be pulled out into a real OS window (`widgets::panel_dock::window`). Such a
window is an immediate child viewport and therefore exists only while it is SHOWN every frame —
which `PanelDock::end` does only while a dock-hosting program tab draws. `MangaApp::ui` shows them
on every other frame (`show_idle_sub_windows`, gated on `tab_hosts_panel_dock`), so the user's
detached windows survive a tab switch (empty and grey, by the user's decision). Exactly one of the
two per frame, or the viewport would be shown twice in one pass.

Two deliberate behaviour changes came with the migration: the mask and deformation panels are now
movable and collapsible (they were fixed and always expanded), and the layout editor keeps one
user-controlled width instead of widening itself in Editing mode. A third came with the canvas'
own panel: «Превью текста» now hangs off the «Лента» PANEL (`PanelAnchor::Panel`) instead of the
dedicated canvas-controls anchor, which was removed with the panel it addressed. An arrangement
stored by an older build keeps that panel where it was: the retired stored anchor decodes to
`Free` at its stored position (`panel_dock/persist.rs`).

**Panels go in the dock; scene-anchored overlays and toasts do NOT.** The create-editor's
"идёт рендер" hint and its error/warning toasts (`tab/create_upload.rs`) stay plain
`egui::Area`s on `Order::Foreground`: they are transient, TTL-bound and anchored to the scene or to
the canvas' top centre, none of which a dock panel can express, and the dock is for panels by design
(`dev-docs/dockable_panels_plan.md` §1, non-goals). The same holds for the on-canvas handles and
guides. Only a surface a user would call a panel — a titled box with content they may want to move,
resize or collapse — becomes a dock tab.

The main data flow is:

1. `ProjectData` provides page paths. Text overlays (their `text_info.json` metadata and PNGs) now
   live in the chapter's `layers/` folder (saves stage to `*_unsaved/layers/`). The legacy
   `text_images/` folder is still read as a fallback so older chapters open and convert — their
   metadata migrates into `layers/` on the next save, while their PNGs keep being read from
   `text_images/`. Page masks (`mask.rs`) are a separate store and remain under `text_images/`.
2. INITIAL load of a legacy chapter reads `text_info.json` + referenced PNG files on worker threads,
   trying the unsaved `layers/`, committed `layers/`, then legacy `text_images/` dirs in order. Each
   overlay carries a stable `uid` (minted on creation or on first load). Legacy placement schemas are
   normalized up front by the SHARED codec `text_payload::migrate_overlay_entries` (absolute ribbon
   `x`/`y` via `project::LegacyRibbonGeometry`, top-left `u`/`v` via the PNG footprint) — IN MEMORY
   only; `text_info.json` is never rewritten. Persistence is owned by the shared `LayerDoc`: overlays
   become **text nodes** in `layers.json` with their FULL inline payload via the doc's text flush.
   Persistence is now OFF-THREAD: the placement autosave, `flush_text_layers` (save-to-project), and
   per-page text saves call `doc.enqueue_page_text_save` (the doc's background saver, coalescing PNG
   encode off-thread; sync-flush fallback when no saver). EXCEPTION: `flush_target_page_text_to_staging`
   (right before a raster-create worker reads the page's on-disk staging) stays SYNCHRONOUS — an async
   enqueue would race that read and resurrect a deleted-last-text overlay, and we cannot barrier on the
   GUI thread. `flush_text_layers` still returns the OWNED page set on a successful enqueue; the
   save-to-project merge worker barriers the saver before reading staging, so enqueued text is on disk
   first. `text_info.json` is READ-ONLY legacy and is ignored once the page has migrated to inline. **Text order is FULLY MANUAL** (auto-Y retired): every text is
   pinned-with-explicit-Z on one unified axis with rasters (text may sit BELOW a raster). Legacy
   `TextGroup`s are flattened into per-text bands ON READ by `layer_doc::ensure_page_loaded`, preserving
   the current page-Y visual order; the writers (`write_page_text_payload`) always emit text pinned and
   never create new groups; new text lands on TOP (`doc.add_node` → max Z + 1). Per-text ⬆/⬇ reorder
   routes through the doc + the shared `save_page_band_order`, exactly like the PS editor's band move, so
   a later flush never clobbers it (`merge_preserved_text_fields` keeps the pinned Z). Draw order,
   interaction, and export all sort by this unified band-Z (the old `overlay_stack_cmp` is gone).
   `sync_from_doc` is doc-authoritative for
   text: it reconciles-OR-CREATES — a doc Text node with no local `overlays` runtime is MATERIALIZED
   from the node (`text_runtime_from_doc_node`, mirroring PS's `sync_view_from_doc`). This is what makes
   a MIGRATED chapter (whose `text_info.json` is retired to `.bak`, so the legacy loader populates no
   runtimes) still show its text. The created runtime's rendered-PNG `file_name` is reconstructed
   deterministically via `persist::text_image_file_name(page, uid)` — the same name the doc text flush
   writes — so a later placement-save round-trips. Creation is additive (append), so existing overlay
   indices (`selected_overlay_idx`, the upload queue) stay valid across a sync. The legacy disk loader's
   COMPLETION (`poll_loader`) MERGES its decoded overlays into `self.overlays` by `(uid, page)` via
   `merge_loaded_overlays` (replace-in-place or append) rather than wholesale-replacing — otherwise a
   migrated chapter's empty load would WIPE the doc-created runtimes the instant the loader finishes (a
   timing race = the intermittent "text shows then vanishes"). Cross-chapter reset stays with
   `ensure_loader_started`, which clears `overlays` at the START of an open, so a switched-away chapter's
   overlays never linger; the merge only governs completion within one open.
3. The GUI thread uploads decoded overlay images to egui textures within a per-frame
   budget and draws them through the canvas hook layer. It also displays the unified **raster
   layers** interleaved with the overlays by band Z (`TypingRasterLayer` / `ensure_raster_layers_for_page`
   via `layer_model::persist::load_page_rasters`). Rasters are now **editable** in this tab, not
   read-only: `interact_page_rasters` adds canvas select + rotate drag, and hands a whole-layer MOVE
   (pointer drag as well as arrow-key nudge) to the shared move primitive in `tab/move_layer.rs` — the
   same primitive text overlays use, so both kinds and both input sources share every side effect (see
   "Layer move" below). Scale is `-`/`=`/`0`; Ctrl/Cmd+wheel ordinary rotation is
   `try_rotate_selected_raster_by_ctrl_wheel` — rasters have no vector rotation, so it always rotates
   `transform.rotation` regardless of `RotationCtrlWheelMode`.
   The raster selection is `selected_raster_idx` PLUS `selected_raster_page` (kept in lock-step: set
   together in `select_raster`, cleared together everywhere). The page pairing is REQUIRED because
   `draw_page_overlays` runs once per visible page — the per-page shortcut handlers (rotate/scale/nudge)
   guard on `selected_raster_page == Some(page_idx)` so one gesture only affects the raster on its own
   page, not the same bare index on other simultaneously-visible pages. EVERY raster geometry write
   from this tab is DEFERRED off the GUI thread: Ctrl+wheel rotation, keyboard scale, the image-panel
   transform controls and a settled move go through `persist_raster_transform_deferred`, and a settled
   move of a DEFORMED raster through `persist_raster_deform_deferred`. Both route the geometry to the
   doc live, then call `doc.enqueue_page_save` inline so the coalescing background saver and its later
   durability barriers cover the edit instead of performing a per-event synchronous manifest rewrite.
   The only remaining SYNCHRONOUS raster writer is `persist_raster_deform`
   (`persist::update_raster_geometry`), used by perspective transform mode's enter/reset menu actions
   and its handle-drag end — a separate gesture, out of the move primitive's scope.
   Selecting a raster opens the **same right-side edit panel that image
   overlays use** (scale + rotation + the effects cards, no text params): `selected_item_for_edit`
   builds an `Image`-kind `TypingSelectedOverlayForEdit` carrying a `TypingEditTarget::Raster{page,uid}`,
   and `queue_selected_overlay_edit_request` routes `ImageTransform`/`ImageEffects` to
   `apply_raster_transform_edit` / `apply_raster_effects_edit`. Raster effects are **non-destructive**:
   the worker (`render_raster_effects`) renders the chain from the ORIGINAL `base_file`, and
   `poll_raster_effects_jobs` persists via `doc.enqueue_raster_effects` (the off-thread effects-only
   saver path; writes a `_fx` PNG, keeps the base; sync `persist::update_raster_effects` fallback when
   no doc/saver), so effects survive a restart and removing them restores the original. One selection
   at a time across the two kinds (`selected_raster_idx` vs `selected_overlay_idx`, funnelled through
   `select_raster`, which also SETTLES an open move session, so a gesture interrupted by a selection
   change still lands its write). Panel transform edits persist through
   `persist_raster_transform_deferred` (doc + enqueued page save), never a synchronous manifest
   rewrite. A **right-click (ПКМ) canvas context menu** on a selected raster mirrors the text-overlay
   menu (`raster_context_menu` → deferred `apply_raster_menu_actions`). In normal mode the menu is
   attached to a response re-created EVERY frame (like text overlays / transform mode): the SELECTED
   raster's response is created unconditionally (id `("typing_raster", page, sel)`), so the menu stays
   open when the cursor leaves the layer and closes only on a click outside it; NON-selected rasters use
   a topmost hit-test (`topmost_raster_target`, which SKIPS the selected idx to avoid an egui duplicate
   Id) so a first right-click both selects the raster and opens its menu. Overlay tie-gating is preserved
   (`primary_pointer_targets_overlay_this_frame`): when an overlay claimed the pointer, the selected
   raster's response + menu are still created (so the menu persists) but its click/drag handling is
   skipped. **Unified click hit-test (text vs raster):** the raster interaction runs after the overlay
   pass, and egui awards the click to the later-registered widget, so a raster could steal a click that
   lands on a higher-Z text overlay. Before the raster interaction, the topmost overlay and topmost
   raster UNDER THE POINTER are resolved by unified band-Z (`topmost_overlay_at` / `topmost_raster_target`
   + `raster_band_z`), and `unified_topmost_pointer_target` (pure, overlay wins ties — text draws above a
   raster at the same band) decides the winner: if the overlay wins, the raster pass is gated out and the
   winning overlay is selected directly on a primary click (egui already routed the click to the raster);
   if the raster wins (text allowed BELOW a raster) the gate is not set so the raster takes it. Menu
   items: "Войти в режим трансформации"
   (perspective DEFORM mode — `ensure_raster_deform_mesh` seeds an identity grid from the affine
   transform if absent, `transform_mode_raster_idx` gates the canvas drag to edit the mesh's 4 corner
   handles via the shared `apply_perspective_corner_drag`, persisted by `persist_raster_deform` /
   `persist::update_raster_geometry`), paired "Выйти" / "Сбросить трансформацию" (`doc.set_deform(None)`);
   "Включить/Выключить обрезание маской" (raster mask-clip, **DEFAULT OFF** — `NodeBody::Raster.mask_clip`
   round-trips through `LayerRec.mask_clip`; `set_raster_mask_clip` bumps generation so
   `prepare_raster_mask_clips` re-clips via `mask_layer::clip_overlay_color_image_in_place` — which
   clips straight into a REUSED `ColorImage` buffer, because this path also runs every frame a
   mask-clipped raster is moved — and re-uploads);
   "Порядок" ▲▼ (`move_raster_in_unified_z` → the shared uid-based band-Z core `move_node_in_unified_z`,
   reused with the overlay reorder); "Удалить слой" (`remove_raster` → `doc.remove_node` +
   `flush_page_dropping_raster` so the deleted raster does not resurrect on disk). Everything routes
   through the shared doc; the PS tab sees it via the version watch. The LAYERS list is the «Слои» dock
   tab (`typing.layers`), sharing a panel with «Действия»; its body is
   `TypingTextOverlayLayer::draw_layers_tab_body(ui, page_idx)`, because the layer state lives on
   `text_overlays`. The «Слои» body is ONE unified,
   interleaved list of ALL the page's layers — text overlays, image overlays, AND rasters — ordered by
   unified band-Z DESCENDING (top first), with overlay-above-raster on a Z tie (`order_unified_layer_rows`,
   the canvas/hit-test tie-break). Every row has ⬆/⬇ moving it one step in the unified Z (overlay →
   `move_overlay_in_unified_z`, raster → `move_raster_in_unified_z`; both route through the shared doc band
   reorder so kinds interleave), at most one move per frame; clicking a row selects it (opening the
   right-side edit panel). The list WIDTH is whatever the dock panel gives it — the panel's own resize
   grip is the only width control, and the preview char budget is derived from `ui.available_width()`
   each frame (`preview_char_budget`, floor `LAYERS_PANEL_MIN_PREVIEW_CHARS` = 5). HEIGHT follows
   content, capped at `LAYERS_PANEL_DEFAULT_ROWS` (8) rows by the inner `ScrollArea::max_height` +
   `auto_shrink([false,true])` (a short list hugs; >8 rows scroll); `row_height` is derived from a
   measured galley, not a magic number. A text row's label is
   `Текст ({preview})` where
   `preview = text_preview_label(render_data_json.text_params.text, max_chars)` — the first `max_chars`
   Unicode chars + trailing dots brought to ≥3 (regular dot = 1, ellipsis `…` = 3, accounting for
   existing). `max_chars = preview_char_budget(panel_width − overhead, char_px) = max(5, fits)` GROWS with
   the panel width (wider → more chars before the dots, floor 5). `char_px`/`row_height` come from a
   measured `оооооооооо` galley (`ctx.fonts_mut(layout_no_wrap)`). Image rows show `Картинка`, rasters
   `🖼 {name}`.
   Cross-tab sync: both tabs hold the shared in-memory `LayerDoc` (`set_layer_doc`), which is the
   source of truth for per-page layer MODEL state. Edits route through it (`route_to_doc`), bumping its
   monotonic `version`; each frame `maybe_reproject_from_doc_version` re-projects the current page when
   the version advanced. (The old disk-revision counter / app bridge are gone.)

   **External images are raster layers**, not overlays: the "вставить/выбрать картинку" buttons now
   route through `request_create_image_overlay` → `render_and_store_created_raster` (worker) →
   `persist::add_page_raster` (a `kind:Raster` node + PNG), then the cache reloads and the new raster
   is selected. Existing `overlay_type:image` overlays are untouched (back-compat). DATA-SAFETY:
   `add_page_raster` seeds an unstaged page from the committed manifest (`ensure_page_staged`) so a
   typeset page keeps its text (drop fix); but committed is stale w.r.t. an in-session deletion of the
   page's LAST text (that empty page is skipped by the placement-save, so the deletion lives only in the
   doc). To avoid RE-SEEDING the deleted text, `request_create_image_overlay` first calls
   `flush_target_page_text_to_staging(page)` — flushing the doc's CURRENT text present-but-empty — so
   `ensure_page_staged` finds the page present and does not seed stale committed text.
4. Create/edit panel changes are converted to `TextRenderParams` and rendered by
   `render_next::render_text_to_image` in background workers. Fonts reach the renderer
   BY NAME, not by path: `TextRenderParams.font_name` and inline `<font=...>` tags are
   resolved through a caller-supplied `render_next::FontProvider`. That identity name is the
   font's identity (`FontEntry.identity_name`): the representative face's POSTSCRIPT NAME,
   suffixed with `%{16 hex of the content hash}` only when another file claims the same name
   with different bytes (byte-identical claimants merge into one entry instead, on the folder
   list AND on the combined folder+imported list), so each font keeps a distinct persisted
   identity that does not shift when another claimant appears or disappears. A PostScript name
   is only accepted when it is spec-valid; the `%` separator is a character the spec forbids,
   so a suffixed identity can never be some other font's real name. The user display-name is for SHOWING the font in combos/lists only, never
   persisted or sent to the renderer. Persisted `render_data.text_params` is SCHEMA 2 and carries
   the identity in exactly one key, `font`; legacy blobs (no `schema` key) that had a
   path/family/stem/label still resolve via the provider's READ-ONLY aliases and are converted
   on load (see "Persisted `text_params` schema" below). The typing tab OWNS font loading and
   builds that provider (`panel::TabFontProvider`, keyed PRIMARILY by the normalized identity
   with the bundled legacy spelling, each font's own `%hash` form, the bare contested name and
   family name/label/stem kept as legacy READ aliases, lazy file read + content-id cache). The create/edit panels
   each hold an `Arc<dyn FontProvider>` (rebuilt whenever the font list is (re)assigned);
   the tab layer refreshes its own copy each frame from the panel and captures an `Arc`
   into every render REQUEST struct so background threads resolve fonts without touching
   the panel. The PANEL ITSELF also keys on that identity now (selection, per-font profile
   memory, combo/editor/char-table/forms-metric caches); a file path survives only as the
   source of BYTES and inside the single legacy resolver that reads a reference persisted
   by an older build — see `panel/MODULE_README.md`, "The panel speaks IDENTITIES". `render_text_to_image(&params, &dyn FontProvider, cancel)` takes the provider
   as its middle argument. The advanced-form width metric resolves through the SAME provider
   (`render_next::load_font_content` over the resolved `FontContent`), so no path-keyed
   loader is left anywhere.
   Inline no-break tags (`<no-break>`/`<nobr>` or machine `<m j>`) are editing/form controls:
   the renderer strips them like other inline tags, while the advanced text-form picker applies
   them to the source text and writes a tag-free `formed_text` with protected ranges already kept
   together. Inline alignment tags (`<align=...>` or machine `<m a=...>`) are line-level style
   spans: the line whose start offset is inside the span uses that alignment for horizontal
   placement, while the control tag itself is stripped from rendered text.
5. Finished text or image overlays are appended to the runtime layer, written as PNGs
   in `text_images/`, and serialized back to `text_info.json`.
6. Export workers compose page source, shared clean overlay snapshots, text/image
   overlays, deform meshes, and optional typing masks into final page images
   (`flatten_typing_export_page_rgba`, shared by PNG and PSD). Export is GATED on full residency
   (Phase 2): the trigger defers dispatch behind the whole-project preload (see the preload contract
   below) so EVERY page's text is materialized before snapshotting. ORDERING: `request_export_to_folder`
   builds the text/image overlay snapshot (`build_export_overlay_snapshots`) AFTER the raster residency
   pass (`ensure_raster_layers_for_page` -> `sync_from_doc`), not before — building it earlier silently
   dropped the text of migrated/v3 pages the user never visited (their overlays materialize into
   `self.overlays` only on load). The `rasters_by_page` snapshot is built from the same fully-materialized
   projection. PS **raster layers are composited from an
   on-screen SNAPSHOT** (`TypingExportRasterSnapshot` taken from `raster_layers_by_page` at export time,
   carrying the post-effects display RGBA + transform/deform + band-Z), so the bake matches the canvas
   exactly; it falls back to a disk read of `layers.json` only when the job carries no snapshot. (A pure
   disk re-read silently DROPPED rasters whose `_fx.png` render or staging manifest was missing/stale.)
   Alpha note: `color_image_to_rgba` returns STRAIGHT (un-premultiplied) RGBA via `to_srgba_unmultiplied`
   — egui `Color32` is premultiplied, so `to_array()` would premultiply text TWICE and gray antialiased
   stroke edges. Every `source_rgba` consumer (display upload, mask-clip, effects, export composite)
   treats it as straight.

`panel.rs` owns the floating UI state and emits typed requests; it does not directly
mutate overlay storage. `mask.rs` owns typing-specific binary clip masks. `auto_typing.rs`
contains the image analysis used to center selected text over a detected bubble.
`render_next` is the production text renderer boundary for this module; it now lives in the
`ms-text-render` crate (`crates/ms-text-render`) and is re-exported here as
`crate::tabs::typing::render_next` via `mod.rs` (`pub use ms_text_render as render_next;`).
`segmentation` likewise comes from `ms-text-util` (re-exported in `mod.rs`).

Typing mask tile textures and text/image overlay display textures are reconstructable GPU caches.
The module exposes memory snapshots and eviction methods for those textures only. Persistent
`source_rgba`, placement metadata, deform meshes, and binary mask data remain resident for editing,
saving, and export.

## Files and submodules
- `mod.rs`: module wiring and public re-exports for `TypingTabState`, `TypingDrawParams`,
  `TypingTopPanelState`, and `TypingPanelLayout`, plus the `pub(crate)`
  `typing_default_dock_layout` the app hands to the shared dock state.
- `font_admin.rs`: the ONE sanctioned `pub(crate)` entry point for NON-typing code into the
  font MODEL. Wraps the `panel::{fonts, font_settings_store, fonts_data}` internals (which stay
  `pub(in crate::tabs::typing)`) as a narrow facade — font loaders, imported-fonts add/remove +
  revision, IDENTITY-keyed display-name overrides, VIRTUAL font group CRUD + membership/alias
  (config-only named font sets; members referenced by font IDENTITY on both sides of the facade),
  and `list_folder_group_names` (real `fonts/groups/` names, HEAVY/off-thread) — and
  re-exports `FontEntry` as an opaque type. For a BULK import (many fonts at once) it also
  exposes `locate_system_font_by_identity` (find an INSTALLED font by PostScript name →
  `SystemFontLocation { identity, path }`; BLOCKING, off-thread only), the batch mutators
  `add_imported_fonts` / `add_virtual_group_members` (ONE revision bump + ONE document write per
  batch, skipping what already exists and never overwriting an existing member alias) and the
  pure `is_valid_post_script_name` check. Used by the settings font-settings UI
  (`src/tabs/settings/typesetting/`); nothing else in typing is `pub(crate)` for it. Add a
  wrapper here rather than widening a panel internal.
- `tab.rs`: module root of the tab. Holds the data model (all `struct`/`enum`
  definitions incl. `TypingTabState`, `TypingTextOverlayLayer`, `TypingOverlayRuntime`,
  `TypingRasterLayer`, deform/export/create/edit/layout structs), the public
  `TypingTabState` facade + `Default`, the `impl CanvasHooks for TypingHooks`, and the
  `mod`/`use` wiring. The behavior (methods + free fns) lives in child submodules under
  `tab/`. All child modules are DESCENDANTS of `tab`, so they read the model's private
  fields directly; moved methods/free-fns are `pub(super)` (or `pub(in crate::tabs::typing)`
  when a typing-level sibling like `panel.rs`/`psd_export.rs` calls them).
- `tab/` submodules (each an `impl TypingTextOverlayLayer` method group and/or free-fn slab):
  - `doc_layers.rs`: shared `LayerDoc` sync, unified band-Z ordering, raster-layer projection, and the
    async whole-project page **preloader** (`all_pages_loaded` / `begin_preload_all_pages` /
    `preload_all_pages_active` / `preload_all_pages_progress` / `drive_page_preload`).
  - `render_jobs.rs`: background edit/create/raster/shape-variant render jobs, loader/migration start.
  - `persist.rs`: text placement save / staging flush / save-to-project (`flush_text_layers`).
  - `create_upload.rs`: create/shift-drag UI, text editor, status overlays, texture upload.
  - `selection_rasters.rs`: overlay/raster selection, remove, raster interact/menu/transform/deform and
    the NON-move raster drags (rotate, perspective corner handle) — a whole-layer move belongs to
    `move_layer.rs`.
    Also `resize_selected_overlay_width` (the on-canvas width-guide drag handle): it edits the selected
    text overlay's `text_params.width_px` and re-renders via the SAME `dispatch_vector_rerender` tail as
    Ctrl+wheel rotation (latest-wins re-render + render_data write-back + placement save), so canvas and
    edit-panel width stay in sync.
  - `panels.rs`: the deformation, «Слои» and layout-editor dock tab BODIES, plus the layout
    editor's lifecycle (enter/exit, re-render) and its on-page overlay. No panel of its own —
    the dock owns every frame, header and position.
  - `autotype.rs`: auto-typing hotkey trigger, job poll, result apply, debug visuals.
  - `draw_page.rs`: `draw_page_overlays` (master per-page draw) — takes the per-page `PageView`
    transform plus a `TypingPageInteractionPolicy` snapshot (mask/focus/eyedropper/auto-type/strict-pixel
    flags + `TypingCenteringAssistConfig`) built in the canvas hook before `text_overlays` is borrowed; its
    `ctx` comes from `ui.ctx()`. Plus repaint/visibility/pixel-snap and centering-assist helpers
    (`draw_centering_assist` takes a `CenteringMarker` + `PageView` + centering config).
  - `move_layer.rs`: the ONE whole-layer MOVE primitive — the move session's lifecycle
    (`begin_layer_move` / `drive_pointer_layer_move` / `add_keyboard_layer_move_step` /
    `settle_layer_move` / `drive_layer_move_settle`), the single arrow-nudge entry point for both
    layer kinds (`try_move_selected_layer_by_arrow_shortcuts`, guards included) and the mapping of a
    move onto the two geometry stores. Its pure math lives in `mesh_geometry.rs`; see the "Layer move"
    contract below.
  - `vector_transform.rs`: on-canvas VECTOR transform mode for text overlays (Phase 3a + 3b) — seeds a
    transient 13x13 working mesh over the overlay's oriented source-rect footprint, reuses the shared
    deform handles/brushes to edit it, and bakes the result into
    `render_data.text_params.raster_transform` via the background edit-render. The convert → inject →
    dispatch step is `inject_working_mesh_and_rerender` (shared by settle and the live path). The sharp
    warped re-render now fires LIVE during the drag: every frame the working mesh actually changes it
    dispatches the real edit-render (latest-wins via `edit_render_latest_token`, so superseded renders
    drop; the placement save coalesces behind the in-flight render), and `drag_stopped` does a final
    settle + `request_overlay_placement_save` for the persisted result. Phase 3b's LIVE GPU texture
    preview stays as the instant in-flight visual covering the sub-frame gap: it caches the overlay
    rendered WITHOUT its warp (the un-warped base) and, during a drag, textures that base onto the
    working mesh (`draw_textured_deform_mesh`) so the text bends in real time until the sharp PNG lands;
    the plain baked PNG is hidden for that overlay while the warped preview draws, and it falls back to
    the wireframe-only draw until the base is ready.
  - `mesh_geometry.rs`: deform-mesh/handle math, overlay geometry, hit-tests, unified-Z helpers and the
    layer-move "apply a total delta to a session base" math (pure fns).
    Owns `PageView` (`Copy` per-page page↔scene transform: `page_idx` + `image_rect` + `zoom`), the
    argument bundle threaded through the per-page draw/interaction/geometry helpers; its `page_size_px` /
    `scene_from_page_px` / `page_px_from_scene` methods wrap the same-named free fns (kept as the math
    source of truth). Re-exported at the typing-module level (`tab::PageView`) so `mask.rs` can name it.
  - `layout_editor.rs`: vector-line layout-editor free fns (frame/line hit-test, draw, conversions).
  - `render_store.rs`: create/edit/raster render-and-store workers, shape-variant grid/preview.
    Also the project's ONLY transparency checkerboard for text previews
    (`paint_shape_variant_checkerboard`, used by the shape-variant menu; `pub(super)`) and the
    ONLY luminance rule for judging rendered text against a backdrop:
    `shape_variant_luminance` (Rec.709 over white, 0..255) with its two-way form
    `use_dark_shape_variant_checkerboard`. The VALUE is `pub(in crate::tabs::typing)` because
    the sibling `panel` module picks one of three flat greys behind a local-preset row and
    must not grow a second luminance rule to do it.
  - `export.rs`: PNG/PSD export jobs + page composition/flatten free fns.
  - `codec.rs`: `render_data`/`TextRenderParams` parsers and overlay storage-entry normalize/parse.
  - `helpers.rs`: selection→page resolution, bubble/area seed text (incl. the `BubbleClass::Hint`
    exclusion predicates `is_hint_bubble` / `bubble_offers_create_text_header`), doc-node runtime,
    page-size/overlay disk loaders.
  - `geometry.rs`: small scalar/coordinate helpers (angle normalize, lerp).
  - `tests.rs`: `#[cfg(test)]` unit tests for the tab.
- `panel.rs`: module root of the top panel. Holds the data model (all `struct`/`enum`/`const`
  definitions incl. `TypingTopPanelState`, `TypingCreatePanelState`, effect cards, inline-tag
  types) plus the small `Default`/enum-helper impls, and the `mod`/`use` wiring. The behavior
  (the `impl TypingTopPanelState`/`impl TypingCreatePanelState` method groups and the free-fn
  slabs) lives in child submodules under `panel/`. Child modules are DESCENDANTS of `panel`, so
  they read the models' private fields directly; moved methods/free-fns are `pub(super)` (or
  `pub(in crate::tabs::typing)` for the `TypingTopPanelState` methods that `tab.rs` calls).
- `panel/` submodules:
  - `facade.rs`: whole `impl TypingTopPanelState` — public facade, the four dock tab BODIES
    (`draw_preview_tab_body`, `draw_params_tab_body`, `draw_effects_tab_body`,
    `draw_actions_tab_body`), request queues (`pub(in crate::tabs::typing)` for the methods `tab.rs`
    calls). The frame is bracketed by `begin_frame` (font upkeep, background-job polling, preview
    render pump) and `end_frame` (drains the settings deep-link a tab body raised); the dock runs
    between them, so every body sees this frame's polled results and every click it raised is
    consumed in the same frame. Panel position, size, collapse and tab activation belong to the dock
    layout, not to this module; `active_main_tab` is only a mirror of which of «Параметры»/«Эффекты»
    drew, needed by `emit_edit_request` to tell an image-effects edit from a pure transform.
  - `create_state.rs`: `TypingCreatePanelState` construction, focus/eyedropper tracking, font-group
    management and font-index lookup.
  - `create_render_data.rs`: render-data/effects/font-profile/shape-layout JSON building + profile sync.
  - `create_presets.rs`: global create-preset UI (apply, create, rename, save, delete) and the
    formula-preset apply/save UI, font-combo binding, face-index clamp.
    Also owns the preset SIDE of `fonts/presets.json`: the OFF-THREAD seed
    (`spawn_presets_seed` -> `read_presets_seed`), the deferred one-shot migration out of
    `user_config.TextTab.create_presets` (`finish_legacy_presets_migration` ->
    `migrate_legacy_presets`, which needs the font list to re-key legacy references) and the
    off-thread save (`run_presets_save`). Everything the background workers produce comes back
    as a `PresetStoreEvent` and is applied by ONE per-frame drain,
    `poll_preset_store_events` (seed install, migration, presets merged in from another app
    instance, save failure -> status line).
  - `local_presets.rs`: the LOCAL-PRESET parameter identity mode
    (`dev-docs/local_presets_plan.md`). Owns THE ownership dispatch every parameter edit
    funnels through (`store_current_params_snapshot`: font profile vs. selected local preset,
    crossed with whether a global preset is applied), the create/select/deselect/rename/delete
    operations behind the combo and name row, the LIVE-SET INVARIANT
    (`default_local_set_snapshot` / `park_default_local_set_for_global_preset` /
    `restore_default_local_set_after_deselect`: the live set is the selected GLOBAL preset's
    set when one is selected and the document-level DEFAULT set otherwise), the debounced
    off-thread persistence of that default set plus its clean/dirty generation rule and the
    app-exit flush, and the mode switch itself (persisted in
    `user_config.TextTab.param_identity_mode`). EDIT HERE for anything about local presets
    except their storage schema (`panel/presets_store.rs`) and their row previews
    (`local_preset_preview.rs`); the contracts — identity vs. index, the ownership matrix, the
    merge-by-id rule — are in `panel/MODULE_README.md`. Nothing here may touch
    `fonts_data.fonts.<identity>.profile` or `font_profiles_by_identity`: in this mode the
    font owns nothing.
  - `local_preset_preview.rs`: the off-GUI-thread preview renderer of the LOCAL PRESET combo
    (`dev-docs/local_presets_plan.md` §8). Owns `LocalPresetPreviewCache`: one long-lived
    `typing-local-preset-preview` worker thread, at most 4 renders outstanding, a
    least-recently-requested cache keyed by hash(35-char-capped name, preset profile JSON, row
    height), and the GUI-tick texture upload. Parameters come from
    `tab::codec::text_render_params_from_render_data`, so a preview is drawn by exactly the same
    parameter path (and full effect chain) as the canvas; only `text`, `text_wrap_mode`,
    `new_line_after_sentence` and `enable_inline_style_tags` are overridden, to force ONE line.
    The worker downscales to the row height before sending, so the egui atlas never holds a
    full-size render. A failed render (missing font) is a STATE, logged once; the row falls back
    to `preview_label(name)`. The font provider is NOT part of the key — a font reload must call
    `LocalPresetPreviewCache::clear`. Also owns `PreviewBackdrop` and the CONTRAST RULE that
    picks one of its three flat greys for a row from the preset's own colours (last visible
    outline first, main text colour second) — see `panel/MODULE_README.md` for the formula.
  - `create_sections.rs`: top-level section drawing (preview/params/effects/right actions) + effects_json.
  - `create_main_text.rs`: main text-param UI. The "Параметры" sub-tab is grouped into six
    collapsible sections (font / glyph metrics / layout & alignment / shape & smoothing / typeface
    style / text processing) drawn by the `pub(super)` free fn `collapsing_param_section`, followed by
    the unchanged advanced-params header. In the CREATE panel the font section opens with the
    PARAMETER IDENTITY MODE switch (above the font-group combo, which stays a plain filter) and,
    in `ParamIdentityMode::LocalPreset`, the local-preset combo with per-row preview images plus
    the rename/delete row — the row is DISABLED while nothing is selected. Also inline offset +
    alignment controls. The former
    left/right column split is gone; the non-stacked ("wide") path is dead (both call sites pass
    `stacked_columns = true`).
  - `create_advanced.rs`: advanced params — formula/shape layout, spacing, text accordion, and the
    advanced-form window (its BACKGROUND `forms::search_forms` job, the debounce/cancel state
    machine and the «Параметры поиска» knob section).
  - `create_edit.rs`: edit-mode params section + inline text-selection / inline-tag styling.
  - `create_apply.rs`: apply selected-overlay data, font selection, preview render queue/poll, render-param builders.
  - `text_forms.rs`: char/byte range conversions, advanced-form range-row + order + card (free fns).
    `order_advanced_forms` is the ranked search's presentation order (layer C: quality floor,
    line-count buckets, narrow lean, round-robin) and the window's ONLY ordering path — the
    legacy width-run comparator `sort_advanced_forms` is gone with the exhaustive enumeration.
  - `advanced_form_params.rs`: the eight user knobs of the advanced form search — ranges,
    defaults, the process-global runtime value, the `TextTab.advanced_form_search` JSON shape
    and the mapping onto `forms::FormSearchParams`. `pub(crate)` (re-exported by
    `tabs::typing`) because the startup seed and the config writer live outside typing.
  - `inline_tags.rs`: inline-tag machine/opening/closing build + parse, offset/stretch/color/align tokens (free fns).
  - `effect_cards.rs`: effect-card title, per-card control UI, preview-render worker spawner (free fns).
  - `fonts.rs`: font discovery/loading — folder fonts PLUS the imported system fonts
    (`load_fonts` / `build_combined_font_list` / `load_imported_system_font_rows`, the last
    of which resolves one stored entry: recorded path, else by NAME, else an unavailable
    row), duplicate merge (key: PostScript name +
    content hash, so byte-identical copies merge across differing FILE NAMES) and
    render-IDENTITY assignment (`assign_font_identity_names`: the representative face's
    PostScript name, `%hash`-suffixed on a same-name/different-bytes contest or on a claim of
    the reserved bundled-UI name), disambiguation, group listing
    (free fns), and VIRTUAL-group injection (`apply_virtual_groups`: folds the user-defined
    `fonts_data` virtual groups into a finalized list — membership into `FontEntry.groups`,
    per-group aliases into `FontEntry.virtual_group_aliases` — and returns the merged combobox
    group list; MUST run after merge/disambiguation/identity; see `panel/MODULE_README.md`).
    `load_system_fonts` (whole-OS enumeration) is the catalog source for the
    settings font-import picker (`src/tabs/settings/typesetting/font_settings.rs`, reached via
    the `font_admin` facade), run off the GUI thread; it also PUBLISHES what it enumerated as
    the process-wide `PostScript name → file` index (`SystemFontNameIndex`) that locates a
    moved imported system font by name.
    Coverage (`font_coverage`) is classified once per font at LOAD time (off the GUI thread) from the
    representative face's bytes against the current TYPESETTING language and cached on
    `FontEntry.coverage`; the dropdown never recomputes it. A runtime language change makes the cache
    stale, so `TypingTopPanelState::draw` compares `ms_text_util::language::text_language()` against a
    stored `coverage_language` and re-runs `spawn_font_reload` on both panels when it differs.
    Discovery also records each font's `original_name` (real family/name of the representative face,
    fallback post_script_name then file stem) for PSD export and future virtual fonts.
    Every font FILE is read and parsed EXACTLY ONCE (`fonts::read_font_file` -> `FontFileData`): one
    `fs::read` plus one `fontdb` parse yield the faces (each with its own `post_script_name`), the
    representative face's family name, the coverage and the content hash together, with the bytes
    shared into `fontdb` by `Arc` instead of copied. A file that reads but does not parse keeps a
    placeholder entry with empty names (folder fonts) or is skipped (imported system fonts).
  - `font_provider.rs`: `TabFontProvider`, the app-side `render_next::FontProvider`. Maps a normalized
    working name to a font — PRIMARY key is the font's IDENTITY (`identity_name`, its PostScript
    name), with the bundled legacy spelling, each font's own `%hash` form, the bare contested name
    (lowest content hash wins), the family name, the file stem and the display label kept as
    READ-ONLY legacy aliases (deterministic FIRST-wins; a display-name override is never a key;
    nothing writes an alias form any more). Reads bytes lazily OUTSIDE its lock and caches
    `Arc<Vec<u8>>` + content id, and carries each font's `original_name` to the renderer. Built from the
    panel's font list; shared (`Arc`) with background render threads.
  - `font_coverage.rs`: pure classification of a font's support for the selected TYPESETTING language
    (`ms_text_util::language::text_language()`, independent of the UI language) → `Full`/`Partial`/
    `Unsupported` via the swash charmap. Script base alphabet comes from the language's `ScriptGroup`
    (Cyrillic or Latin), language-specific letters + typography from the concrete `TextLanguage`.
    Drives the red/yellow font-dropdown highlight + hover tooltip in
    `create_presets::draw_font_combo` (per-row `primary_color` + `tooltip`). See `panel/MODULE_README.md` for the coverage/cache contract.
  - `presets_io.rs`: what still belongs to `user_config.json` — formula presets, per-effect-kind
    defaults, the legacy inline-tags flag — plus the formula/drawn/vector layout <-> `Value`
    conversions (free fns). Retains only the READ helper `load_text_tab_imported_system_fonts`
    for the one-time legacy migration (see `font_settings_store`); the imported-fonts WRITE path
    moved to `fonts_data.rs`, and the CREATE PRESETS moved to `presets_store.rs`.
  - `presets_store.rs`: the SINGLE owner of `fonts/presets.json` (version 2 — the version that
    added the identity mode, the per-preset local-preset sets and the document-level default
    set; a v1 document decodes as v2 with those at their defaults) — schema, typed
    `LoadOutcome`, quarantine, the atomic + crash-durable + optimistically-concurrent save with
    a TYPED error, the read of the legacy `user_config` payload and the deletion of the
    migrated `TextTab` keys. Preset NAMES are stored verbatim (never trimmed, so two names that
    differ only in spaces stay two presets). Holds no font knowledge: resolution lives in
    `create_presets`.
  - `doc_store.rs`: the ONE crash-safe write recipe (`write_atomic`: sibling temp + `write_all`
    + `sync_all` + CLOSE + `rename`, plus an optional parent-DIRECTORY fsync) and the ONE
    optimistic-concurrency vocabulary (`DocumentFingerprint` / `SaveBaseline`), shared by
    `fonts_data.rs` and `presets_store.rs`. Both used to carry their own copy, and the copies
    had drifted. `Durability::ContentsAndDirectory` is mandatory for any document whose
    previous home is DELETED once the write returned `Ok`.
  - `fonts_data.rs`: serde schema + disk I/O for the app-level per-font settings document
    `fonts/fonts_data.json` (`version: 2`: `system_fonts` = imported fonts by PostScript NAME with a
    `last_path` hint, `fonts` = per-font `display_name` override + default `profile` keyed by font
    IDENTITY, `virtual_groups` = named member sets keyed by identity; `sanitize_virtual_groups`
    cleans them on decode AND encode, and every unset field / empty collection is OMITTED). The
    path-keyed `version: 1` form is READ FOREVER and decoded verbatim with
    `FontsData.pending_migration` set (see `font_settings_store`); it is never written back. Load
    returns a typed `LoadOutcome` (a corrupt file is quarantined, never degraded to empty) and
    best-effort parses a newer version; save writes a full snapshot through the shared
    `doc_store::write_atomic` (contents-only durability — nothing deletes a source after it)
    and creates the fonts dir if missing.
    Independent of `FontEntry.label` — a display override never touches rendering.
  - `font_settings_store.rs`: single process-global runtime store backed by `fonts_data.json`
    (`OnceLock<RwLock<StoreState>>` = imported system fonts + per-font records + virtual groups +
    the pending-migration flag, plus ONE shared revision `AtomicU64`; the virtual-group mutators
    bump the same revision and persist only on a real change, exactly like the other mutators).
    EVERYTHING is keyed by font IDENTITY; a path survives only as `SystemFontRef::last_path`, the
    byte-source hint. Seeded at startup from `fonts_data.json`
    (`seed_imported_system_fonts_from_config`), or on first run migrates the legacy
    `TextTab.imported_system_fonts` list once (never written again; the key itself is deleted
    later, by `presets_store::drop_migrated_user_config_keys`, and only against CONTENT proof
    that `fonts_data.json` took the list over).
    `add_/remove_imported_system_font` and `set_font_display_name_override` mutate state, bump the
    SAME revision, and persist the whole snapshot off the GUI thread via `fonts_data::save`;
    the BATCH forms (`add_imported_system_fonts` / `add_virtual_group_members`) apply a whole
    slice under ONE write lock with ONE bump and ONE persist, and bump nothing when they added
    nothing;
    `set_font_profile` writes the font's DEFAULT parameter profile through a DEBOUNCED writer and
    does NOT bump the revision (a profile changes on every parameter edit). `migrate_legacy_font_keys`
    performs the DEFERRED v1 re-key — see `panel/MODULE_README.md`. Seeding does not bump the revision. The
    create/edit panels watch the revision to reload their font lists; the settings font UI
    (via `font_admin`) drives the mutators. The font-administration UI itself
    (`FontSettingsEditorState`, the per-font properties window) lives OUTSIDE this module, in
    `src/tabs/settings/typesetting/`; only the MODEL is here.
  - `ui_helpers.rs`: per-FORM font matchers (identity first, then the READ-only legacy
    family/label/stem/`%hash` aliases and `font_matches_path`, which only
    `create_state::find_font_idx_by_legacy_reference` may call), group membership,
    wheel-scroll, param rows, enum cyclers/parsers, Value readers (free fns). The generic egui font-family binding/registration helpers moved to `crate::widgets::font_preview`.
  - `effect_parse.rs`: `parse_effect_cards` (free fn).
  - `effect_defaults.rs`: user-configurable DEFAULT parameters per effect kind. Owns a
    runtime-global `OnceLock<RwLock<HashMap<discriminator, Value>>>` store (seeded at
    startup from `TextTab.effect_defaults` via `seed_effect_defaults_from_config`),
    resolves the add-time default card (`effect_default_card`, consulted in
    `create_sections`), and provides the `EffectDefaultsEditorState` editor widget
    rendered by the settings pane. Per-card (de)serialization reuses the shared
    `effect_card_to_value` (`effect_cards.rs`) / `parse_effect_cards` codec; persistence
    reuses `presets_io::{load,save}_text_tab_effect_defaults` (off-GUI-thread saves).
  - `color_presets_store.rs`: the SINGLE owner of the title-scoped color-preset document
    (`{title_dir}/color_presets.json`, `ProjectPaths::color_presets_file`). Owns the 20 cells
    every color picker of this tab offers, loads them in the background and persists a
    confirmed cell edit through the shared `char_table::SnapshotWriter`. The set itself lives
    in `TypingTopPanelState` — ONE per tab, so the create and the edit panel cannot drift
    apart — and is handed to the drawing code as a `ColorPresetsBinding` (`panel.rs`).
  - `char_table/`: the «Таблица символов» symbol picker — `CharTableState` (tabs, cell
    size, expansion, the two favorite lists, the background glyph-coverage job) plus its
    `egui::Window` in `char_table/window.rs`. Opened from the "Изначальный текст"
    accordion header of the EDIT panel; a click inserts the character at the stored caret
    of the active text buffer, inline-tagged with `<font=...>` only when the picked font
    differs from the base one. Has its own `MODULE_README.md` — read it before touching
    the coverage job, the favorites documents, or the font-registration throttle.
  - `tests.rs`: `#[cfg(test)]` unit tests for the panel.
- `mask.rs`: per-page binary clipping masks stored as `mask_page_{idx}.png`,
  tiled mask preview textures, brush/fill editing, async loading/saving, and export
  snapshots. Its UI is the `typing.mask` dock tab body (`draw_mask_tab_body`); the
  status line's TTL is ticked every frame by `expire_status_error`, since the body only
  runs while the panel is open.
- `auto_typing.rs`: optical center computation for rendered overlays and region-growing
  bubble detection from the shared composited page cache.
- `rotation_ctrl_wheel.rs`: app-wide runtime-global (`RotationCtrlWheelMode` Vector/Raster,
  default Vector) selecting how the Ctrl+wheel gesture rotates a selected overlay. Config-free;
  seeded at startup from `TextTab.rotation_ctrl_wheel_mode`, written by the settings "Тайп" pane,
  read by the overlay Ctrl+wheel handler in `tab/selection_rasters.rs`. `pub mod` so settings can
  reach it. Only text-overlay rotation consults the mode; raster Ctrl+wheel rotation
  (`try_rotate_selected_raster_by_ctrl_wheel`) ignores it and always uses ordinary rotation.
- `render_next`: text rendering subsystem, now the `ms-text-render` crate re-exported as
  `render_next` (via `mod.rs`). Its public contract is `render_next::types::*` plus
  `render_next::render_text_to_image`; callers in this directory should treat its layout,
  wrap, raster, formula, and effects modules as renderer internals.
- `segmentation`: re-exported from the `ms-text-util` crate (line/unit segmentation used by
  the renderer's wrap path and the panel's form preview).

## Contracts and invariants
- GUI code must not block on rendering, file I/O, image decode, mask save/load, mask
  flood fill, export, or auto-typing detection. Use worker threads and poll receivers
  from the frame loop.
- **Every surface of this tab that shows CHAPTER TEXT with an egui font must offer that
  string to `ui_fonts::ensure_covers` first.** The UI chain only carries `fonts/ui/core`
  until something asks for more, so a rare script (Arabic, Thai, Hebrew, …) renders as tofu
  in the panel while the rendered page — which uses the renderer's own fallback chain — is
  correct. One call per surface, where the string is already assembled:
  `tab/create_upload.rs::draw_text_editor` (the overlay-creation field),
  `panel/create_advanced.rs::draw_text_accordion` (the source/formed editor, on whichever
  buffer the accordion is showing) and `tab/panels.rs::draw_layers_tab_body` (the per-row
  text preview). The create-preview panel needs no call: it shows a rendered IMAGE, not
  egui text. The call is idempotent, must run on the GUI thread inside a frame, and does its
  work on a worker thread — see `src/ui_fonts.rs`.
- **An egui resource this tab registers is NAMED BY ITS CONTENT, never by an instance counter.**
  A project reload (structural page-manager op, «Перезагрузить проект») rebuilds the whole
  `MangaApp` — every tab state with it — inside the SAME `egui::Context`, and `Context::add_font`
  keeps the FIRST registration of a name without comparing bytes. A sequence number therefore
  re-issues a name the context already holds foreign bytes for. The create editor's own-typeface
  family is `tab/create_upload.rs::editor_font_family_name(identity, content id, face)`, a pure
  hash of its key; `widgets::font_preview::combo_font_family_name` is the same pattern for the
  panel combos (a distinct prefix — the two content discriminants are different quantities).
- **Layer move: ONE primitive, two input sources, two layer kinds** (`tab/move_layer.rs`). Translating
  a layer — text/image overlay or raster, by pointer drag or by arrow keys — is a single *move session*
  (`TypingLayerMoveSession`, at most one open). Nothing else may move a layer: rotation, deform-handle,
  brush and vector-warp drags are NOT moves and keep their own drag states and settle blocks.
  - **The delta is applied to the session BASE, never incrementally to the live geometry.** The base is
    the layer's geometry snapshotted at gesture start (the deform grid when the layer has one — both
    kinds RENDER from the mesh when present — else the affine center); a pointer frame RECOMPUTES the
    total delta from the press position, a key press ADDS its step to it. Meshes move RIGIDLY
    (`translate_rigid`). This is what makes the gesture idempotent: a held arrow at the page bound
    cannot cumulatively squash a mesh, and a move out to the bound and back returns the exact original
    geometry. Boundary POLICY is unchanged — a layer may hang off the page (`clamp_page_point` at ±0.9
    page) and is clipped there.
  - **The whole-pixel snap of the base runs on the FIRST real displacement, not on the press.** A click
    that never moves the pointer must change no geometry and must not mark the project edited; the
    session's `has_changes` gates the settle, so such a click leaves no trace at all.
  - **A keyboard gesture ends by HELD keys, not by key events.** `drive_layer_move_settle` settles a
    `Keyboard` session on the first frame where `key_down` reports no arrow held (level-triggered, so
    OS key-repeat gaps do not split one hold into many gestures, and egui's focus-loss clearing of
    `keys_down` ends a session left open by an alt-tab). A `Pointer` session settles when the primary
    button is up. `wants_repaint()` includes `move_session.is_some()`, or the release frame might never
    be drawn and the write would strand.
  - **Exactly ONE settle site**: `TypingTabState::draw` calls `drive_layer_move_settle(ctx)` once per
    frame AFTER `canvas.draw`, immediately before `drive_placement_save_debounce` — same reasoning as
    the debounce tick, since the move is applied inside `canvas.draw`'s callees. Settling elsewhere is
    limited to the interruptions that would otherwise drop the write: `clear_selection`,
    `select_raster`, `remove_overlay`, `remove_raster`, and — load-bearing — the WHOLE-DOCUMENT flush
    `flush_text_layers`, which settles FIRST (before `sync_overlay_state_into_doc`, so a moved text
    layer's geometry is in what gets pushed, and before the doc flush, so a moved raster's geometry
    has reached the document). It must sit there and not only in the `flush_text_layers_if_dirty`
    wrapper, because `MangaApp`'s page operations and save-to-project call `flush_text_layers`
    DIRECTLY — and a raster move lives nowhere but this tab's runtime projection until it settles, so
    a page op would reload over it and a save would merge without it, both reporting success. The
    wrapper keeps its own settle too, since it decides whether to flush at all
    (`has_pending_placement_save`) BEFORE calling it.
    `draw` stops the moment the tab is left, so a gesture still open then would otherwise be invisible
    to every dirty check and lost silently; for the same reason `has_pending_text_edits` counts an open
    session with changes (`has_unsettled_layer_move`). The in-session flush points (selection / page
    change / idle debounce) must NOT settle — the pointer or arrow may still be down, and ending the
    session under a live gesture would freeze it for the rest of the press. The DISCARD path
    (`discard_pending_placement_save`) DROPS the session instead of settling it, per the discard rule
    below.
  - **Per-frame effects are live; the disk write is deferred and happens once per GESTURE.** During the
    gesture the primitive invalidates the moved layer's mask clip (overlay: geometry-changed mark;
    raster: `clipped_image = None`, no doc round-trip) and re-runs the overlay visibility limit. On
    settle an overlay re-binds its centering frame, flushes a stale texture and only
    `mark_placement_save_dirty`s; a raster dispatches exactly one `persist_raster_transform_deferred`
    or `persist_raster_deform_deferred` for the whole gesture. No move performs a synchronous
    `layers.json` write on the GUI thread. Both deferred persists RETURN a `RasterPersistDispatch`
    for the same reason `request_overlay_placement_save` does: the settle retires the gesture, so it
    may not read an unscheduled write as success. Two things decide that verdict, and BOTH are
    load-bearing: `route_to_doc`'s result (`false` = no document, or the page is not resident / the
    node is gone) and the enqueue's result — `enqueue_page_save` returns `Ok(())` even for a page the
    document does not hold, so skipping the first check reports a write that would save nothing as
    `Enqueued`. `NotEnqueued` carries a `RasterPersistFailure` because the two classes need opposite
    handling: `NotWired` (no layers dir / no document / poisoned lock / document rejected it) is
    logged and NOT retried, since every later attempt reproduces it into a loop; `WriteFailed` (the
    enqueue's synchronous `flush_page` fallback failed — a locked file, an antivirus hold, a momentary
    permission error) is possibly transient, so the page joins `raster_save_retry_pages`, is surfaced
    to the user (`typing.status.raster_geometry_save_failed`, parked in `pending_status_error` and
    published by the next `drive_layer_move_settle`, which has a `Context`), and is retried by
    `retry_failed_raster_page_saves` at FLUSH POINTS only — never per frame. A queue that can never
    drain (the tab is no longer wired to a chapter) is dropped loudly instead of carried.
  - **Guards run before any key is consumed**, so a rejected gesture leaves the arrows to their real
    owner: any focused widget (`panel_text_input_focused` / `egui_wants_keyboard_input`), a text
    overlay in VECTOR transform mode (moving it would invalidate the normalized warp points) and a
    raster in perspective transform mode. Each kind's arrow rule mirrors that kind's mouse rule — which
    is why a text overlay in RASTER transform mode is still movable by arrows.
  - **A layer NEVER changes page by being dragged.** The cross-page drag transition no longer exists;
    dragging past a page boundary clamps within the layer's own page.
  - An open session is index-bookkeeping-aware like the drag states: `sync_from_doc` remaps a raster
    session by uid across a reproject, the per-page bounds checks drop a session whose layer is gone,
    and the raster interaction treats an open session as an active gesture (it owns the pointer, so it
    neither loses the topmost-by-Z gate nor reads as a click on empty canvas).
  - **A reprojection RE-APPLIES an open session** (`reapply_layer_move_after_reproject`, the last step
    of `sync_from_doc`). Gesture geometry reaches the document only on settle, so a rebuild from the
    document restores the PRE-gesture state; a pointer session self-heals only on the next frame the
    pointer moves, and a keyboard session never does, so the move would be reverted and the settle
    would persist the revert. Re-applying `base + delta` is exactly idempotent and therefore free when
    the reprojection did not disturb the layer. The visibility limit is not re-run there (no `PageView`
    at a doc sync); the next interaction frame covers it.
  - **Known boundary behaviour of the shared rigid translation.** `translate_rigid` (and therefore
    every mesh move) yields ZERO on an axis where the mesh's control-point box is already wider than
    the page's allowed span (2.8 sides) — no translation can make it fit. The rule is shared with the
    centering-assist reconciliation, whose one-step convergence proof depends on it. Reading a raster's
    stored `DeformRec` as a move base also NORMALIZES its points into that band (every mesh in this tab
    already satisfies it, `TypingOverlayDeformMesh::new` being the only constructor), and since a move
    now writes the mesh back, that normalization is persisted by the first move of a mesh authored
    elsewhere. Under strict pixel movement the whole-pixel snap runs AFTER the page clamp, so a layer
    driven into the bound still rests on the grid — except exactly at the bound, whose coordinate is
    fractional and where feasibility wins.
- **Text-layer EDIT writes are DEFERRED; STRUCTURAL writes stay EAGER.** An edit (placement, geometry,
  mask-clip toggle, render-data) only calls `mark_placement_save_dirty` (`tab/persist.rs`), which
  writes nothing. The write happens at a FLUSH POINT. This is what stops a drag from spawning a save
  worker every frame — the worst offender was `vector_transform::dispatch_vector_rerender`, reached per
  drag frame from `draw_page.rs` via `resize_selected_overlay_width`. Deferral is safe because
  durability never came from the individual writes in the first place: it comes from the barriers (see
  `README_AGENT.md`, "Что важно не ломать").
  **A flush point may retire its dirty state ONLY once a write is genuinely dispatched.** Both writers
  report that, and neither may be assumed to have written: `request_overlay_placement_save` returns
  `PlacementSaveDispatch` (`Started`/`Parked` = the pipeline owns the write; `NotWired` = nothing was
  or will be written), and `flush_text_layers` returns `Result<TypingTextFlushOutcome,
  TypingTextFlushError>` (an `Err` could not run; an `Ok` with an empty `owned_pages` legitimately ran
  with no resident pages). Clearing before the dispatch made a failed one look saved forever — the
  debounce stops arming its repaint, `has_pending_placement_save` goes false, and tab-leave/exit stop
  retrying, which at exit is silent data loss.
  The `Err` vs `Ok`-with-empty-`owned_pages` distinction binds `app.rs` too, and in BOTH of its eager
  callers an unverified set means ABORT, never proceed: save-to-project would otherwise let the merge
  preserve stale committed text, delete the staging dir, and report success, and a page operation would
  remap the page-keyed trees without the pending edits. See `README_AGENT.md`. One asymmetry belongs
  here: `NoLayersDir`/`NoLayerDoc` mean the store was never wired, because `ensure_loader_started`
  wires it on the tab's FIRST DRAW. A session that never opened the Text tab therefore gets `Err` from
  a tab that owes nothing — which is why `app.rs`'s page-op gate treats those variants as "quiesced
  unless an edit is actually pending" instead of aborting on any `Err`.
  `has_pending_placement_save` covers THREE axes — `placement_save_dirty`, `edit_render_data_dirty`,
  and `save_requested_while_busy` (a flush that could only PARK behind an in-flight render has not
  written yet, so exit/tab-leave must still see it). `clear_placement_save_dirty` retires the first two
  only: the parked flag is the sole record of a re-fire that `poll_save_jobs` /
  `poll_edit_overlay_jobs` own. The flush points:
  - **Selection change** — `flush_edit_save_on_selection_change` (`tab/selection_rasters.rs`), the
    primary focus loss. Observes BOTH selection axes (overlay AND raster `(page, idx)`), so an
    overlay→raster switch counts; the first selection of a session only seeds the trackers.
  - **Page change** — `flush_placement_save_on_page_change`, driven per frame from `TypingTabState::draw`
    off `canvas.current_page_idx()`. The canvas is a continuous scroll strip, so this fires per page
    crossed (cheap and desirable). `last_page_idx` starts/resets to `None` so a freshly loaded chapter
    seeds instead of flushing.
  - **Idle debounce** — `drive_placement_save_debounce`, `PLACEMENT_SAVE_IDLE_DEBOUNCE_SECS` (1.5 s) of
    no further edits. The safety net for an edit that never loses focus, so a walk-away or a crash is
    recoverable from the `_unsaved` staging dir. Its `ctx.request_repaint_after` is LOAD-BEARING: egui
    draws no frames while idle, so without it the deadline is unreachable and the write never happens.
    It is driven at the END of `TypingTabState::draw`, AFTER `canvas.draw`, and must stay there: nearly
    every `mark_placement_save_dirty` lives inside `canvas.draw`'s callees, so an earlier tick observes
    a mark only on the NEXT frame — which strands the write entirely when the marking frame is the last
    one drawn (no repaint armed), and otherwise doubles the delay to ~2 windows across two wakeups.
  - **Tab leave / app exit** — `TypingTabState::flush_text_layers_if_dirty`, called by `app.rs` (the
    tab cannot see either event: both stop its `draw`). The EXIT flush is REQUIRED and must run BEFORE
    `on_exit`'s layer-saver barrier — see the quiescence note below.
  - Eager, deliberately NOT deferred: `remove_overlay` / `remove_raster` (anti-resurrection durability
    must not depend on reaching a flush point; both go through `dispatch_structural_placement_save`),
    the internal `save_requested_while_busy` re-fire, `flush_target_page_text_to_staging`,
    `save_page_band_order`, and `exit_layout_editor` (leaving the editor IS a focus loss, so it flushes
    rather than defers).
  - NOT a flush point at all — the DISCARD path (`app.rs::start_exit_cleanup`) calls
    `TypingTabState::discard_pending_text_edits`, which DROPS everything unwritten, including the
    parked re-fire. Discard deletes the staging dir and shuts the saver down, so any surviving pending
    write would be re-dispatched into `enqueue_page_text_save`'s sync fallback and re-create that dir
    with the edits the user threw away; a surviving `has_pending_text_edits` would also re-latch
    `app.rs`'s unsaved cache and re-open the exit dialog.
  The decision core `placement_save_debounce_tick` (`tab.rs`) is a pure fn and is unit-tested;
  `mark_placement_save_dirty` clears the window seed and the next frame re-seeds it, which is how a
  re-mark restarts the window without any edit site needing a clock.
- Quiescence: the detached `spawn_overlay_placement_save` writer has no explicit quiescence handle. The
  layer-saver barrier covers shared-document writes, but a placement worker already detached before a
  page operation cannot be joined directly. This is why the tab-leave/exit flush
  (`flush_text_layers_if_dirty`) routes through `flush_text_layers`, which enqueues INLINE on the
  calling thread: an enqueue placed in the saver's FIFO before the barrier IS covered by it, whereas a
  detached `thread::spawn` would race it and could lose the edit on close.
- Overlay texture upload happens only on the GUI thread and must respect the existing
  per-frame count and byte budgets.
- Memory-pressure eviction may clear only tiled mask textures and text/image overlay display
  textures. It must keep `source_rgba`, mask data, placement/deform metadata, save jobs, and export
  snapshots intact.
- Text persistence is owned by the shared `LayerDoc`: overlay create/edit/move/group route through the
  doc and persist as the INLINE v3 payload in `layers.json` (via `flush_page_text` /
  `spawn_overlay_placement_save`; `flush_text_layers` on save-to-project). `text_info.json` is READ-ONLY
  legacy — it is read on initial load of un-migrated chapters (then migrated to inline on first flush)
  and NEVER written. New code must preserve the legacy READ normalization paths for `style/static`,
  `transform_uv`, and older render-data shapes.
- **The typing render never writes the rendered text PNG.** The create/edit render workers
  (`tab/render_store.rs`) return the pixels in their result; `insert_runtime_overlay` /
  `apply_edit_overlay_render_result` hand them to `doc.set_text_render`, which stores the image and
  marks the node `pixels_dirty`, so the doc's text flush encodes the PNG under its own uid-keyed
  `ps_p{page:04}_{uid}_text.png`. The renderers previously ALSO wrote the pixels to
  `overlay.file_name` on every completed render (i.e. per drag frame, outside the coalescing saver):
  byte-identical to what the doc writes, read by nobody, and — since `file_name` for an in-session
  create is not the doc's name — a permanent orphan that `prune_orphan_pngs` (which matches only the
  `ps_p{page:04}_` prefix) never collects.
  EXCEPTION: `save_drawn_lines_layout_image_if_needed` stays — the `CustomRasterLines` `_layout.png`
  has disk as its SOLE store (the renderer reads it back) and derives from `file_name` + dimensions
  only, so it is the only disk artifact keyed to a created overlay's `file_name`.
- A created overlay's `file_name` is `typing_overlay_p{page+1:04}_{uid}.png`
  (`created_overlay_file_name`), minted from the overlay's own fresh v4 uid, so uniqueness is
  STRUCTURAL — no filesystem probe, no wall-clock resolution to trust. It is a RUNTIME handle only:
  the doc owns the persisted name, and a reloaded overlay's handle is rebuilt from the uid by
  `text_runtime_from_doc_node`. The `typing_overlay_p{page:04}_` prefix is kept because `page_ops`
  documents it as the typing overlay PNG shape in `text_images/`; nothing parses what follows it.
- Text overlays store both placement fields and `render_data`; image overlays use the
  same runtime layer, store an effects-only `render_data` (`{ "effects": [...] }`), and expose
  the post-effect cards (stroke/glow/shadow/etc.) in the panel's Effects tab. Image-overlay text
  layout parameters remain hidden; only transform and effects are editable.
- Image-overlay effects keep the imported picture and the post-effect picture as separate PNGs:
  `file` is the post-effect image used for display/export, `image_original_file` is the untouched
  source. The original is preserved so effects can be re-edited or removed without quality loss.
  When effects are present the post-effect image is written as a `_fx` sibling; when all effects
  are removed the display reverts to the original and the `_fx` file is cleaned up. Effects are
  re-rendered on a worker thread via `render_next::apply_effects_to_image`; the source PNG is read
  from the staging dir with a fallback to the saved (main) `text_images` dir.
- Text/effect colors stored in `render_data` are straight-alpha RGBA. When serializing
  from egui `Color32`, use unmultiplied sRGBA values.
- `render_data.text_params.raster_transform` is the optional vector mesh warp
  (`{cols,rows,src_width_px,src_height_px,points_norm}`, row-major, `len == cols*rows`;
  absent => no warp). It is authored on the canvas (Phase 3), NOT a panel text param, so the
  panel carries it VERBATIM: `TypingCreatePanelState.pending_raster_transform` holds the raw
  `Value`, is loaded on edit and re-emitted on every render_data rebuild, and is decoded for
  the renderer via `codec::decode_vector_mesh_warp` (rejects malformed input -> `None`, never
  panics). The legacy `normalize_text_params_object` passes the key through unchanged.
- Layout editor (`tab/panels.rs`): while the vector-line layout editor is open on a text overlay,
  every re-render syncs the overlay's `center_page_px` to `frame_page_rect.center()` (in
  `rerender_layout_editor_overlay` for the optimistic redraw and in `apply_edit_overlay_render_result`
  for the doc-persisted result). The overlay quad is drawn `from_center_size`, so without this the
  layer would grow/shrink about its STALE center and drift off the editor frame on a frame resize.
- Deformation is represented by a high-resolution page-space mesh. Perspective, bend,
  frame, grid, and brush tools edit the shared mesh rather than storing separate tool
  parameters as persistent transform state.
- On-canvas transform mode has TWO independent kinds that COMPOSE (`transform_mode_kind`,
  gated by `transform_mode_overlay_idx`): RASTER edits the runtime `deform_mesh` (post-process,
  baked on top of the PNG — legacy path, unchanged), while VECTOR edits a transient working mesh
  that is converted to `render_data.text_params.raster_transform` and baked INTO the PNG by the text
  renderer on re-render. The vector warp is baked into `source_rgba`; the raster mesh still
  post-processes on top. Vector mode is TEXT-only and available only for `Normal`/`Shape`/
  `CustomVectorLines` layouts (see `vector_transform_allowed_for_layout_mode`). The UI normalizes
  handle positions over the stored source dims and the renderer honors those same dims as its warp
  normalization box (Design B), so the two agree; an identity working mesh round-trips to identity
  `points_norm` (a renderer no-op). LIVE PREVIEW (Phase 3b): entering vector mode caches the overlay's
  UN-WARPED base as a reconstructable GPU texture (transient `vector_transform_base` +
  `vector_transform_base_rx`, cleared on exit). If the overlay currently has NO `raster_transform`, its
  resident `source_rgba`/texture ALREADY is the un-warped base and is reused directly (no extra render);
  otherwise a one-off off-thread render with the warp cleared supplies it (`render_vector_transform_base`,
  never written to disk, polled by `poll_vector_transform_base_render`). During a drag the base is warped
  onto the working mesh (applying the warp EXACTLY ONCE — texturing the already-warped baked PNG would
  double-warp), and the plain baked PNG is hidden for that overlay
  (`vector_transform_preview_active`). On settle/reset the sharp re-render swaps `source_rgba` and the
  base is invalidated so it re-derives on the next drag.
- Mask data is binary alpha (`0` or `255`). Mask files live in the COMMITTED `text_images/` (not the
  `_unsaved` staging dir) and are page-indexed independently from overlay PNGs; `mask.rs` writes them
  directly there on panel close, so mask edits persist immediately and unvisited pages' masks stay on
  disk untouched — project-save (`copy_dir_overwrite_except`) needs no mask handling. The whole-chapter
  eager loader (`ensure_loader_started`) loads every `mask_page_*.png` at chapter open into `masks`;
  `masks_loaded(project)` reports its completion and gates whole-project export/save so
  `export_masks_snapshot` is never partial.
- Clipping applies only when the overlay enables `mask_clip_enabled`; export and live
  rendering must use the same mask sampling semantics.
- Auto-typing depends on `CleanOverlaysModel::cached_page_rgba` plus the current clean
  overlay. If the page is not cached yet, return a clear user-facing error instead of
  inventing a fallback image.
- Clean overlay visibility in the typing tab is a UI/runtime concern; export still
  composites clean overlay snapshots from `CleanOverlaysModel` or `clean_layers/`.
- Do not hold `Mutex` locks from shared models while performing image analysis,
  rendering, export composition, disk I/O, or callbacks. Copy or snapshot the required
  data and release the lock.
- Do not silently ignore worker or serialization errors. Surface a status message and
  include enough context for logs or diagnostics.
- If a selected text overlay references a font that is not among the discovered fonts,
  the edit panel must warn with the missing font name, keep only the font/group/face
  selectors enabled, and block re-rendering (`emit_edit_request`) until the user picks an
  available font. Otherwise the text would be silently re-rendered with a substituted font.
- Coordinate conversion must keep page pixels, scene coordinates, UV coordinates, and
  screen coordinates explicit. Avoid mixing width/height, x/y, row/column, or page/scene
  units in helper APIs.
- Overlay RGBA buffers must match `width * height * 4`; mask buffers must match
  `width * height`. Public helpers should reject invalid sizes instead of panicking.
- Whole-project async page preload (`doc_layers.rs`, Phase 1): per-page layer data loads LAZILY (only
  visited pages), so whole-project operations (export, save) that need EVERY page resident use the
  async preloader instead of a synchronous residency loop (which would block the GUI thread).
  `begin_preload_all_pages` spawns ONE worker that decodes each not-yet-resident page off the GUI
  thread via `LayerDoc::decode_page_payload` (a `Send` pure fn) and streams the payloads. Both the
  per-page decode (`ensure_raster_layers_for_page`) and this worker pass `self.doc_legacy_text_dir`
  (the GATED legacy `text_images/` dir: `Some` only for an un-migrated chapter, `None` once migrated)
  set ONCE per chapter in `render_jobs::ensure_loader_started` via
  `migrate::manifest_has_inline_text` — so an un-migrated legacy chapter's uid-less overlays reach the
  shared doc with a DETERMINISTIC uid (`text_payload::stable_overlay_uid`, matching this tab's own
  loader so the same overlay never double-renders), while a migrated chapter never re-reads a stale
  `text_info.json`.
  `drive_page_preload` (called every frame from `TypingTabState::draw`) applies up to
  `TYPING_PRELOAD_APPLY_BATCH` (4) payloads per frame on the GUI thread. Apply goes through the
  MEMOIZED `LayerDoc::insert_decoded_page` (an already-resident page discards the stale payload) then
  `sync_from_doc`, so a preload NEVER clobbers a resident page's unsaved edits and NEVER resurrects a
  session deletion. The user's raster/overlay selection is saved/restored around the apply loop
  (mirroring `export.rs`), since projecting resolves pending selects. `all_pages_loaded` is the cheap
  residency predicate (projected here AND in the doc); `preload_all_pages_active` /
  `preload_all_pages_progress` (`done`/`total`) provide the data for a "Подготовка страниц N/M"
  indicator. GATING PRINCIPLE (Finding 1 — no hang): both the export and save gates dispatch on preload
  PASS COMPLETION (`!preload_all_pages_active`), NOT on full residency (`all_pages_loaded`). A page whose
  decode genuinely fails (corrupt `layers.json`/`page_*.json`, or a worker panic that drops the sender)
  is dropped from the pass and NEVER becomes resident, so a residency gate would leave the deferred
  operation stuck forever (permanent spinner, disabled "Save project", no retry, exit-via-save unable to
  close). Both consumers tolerate a non-resident page (export skips/omits it; save keeps its committed
  text verbatim), so dispatching once the pass drains is safe. `drive_page_preload` counts genuine decode
  errors and logs one aggregated warning on completion (plus per-page detail) so the operation proceeds
  loudly, not silently. EXPORT is wired to this preloader (Phase 2): the export trigger in
  `draw_canvas_overlay_top_left` (`tab.rs`) runs `request_export_to_folder` immediately only when every
  page is resident AND masks are loaded AND no save is busy; otherwise it starts `begin_preload_all_pages`
  (when layers are the blocker), stores a `pending_export_after_preload` (dir + format only) on
  `TypingTextOverlayLayer`, and shows the `TypingExportUiStatus::Preparing` indicator. That indicator is
  gated on `has_pending_export()` ALONE (not `preload_all_pages_active`), so it stays visible until the
  export actually dispatches — it must not vanish when the pass drains on the give-up path while the
  export is still waiting on masks or on a busy save (progress freezes at total/total during that tail).
  Each frame `TypingTabState::run_pending_export_if_ready` (right after `drive_page_preload`) dispatches
  once the pure gate `export_dispatch_ready(preload_active, masks_ready, save_busy)` holds
  (`!preload_active && masks_ready && !save_busy`), consuming the request via `take_pending_export_if_ready`
  (which only re-checks `!preload_active`). The clip-mask snapshot is captured AT THAT run point (not when
  deferred): the mask store is whole-chapter/eager (`mask.rs` loads all `mask_page_*.png` at chapter open,
  independent of page visitation and of the preload), so capturing after preload reflects the latest mask
  edits and cannot race the preload. If the preload cannot start (no doc / no layers dir) AND no save is
  busy, the trigger runs the export immediately as a best effort rather than hanging.
  EXPORT also gates on the CLIP-MASK loader (Phase 3, the `masks_ready` term): `TypingMaskLayer::masks_loaded(project)`
  (the whole-chapter mask loader has drained for THIS chapter). The mask store has NO per-page disk
  fallback at export time, so a snapshot taken while the (fast, always-completing) loader is still running
  silently drops the clip masks of every not-yet-loaded page; `run_pending_export_if_ready` requests a
  repaint while waiting so the frame loop drains the loader instead of idle-stalling. КЛИН (cleaned base)
  is deliberately NOT gated for export: `export::load_clean_overlay_snapshot_for_export` already falls
  back to a disk read (`clean_layers/{stem}.png`) when the in-memory `CleanOverlaysModel` is not resident,
  so the composite is correct regardless of the App-side eager overlay loader — adding клин gating to
  export would have no correctness effect.
  EXPORT⇄SAVE MUTUAL EXCLUSION (Finding 2): export and project-save share the SAME preloader and both
  mutate shared doc/staging state (save's text flush → staging merge; export reads doc/overlays), so they
  must never dispatch in the same window. `MangaApp` passes `save_busy` (= `save_to_project_rx.is_some() ||
  pending_save_after_preload`) into `TypingTabState::draw`; while it holds, a new export trigger is DEFERRED
  (never dispatched inline) and `run_pending_export_if_ready` withholds dispatch (the `!save_busy` term).
  Save always completes, so the export is not starved; a save trigger does not consult export state, so
  save is prioritized (the more stateful op) without deadlock. PROJECT-SAVE (Phase 4, `app.rs`) gates on
  LAYERS ONLY — NOT masks, NOT
  клин: the save merge copies the committed `text_images/` verbatim (`copy_dir_overwrite_except`), so no
  in-memory mask data is consumed (unvisited pages' masks stay on disk), and клин edits are captured
  synchronously by `take_dirty_save_snapshots` while unedited `clean_layers/` PNGs are copied verbatim —
  so gating save on the mask loader or the клин `overlay_loader_finished` flag would have NO save-time
  correctness effect (CLAUDE.md §14). LAYER residency is what matters for save quality: only a resident
  page is in
  `LayerDoc::resident_pages()`, so `TypingTabState::flush_text_layers` flushes it and marks it OWNED,
  making the unsaved→committed merge authoritative for it (v3-complete inline text incl. deletions);
  an unvisited or decode-failed page's committed text is preserved as-is (v3-incomplete but never lost).
  The save
  TRIGGER (`MangaApp::request_save_to_project`, all three call sites: toolbar + both exit-dialog "save
  chapter" buttons) runs the save immediately when `all_pages_loaded`, else DEFERS: it starts
  `TypingTabState::begin_preload_all_pages` and sets `MangaApp::pending_save_after_preload`.
  `MangaApp::drive_pending_save_preload` (called every frame from `update`, BEFORE the tab-draw and
  independent of the active tab — the typing tab drives its own preload only while it is drawn) advances
  the preload, shows a "Подготовка страниц N/M" status (`app.save.preparing_pages`), and dispatches the
  real `start_save_to_project` once the preload PASS drains (`deferred_save_ready(preload_active) =
  !preload_active`). When the typing tab is the active tab AND a save is pending, `drive_page_preload`
  runs twice per frame (once from `drive_pending_save_preload`, once from `TypingTabState::draw`), so up
  to 8 pages apply that frame instead of 4 (Finding 3): benign and bounded — the apply is idempotent and
  a completed pass makes the second call a no-op — so it is left as-is. Save-on-exit uses
  the SAME deferral: the exit-dialog "save chapter" path keeps the app alive (frames keep pumping) until
  the deferred save completes and then closes — `on_exit` only drains the layer saver and never triggers
  a save, so there is no synchronous full-load and no hang. The pure gate cores are unit-tested: in
  `app.rs` `save_trigger_decision`, `deferred_save_ready` (incl. `deferred_save_does_not_hang_on_decode_error_giving_up`
  — the Finding 1 give-up path dispatches instead of hanging); in `tab.rs`
  `export_dispatch_ready` (`export_dispatch_gate_pass_completion_masks_and_mutual_exclusion` — proves the
  export gate carries NO residency term so the give-up path cannot hang, waits on masks, and is blocked
  while a save is busy — Findings 1 + 2). Testing note: the deterministic cores are unit-tested (`all_page_indices_resident`
  transitions; the memoized apply preserving edits + deletions; the Phase-2 ordering fix —
  `export_overlay_snapshot_is_empty_before_residency_and_populated_after` proves `build_export_overlay_snapshots`
  drops an unvisited page's text before `sync_from_doc` and includes it after; the Phase-3 mask gate —
  `masks_loaded_is_false_until_loader_finishes_for_the_chapter` in `mask.rs` proves `masks_loaded_for_dir`
  is not-ready until the chapter's mask load drains). The full async drive
  (worker thread + channel + batched apply) and the GUI export-deferral gate transition (needs an
  `egui::Context`, a live worker, and multi-frame polling) are exercised only through the GUI drive
  point and are not unit-tested, because they are GUI-coupled; the risky invariants — no-clobber/
  no-resurrect on apply, snapshot-after-materialization, and the mask-loader gate — are covered directly
  against `insert_decoded_page`, `build_export_overlay_snapshots`, and `masks_loaded_for_dir`, the exact
  steps the driver and export perform.
- Any new executable runtime logic in this module needs focused tests or an explicit
  documented reason if testing is not currently practical.
- UI strings are localized through `ms-i18n` (`t!`/`tf!`, keys under `typing.*`), NOT
  hardcoded Russian. Two classes stay as stable Russian LITERALS because they are DATA,
  not chrome (see `docs/i18n_exclusions.md`): (1) the built-in formula-preset NAMES in
  `panel/presets_io.rs` (§A1 — they are persisted `TextTab.formula_presets` map keys),
  and (2) the PSD export LAYER NAMES in `psd_export.rs` (§A5 — written verbatim into the
  exported `.psd`, so the export format must not depend on the interface language). Each
  such site carries a justifying comment. Do not route them through the catalog.
- CENTERING ASSIST ("Помочь с центровкой"). The «Действия» panel arm (`panel/facade.rs`) hosts a
  localized `ui.checkbox` (`typing.panel.centering_assist_toggle`) on the transient
  `TypingTopPanelState::centering_assist_enabled` flag plus an indented block holding a
  "Показывать центр" `ui.checkbox` (`typing.panel.centering_show_center` →
  `TypingTopPanelState::centering_show_center`, default TRUE) ABOVE a bound-center `WheelComboBox`
  (`centering_assist_kind`: image / mean / median, default Mean; `centering_*` i18n keys). All three
  are mirrored onto `TypingTextOverlayLayer` each frame (`set_centering_assist`). The show-center flag
  gates ONLY the drawn bound-center marker (the red cross+circle in `draw_page.rs::draw_centering_assist`);
  the guide frame, corner handles, binding/reconciliation, and renderer center computation stay governed
  by `centering_assist_enabled` alone. PERSISTENCE: `centering_assist_enabled` and `centering_show_center`
  persist in `user_config.json` under `TextTab` (keys `centering_assist_enabled`, `centering_show_center`),
  seeded ONCE in `MangaApp::new` (via `TypingTabState::set_centering_assist_persisted_state`) and written
  ONCE in `MangaApp::on_exit` (`persist_typing_centering_assist_state` → `config::update_user_config_file`,
  the canonical locked RMW; enabled defaults absent→false, show-center absent→true). `centering_assist_kind`
  stays SESSION-ONLY (not persisted). When enabled, the
  production text renders request BOTH renderer centers (`RenderExtraInfoRequest`) at the five dispatch
  sites landing in the live overlay runtime (create, edit, vector re-render / Ctrl+wheel / width drag,
  layout-editor re-render, shape-variant apply); the result is carried on `TypingOverlayRuntime.extra`.
  STICKY CENTERS (behavioural contract): four of those five sites request the centers when the assist is
  on **OR** when the target layer ALREADY carries a measured center
  (`TypingOverlayRuntime::has_centering_centers`). The centers are PERSISTED, and the renderer returns
  all-`None` when they are not requested — so without the OR, one assist-off text edit would ERASE a
  layer's stored centers. A layer that was ever centered therefore keeps them fresh forever; a layer the
  assist never touched pays nothing (the renderer keeps its no-compute fast path). The CREATE site is
  the exception: a brand-new layer has no stored centers, so it stays gated on the flag alone. The two
  panel-owned sites (create + edit) cannot see the runtimes, so the edited layer's sticky bit rides the
  selection mirror: `TypingSelectedOverlayForEdit.has_centering_centers` →
  `TypingTopPanelState::edit_overlay_has_centering_centers` (`false` in create mode). The
  layout-editor site must read the bit BEFORE its optimistic `overlay.extra` clear.
  CENTER OWNERSHIP: `TypingOverlayRuntime.extra` is a PROJECTION of the doc node's
  `NodeBody::Text.extra_centers`, not an independent copy. Every text render routes its pixels through
  `route_to_doc(set_text_render(..., extra))`, and `route_to_doc` re-projects the page on the SAME call
  stack, so `sync_from_doc` restores `extra` from the node whenever `pixels_changed`. A runtime-only
  center would therefore be wiped by the projection that immediately follows the render that produced
  it — which is exactly why all three bound-center kinds used to collapse onto the plain image center.
  PERSISTENCE (layers.json schema v4, `LayerRec.text_centers`): the centers are written with the very
  PNG they describe, so a reopened chapter keeps its bound center instead of falling back to the plain
  image center. The `mesh_geometry::centering_chosen_img_px` fallback now applies only to a layer the
  assist genuinely never measured.
  The MEDIAN center weights every layout LINE equally (the renderer collapses each line to one sample
  before taking the median), so it does not snap onto whichever line holds the most glyphs. Leading and
  trailing hanging punctuation is excluded from both centers whenever the layer has hanging punctuation
  ON, in the horizontal and formula layouts; the vertical layout never hangs punctuation, so its centers
  do not react to that setting.
  When OFF the feature keeps a negligible constant per-frame cost (the flag/kind mirror plus an
  early-returning `reconcile_centering_frame` call); renders compute no centers and nothing is drawn.
  STATE HOME: the guide frame is an `Option<CenteringFrame>` on `TypingOverlayRuntime` (precedent:
  `extra`), NOT reset on re-render (only the reconciliation reacts). It is PERSISTED through the doc
  node (`NodeBody::Text.centering_frame` → `LayerRec.centering_frame`, schema v4) with the OPPOSITE
  ownership direction to the centers: the RUNTIME is the live owner and the doc node is the durable
  copy. WRITE — pushed into the node by `tab/persist.rs::sync_overlay_state_into_doc` only (next to
  `mask_clip`), never by the per-frame `reconcile_centering_frame`, which would rewrite `layers.json`
  every frame. READ — `doc_layers.rs::sync_from_doc` fills it ONLY when the runtime's frame is still
  `None` (a drag in progress must win) and is deliberately NOT gated on `pixels_changed`: the frame is
  an anchor the user placed, not a property of the pixels. DIRTY MARKING — a frame-only edit moves no
  layer, so the corner-resize handler (equality-guarded) and the lazy creation call
  `mark_placement_save_dirty` themselves; the move-driven mutations need nothing, because the move
  settle already marks it. On disk the frame stores no rotation (`CenteringFrameRec`): its angle is
  always the overlay's total visual angle. TEXT layers only — an image overlay's chosen center is
  always the plain image center, so a frame would carry no information there.
  BINDING INVARIANT: while assist is on for the selected text overlay, the chosen center (page px,
  computed by `mesh_geometry::centering_chosen_center_page_px`) equals the frame center.
  `draw_page.rs::reconcile_centering_frame` runs once per frame (before `draw_entries`): it creates the
  frame lazily, makes the frame FOLLOW a whole-layer move drag, and otherwise (re-render, rotation, kind
  switch, corner-frame resize) leaves the frame anchored and translates the LAYER back. The anchored
  move targets a FIXED POINT (`mesh_geometry::centering_reconcile_target_center`): the ideal target is
  pre-clamped through the SAME constraints the later systems apply — the visibility limit
  (`clamp_translation_within_visible`, shared with `enforce_overlay_visibility_limit`), strict-pixel
  snapping, and the apply-step page/box clamp — then compared against the CURRENT center, so an
  unreachable off-page frame center converges in one move (no ping-pong, no strict-pixel alternation)
  and a move already at the fixed point marks nothing / requests no repaint. Deform meshes translate
  RIGIDLY (`TypingOverlayDeformMesh::translate_rigid`, whole-box clamp, shape preserved) so repeated
  reconciliation cannot cumulatively squash the mesh at a page edge. Corner handles
  are hit-tested WITHIN the selected overlay's own `ui.interact` (a `centering_frame_drag` state, like
  the width-guide/rotation handles) — NOT as separate later-registered widgets, because the overlay body
  still senses `click_and_drag` and egui would not award the drag to a later overlapping widget (see the
  in-file note at the vector-transform gate). All assist-driven layer moves DEFER their save via
  `mark_placement_save_dirty` (never eager writes). Pure geometry (chosen-center mapping, frame corners,
  corner-drag resize) lives in `tab/mesh_geometry.rs` and is unit-tested in `tab/tests.rs`.
- Widget-id-deriving calls that show a localized label (`WheelComboBox::from_label`,
  `CollapsingHeader::new`, `egui::Window::new`) must seed a stable, language-independent
  id (`.id_salt("typing.…")` / `egui::Id::new`). `egui::ComboBox` has no `id_salt()`
  builder — use `ComboBox::new(id_salt, label)`.
- The create/edit "Параметры" panel is grouped into collapsible sections by the free fn
  `create_main_text::collapsing_param_section` (a `CollapsingState` with a strong title + optional
  weak summary). Presets (create-only) and the "Слой" width/scale/angle group (edit-only) use the
  same helper.
- **A section's expansion state has TWO owners.** The LIVE state is egui memory under
  `egui::Id::new((id_salt, preview_enabled))` — it wins within a session and is what the user's
  clicks move. The DURABLE state is the tab's `TabExtras`
  (`widgets::panel_dock`), stored in the same `PanelLayout` section of `user_config.json` as the
  arrangement and written by the same `PanelLayoutWriter`; egui memory alone cannot survive a
  restart, because this build compiles eframe WITHOUT the `persistence` feature. The `typing.params`
  tab is therefore declared with `PanelTab::show_with_extras` (`tab.rs`), and the `&mut TabExtras`
  is threaded down `draw_params_tab_body` → `draw_params_section` / `draw_edit_params_section` /
  `draw_create_presets_section` → `draw_main_text_params` → `collapsing_param_section` /
  `draw_advanced_text_params_section`. Each section seeds `load_with_default_open` from the stored
  flag and writes back what its header shows; `TabExtras::set_flag` drops a flag equal to
  `default_open` and reports a change only on a real move, so an untouched panel writes nothing.
- The `TabExtras` key is `create_main_text::section_flag_key` = `"{id_salt}#create|#edit"`. The
  `id_salt` is a LITERAL persistence key (an i18n exclusion, not a caption — section titles come
  from `t!` keys), so the state survives a UI-language switch; the `#create` / `#edit` suffix
  encodes `preview_enabled`, the constructor-time discriminator of the two panel instances, and
  keeps them independent. One rule for every persisted section, including the plain
  `egui::CollapsingHeader` of "Дополнительные параметры" (`create_advanced.rs`), which keeps its own
  look and feeds the flag through `.default_open(..)`.
- Deliberately NOT persisted (no `TabId` to hang state off, or not egui memory at all):
  the "Параметры поиска" section of the advanced-form `egui::Window`
  (`typing.advanced.form_search_section`, passes `None`), the Settings-pane effect-defaults section
  (`panel/effect_defaults.rs`), and `draw_text_accordion` / the char-table folds, which keep their
  expansion in their own struct fields.

## Storage and external boundaries
- Persistent text assets are under `ProjectPaths::text_images_dir`.
- `text_info.json` contains an array of overlay entries with page index, file name,
  overlay kind, placement/deform data, render data, and mask clipping state.
- Render parameters are serialized through JSON-compatible names that are parsed in
  both `panel.rs` and `tab.rs`; keep enum string mappings synchronized when extending
  `TextRenderParams`.

### Persisted `text_params` schema
- **One owner**: `panel/text_params_schema.rs` owns `TEXT_PARAMS_SCHEMA_VERSION`, the FROZEN
  per-version default set, `write_text_params` (writer) and `read_text_params` (reader).
  Nothing else may decide what an absent key means.
- **Schema 2** (current, written by `panel/create_render_data.rs`): the font is named ONCE, by
  IDENTITY, under `font`; `font_path` / `font_label` / `font_original_name` / `font_family` are
  never written. Any value equal to its frozen default is OMITTED (`text`, `width_px` and `font`
  are always written, so the small direct readers stay correct); an empty `effects` array is
  omitted; the dead keys `strict_shape_fit` / `aggressive_word_breaks` are dropped. Real data
  shrinks ~1600 B → ~350 B per payload.
- **Reading order**: every reader (`tab/codec.rs`, `panel/create_apply.rs`, `psd_export.rs`) first
  calls `read_text_params`, which fills the defaults of the schema the DOCUMENT declares. A
  document with no `schema` key is schema 1 and is handed through untouched, because its absent
  keys mean the LEGACY defaults (e.g. `line_placement_reference` = `glyph_height`,
  `trim_extra_spaces`/`hanging_punctuation`/`replace_ellipsis_with_dots` = off,
  `text_shape` = `rectangle`, `line_spacing` = 50 %), not today's. The font name of a schema-1
  payload is resolved through the historical chain `font_original_name → font_label →
  font_family → font → file stem of font_path`, every form of which is still a READ-ONLY
  provider/panel alias. That chain is kept FOREVER — old projects must keep opening. It has ONE
  owner, `text_params_schema::legacy_font_name_candidates`, so the codec (which converts) and
  the PSD export (which names the font for Photoshop) cannot drift apart; both take the FIRST
  entry when they need a single name, and the conversion walks the whole list.
- **The load-time normalizer is a WHITELIST.** `codec::normalize_text_params_object` (schema-1
  entries only — it passes a schema-2 payload through verbatim) rebuilds `text_params` key by
  key, so a stored key missing from BOTH its `json!` literal and its verbatim pass-through list
  is destroyed on load, and the conversion then writes that loss back permanently. Every frozen
  schema-2 key must appear in one half or the other; `normalization_preserves_every_schema_two_key`
  (`tab/tests.rs`) enforces it.
- **Conversion**: `codec::upgrade_text_params_to_v2` converts a schema-1 payload in memory
  (folding the legacy `*_px`/`*_percent` pairs into their token key — LOSSLESSLY, via
  `PxOrPercent::to_token_lossless`, since the token then becomes the only copy — and
  materializing every field whose schema-1 absent-meaning differs from the frozen default, so
  nothing re-renders differently). `TypingTextOverlayLayer::convert_legacy_text_params_to_v2`,
  called each frame from the tab with the panel's legacy resolver, applies it to the resident
  overlays, writes it into the doc node and marks the layer dirty — the NORMAL deferred save
  writes it. An already schema-2 project is never touched, so opening it writes nothing; a
  conversion no doc node accepted marks nothing either (`route_to_doc_reporting`).
- **SAFETY RULE (non-negotiable)**: when the legacy font reference does NOT resolve (the font is
  not installed) the payload is left completely untouched and the layer keeps its legacy keys.
  The conversion may never destroy the only surviving record of which font the text was set in.
- **SAFETY RULE D (non-negotiable)**: the conversion resolves by NAME; a stored `font_path` is a
  weak hint and never evidence. Each name of the chain is offered to the resolver ALONE and in
  order (the panel's resolver gives a supplied path absolute priority, so passing both would let
  the path decide); the first that matches supplies the identity. A layer whose names all fail
  while its PATH still resolves is NOT converted — the file at that path may be a different font
  today, and converting would write that font's identity and delete the legacy keys. Such a layer
  keeps its keys verbatim and is reported once as needing the user's attention
  (`TextParamsUpgrade::PathOnlyFont`). Nothing is lost by refusing: rendering resolves a v1 layer
  by name too, so a payload whose names do not resolve does not render either way.
- **PSD export** takes the Photoshop font name from the identity: the export job carries a
  `FontPostScriptNames` snapshot (`identity → PostScript name per face`) built by the panel at
  dispatch, so the exporter no longer opens each font file twice per text layer. Which STORED key
  supplies that identity is decided by the document's OWN schema, exactly as in the codec: schema
  2 reads `font` and nothing else (a stale legacy key may not override it), schema 1 walks the
  historical chain above.
  - **FORMAT LIMITATION, REPORTED — not fixable.** A `.psd` records a text layer's font by
    NAME; our identity discriminates by CONTENT. Two installed files declaring one PostScript
    name with different bytes are two fonts here (`X%1111…` / `X%9999…`) and the baked raster
    uses the selected one's bytes, but both go into the PSD as the bare `X`, so Photoshop may
    bind the EDITABLE layer to the other file. The export is honest about it rather than
    silent: the name is still written (losing the font is worse than an ambiguous name), the
    ambiguity is logged with the page, the identity, the written name and the claimant count,
    and it reaches the user as a warning under the export success line
    (`psd_export::AmbiguousExportFont` → `TypingExportResult.warnings` →
    `TypingExportUiStatus::Success.warnings`, deduplicated per PostScript name across pages).
    PNG export produces no warnings — it bakes pixels and names no font.
- Font discovery is CONFIG-DRIVEN: the font list is the project/app `fonts` folder PLUS the
  user-imported system fonts. Those are persisted in `fonts/fonts_data.json` under `system_fonts`
  BY NAME (PostScript name) with a `last_path` byte-source hint, owned at runtime by
  `panel/font_settings_store.rs`; the create/edit panels snapshot the hints and reload the list
  live when the store changes. The legacy `TextTab.imported_system_fonts` key is READ once, for
  the first-run migration, and never written again. An imported font whose hint no longer holds
  it is LOCATED BY NAME among the installed fonts and the hint is rewritten. There is no
  "use system fonts" flag — the whole-OS enumerator (`fonts::load_system_fonts`) is used by the
  settings font-import picker, which also refreshes the by-name index.
- Shared state enters through `set_bubbles_model` and `set_overlays_model`; typing must
  not duplicate ownership of project bubbles or clean overlays.
- **`BubbleClass::Hint` bubbles are never a text source in this tab.** A hint is an author note,
  not a replica, and its single line lives in `Bubble.text` — the same field a replica's translation
  uses — so nothing distinguishes it from an ordinary text bubble unless the class is checked
  explicitly (`BubbleClass::from_str` falls back to `Text` for unknown tokens). Two guards enforce
  it, both in `tab/helpers.rs`: `pick_bubble_text_for_selection` filters hints out of the
  Shift-drag seed-text candidates, and `bubble_offers_create_text_header` (the body of the
  `CanvasHooks::has_bubble_header` impl) withholds the «Создать текст» button. The second guard is
  load-bearing because the canvas visibility gate is per-bubble: a hint with
  `extra["hint_show_outside_translation"] == true` IS rendered here, area rect and all. Any new path
  that turns a bubble into a text layer must add the same class check.

## Editing map
- The tab is `tab.rs` (data model + facade + hooks + wiring) plus behavior submodules under
  `tab/`. Add a new field to `TypingTabState`/`TypingTextOverlayLayer` in `tab.rs`; put the
  logic in the matching submodule below.
- To change overlay/raster selection, non-move drags (rotate / deform handles), or context menus, edit
  `tab/selection_rasters.rs`.
- To change how a layer MOVES (pointer or arrows, either layer kind — clamp, pixel snap, settle,
  persistence), edit `tab/move_layer.rs`; its pure delta math lives in `tab/mesh_geometry.rs`.
- To change the master per-page drawing, edit `tab/draw_page.rs`.
- To change background render/save jobs, edit `tab/render_jobs.rs` / `tab/persist.rs`.
- To change deform-mesh math or hit-testing, edit `tab/mesh_geometry.rs`.
- To change the on-canvas VECTOR transform (seed/interaction/settle/reset), edit `tab/vector_transform.rs`;
  its pure page-px<->normalized conversions and the layout-gating predicate live in `tab/mesh_geometry.rs`.
- To change persisted overlay schema parsing/normalization, edit `tab/codec.rs`.
- To change export composition, edit `tab/export.rs`.
- To change create/edit UI, presets, font loading, inline tag controls, or effect cards,
  edit `panel.rs`.
- To change clipping mask loading, painting, fill, save, or export snapshots, edit
  `mask.rs`.
- To change automatic centering over bubbles, edit `auto_typing.rs`.
- To change text layout/raster/effects behavior, use the `render_next/` public contract
  first and keep call-site changes in this directory typed through `TextRenderParams`.
  See `render_next/MODULE_README.md` and nested renderer readmes before editing
  renderer internals.
- To change persisted overlay schema, update the parser/normalizer in `tab/codec.rs`, the
  writer path in `tab/persist.rs` / `tab/doc_layers.rs`, and the export path in
  `tab/export.rs`; update this document if the contract changes.
