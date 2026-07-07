# Module: src/tabs/typing

## Purpose
This directory implements the `Text` tab. It combines a read-only `CanvasView`,
text/image overlay placement, text rendering, overlay deformation, clipping masks,
auto-typing, import/export, and the floating panels used to create or edit text.

The module is a tab-level integration layer. It must keep long rendering, file I/O,
image decoding, export, mask filling, and auto-detection work off the GUI thread.

## Architecture
`TypingTabState` in `tab.rs` owns the tab runtime and implements the canvas extension
through `TypingHooks`. The canvas remains the common viewer/input surface; typing adds
extra page overlays, selection handles, deform tools, mask preview/input, and top-left
floating UI.

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
   read-only: `interact_page_rasters` adds canvas select + move/rotate drag (parity with overlays;
   scale via `-`/`=`/`0`, arrow-key pixel nudge via `try_move_selected_raster_by_arrow_shortcuts`).
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
   `select_raster`). Transforms persist via `persist::update_raster_transform` (no whole-page
   rewrite). A **right-click (ПКМ) canvas context menu** on a selected raster mirrors the text-overlay
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
   `prepare_raster_mask_clips` re-clips via `mask_layer::clip_overlay_rgba_if_needed` and re-uploads);
   "Порядок" ▲▼ (`move_raster_in_unified_z` → the shared uid-based band-Z core `move_node_in_unified_z`,
   reused with the overlay reorder); "Удалить слой" (`remove_raster` → `doc.remove_node` +
   `flush_page_dropping_raster` so the deleted raster does not resurrect on disk). Everything routes
   through the shared doc; the PS tab sees it via the version watch. The LAYERS list lives as the «Слои»
   tab of the combined floating **Actions/Layers panel**: `TypingTopPanelState::draw_vertical_panel`
   draws the panel Area/Frame + a 2-tab header (collapse toggle + `selectable_value` «Действия» / «Слои»,
   `actions_panel_tab`, default «Действия», expanded) mirroring the Параметры/Эффекты panel; the
   «Действия» arm holds the mask/import/export actions, the «Слои» arm calls
   `TypingTextOverlayLayer::draw_layers_tab_body(ui, page_idx)` (the layer state lives on `text_overlays`,
   so `draw` takes `&mut TypingTextOverlayLayer` + `page_idx`). The «Слои» body is ONE unified,
   interleaved list of ALL the page's layers — text overlays, image overlays, AND rasters — ordered by
   unified band-Z DESCENDING (top first), with overlay-above-raster on a Z tie (`order_unified_layer_rows`,
   the canvas/hit-test tie-break). Every row has ⬆/⬇ moving it one step in the unified Z (overlay →
   `move_overlay_in_unified_z`, raster → `move_raster_in_unified_z`; both route through the shared doc band
   reorder so kinds interleave), at most one move per frame; clicking a row selects it (opening the
   right-side edit panel). The list WIDTH is user-resizable (`egui::Resize`, `resizable([true,false])`, so
   HEIGHT follows content) and PERSISTED in
   `layers_panel_width`, clamped to a MIN width that fits exactly `LAYERS_PANEL_MIN_PREVIEW_CHARS` (5)
   preview chars (`overhead + 5*char_px`, so it can't shrink below the 5-char width). On the «Слои» tab the
   combined panel's Frame width is `max(actions_width, layers_panel_width)` (`layers_panel_width()`
   accessor) so the inner resize can actually widen it. HEIGHT follows
   content, capped at `LAYERS_PANEL_DEFAULT_ROWS` (8) rows by the inner `ScrollArea::max_height` +
   `auto_shrink([false,true])` (a short list hugs; >8 rows scroll); `row_height` is derived from a
   measured galley, not a magic number. Only width is user-adjustable. A text row's label is
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
   `render_next::render_text_to_image` in background workers.
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
   (`flatten_typing_export_page_rgba`, shared by PNG and PSD). PS **raster layers are composited from an
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
- `mod.rs`: module wiring and public re-exports for `TypingTabState`,
  `TypingTopPanelState`, and `TypingPanelLayout`.
- `tab.rs`: module root of the tab. Holds the data model (all `struct`/`enum`
  definitions incl. `TypingTabState`, `TypingTextOverlayLayer`, `TypingOverlayRuntime`,
  `TypingRasterLayer`, deform/export/create/edit/layout structs), the public
  `TypingTabState` facade + `Default`, the `impl CanvasHooks for TypingHooks`, and the
  `mod`/`use` wiring. The behavior (methods + free fns) lives in child submodules under
  `tab/`. All child modules are DESCENDANTS of `tab`, so they read the model's private
  fields directly; moved methods/free-fns are `pub(super)` (or `pub(in crate::tabs::typing)`
  when a typing-level sibling like `panel.rs`/`psd_export.rs` calls them).
- `tab/` submodules (each an `impl TypingTextOverlayLayer` method group and/or free-fn slab):
  - `doc_layers.rs`: shared `LayerDoc` sync, unified band-Z ordering, raster-layer projection.
  - `render_jobs.rs`: background edit/create/raster/shape-variant render jobs, loader/migration start.
  - `persist.rs`: text placement save / staging flush / save-to-project (`flush_text_layers`).
  - `create_upload.rs`: create/shift-drag UI, text editor, status overlays, texture upload.
  - `selection_rasters.rs`: overlay/raster selection, remove, raster interact/menu/drag/transform/deform.
  - `panels.rs`: deformation panel, layers-tab body, layout-editor floating panels.
  - `autotype.rs`: auto-typing hotkey trigger, job poll, result apply, debug visuals.
  - `draw_page.rs`: `draw_page_overlays` (master per-page draw) + repaint/visibility/pixel-snap helpers.
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
  - `mesh_geometry.rs`: deform-mesh/handle math, overlay geometry, hit-tests, unified-Z helpers (pure fns).
  - `layout_editor.rs`: vector-line layout-editor free fns (frame/line hit-test, draw, conversions).
  - `render_store.rs`: create/edit/raster render-and-store workers, shape-variant grid/preview.
  - `export.rs`: PNG/PSD export jobs + page composition/flatten free fns.
  - `codec.rs`: `render_data`/`TextRenderParams` parsers and overlay storage-entry normalize/parse.
  - `helpers.rs`: selection→page resolution, bubble/area seed text, doc-node runtime, page-size/overlay disk loaders.
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
  - `facade.rs`: whole `impl TypingTopPanelState` — public facade, vertical/preview panel drawing,
    request queues (`pub(in crate::tabs::typing)` for the methods `tab.rs` calls).
  - `create_state.rs`: `TypingCreatePanelState` construction, focus/eyedropper tracking, font-group
    management and font-index lookup.
  - `create_render_data.rs`: render-data/effects/font-profile/shape-layout JSON building + profile sync.
  - `create_presets.rs`: create/formula preset apply & save UI, font-combo binding, face-index clamp.
  - `create_sections.rs`: top-level section drawing (preview/params/effects/right actions) + effects_json.
  - `create_main_text.rs`: main text-param UI (left/right columns, inline offset, alignment).
  - `create_advanced.rs`: advanced params — formula/shape layout, spacing, text accordion, advanced-form window.
  - `create_edit.rs`: edit-mode params section + inline text-selection / inline-tag styling.
  - `create_apply.rs`: apply selected-overlay data, font selection, preview render queue/poll, render-param builders.
  - `text_forms.rs`: char/byte range conversions, advanced-form range-row + sort + card (free fns).
  - `inline_tags.rs`: inline-tag machine/opening/closing build + parse, offset/stretch/color/align tokens (free fns).
  - `effect_cards.rs`: effect-card title, per-card control UI, preview-render worker spawner (free fns).
  - `fonts.rs`: font discovery/loading (dir + system), duplicate merge/disambiguation, group listing (free fns).
    Coverage (`font_coverage`) is classified once per font at LOAD time (off the GUI thread) from the
    representative face's bytes and cached on `FontEntry.coverage`; the dropdown never recomputes it.
  - `font_coverage.rs`: pure classification of a font's support for the program language's writing
    system (Cyrillic today) → `Full`/`Partial`/`Unsupported` via the swash charmap; drives the
    red/yellow font-dropdown highlight + hover tooltip in `create_presets::draw_font_combo_option`.
  - `presets_io.rs`: TextTab preset persistence + formula/drawn/vector layout <-> `Value` conversions (free fns).
  - `ui_helpers.rs`: font-family binding/matching, wheel-scroll, param rows, enum cyclers/parsers, Value readers (free fns).
  - `effect_parse.rs`: `parse_effect_cards` (free fn).
  - `effect_defaults.rs`: user-configurable DEFAULT parameters per effect kind. Owns a
    runtime-global `OnceLock<RwLock<HashMap<discriminator, Value>>>` store (seeded at
    startup from `TextTab.effect_defaults` via `seed_effect_defaults_from_config`),
    resolves the add-time default card (`effect_default_card`, consulted in
    `create_sections`), and provides the `EffectDefaultsEditorState` editor widget
    rendered by the settings pane. Per-card (de)serialization reuses the shared
    `effect_card_to_value` (`effect_cards.rs`) / `parse_effect_cards` codec; persistence
    reuses `presets_io::{load,save}_text_tab_effect_defaults` (off-GUI-thread saves).
  - `tests.rs`: `#[cfg(test)]` unit tests for the panel.
- `mask.rs`: per-page binary clipping masks stored as `mask_page_{idx}.png`,
  tiled mask preview textures, brush/fill editing, async loading/saving, and export
  snapshots.
- `auto_typing.rs`: optical center computation for rendered overlays and region-growing
  bubble detection from the shared composited page cache.
- `rotation_ctrl_wheel.rs`: app-wide runtime-global (`RotationCtrlWheelMode` Vector/Raster,
  default Vector) selecting how the Ctrl+wheel gesture rotates a selected overlay. Config-free;
  seeded at startup from `TextTab.rotation_ctrl_wheel_mode`, written by the settings "Тайп" pane,
  read by the Ctrl+wheel handler in `tab/selection_rasters.rs`. `pub mod` so settings can reach it.
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
- Mask data is binary alpha (`0` or `255`). Mask files live in `text_images/` and are
  page-indexed independently from overlay PNGs.
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
- Any new executable runtime logic in this module needs focused tests or an explicit
  documented reason if testing is not currently practical.

## Storage and external boundaries
- Persistent text assets are under `ProjectPaths::text_images_dir`.
- `text_info.json` contains an array of overlay entries with page index, file name,
  overlay kind, placement/deform data, render data, and mask clipping state.
- Render parameters are serialized through JSON-compatible names that are parsed in
  both `panel.rs` and `tab.rs`; keep enum string mappings synchronized when extending
  `TextRenderParams`.
- Font discovery reads project/app font directories and can include system fonts when
  `TextTab.use_system_fonts` is enabled in `user_config.json`.
- Shared state enters through `set_bubbles_model` and `set_overlays_model`; typing must
  not duplicate ownership of project bubbles or clean overlays.

## Editing map
- The tab is `tab.rs` (data model + facade + hooks + wiring) plus behavior submodules under
  `tab/`. Add a new field to `TypingTabState`/`TypingTextOverlayLayer` in `tab.rs`; put the
  logic in the matching submodule below.
- To change overlay/raster selection, movement, or context menus, edit `tab/selection_rasters.rs`.
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
