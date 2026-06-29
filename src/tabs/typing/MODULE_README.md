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
   become **text nodes** in `layers.json` with their FULL inline payload via the doc's text flush
   (`flush_page_text` / `write_page_text_payload`). `text_info.json` is READ-ONLY legacy and is ignored
   once the page has migrated to inline. **Text order is FULLY MANUAL** (auto-Y retired): every text is
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
   scale via `-`/`=`/`0`). Selecting a raster opens the **same right-side edit panel that image
   overlays use** (scale + rotation + the effects cards, no text params): `selected_item_for_edit`
   builds an `Image`-kind `TypingSelectedOverlayForEdit` carrying a `TypingEditTarget::Raster{page,uid}`,
   and `queue_selected_overlay_edit_request` routes `ImageTransform`/`ImageEffects` to
   `apply_raster_transform_edit` / `apply_raster_effects_edit`. Raster effects are **non-destructive**:
   the worker (`render_raster_effects`) renders the chain from the ORIGINAL `base_file`, and
   `poll_raster_effects_jobs` persists via `persist::update_raster_effects` (writes a `_fx` PNG, keeps
   the base), so effects survive a restart and removing them restores the original. One selection
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
`render_next/` is the production text renderer boundary for this module.

Typing mask tile textures and text/image overlay display textures are reconstructable GPU caches.
The module exposes memory snapshots and eviction methods for those textures only. Persistent
`source_rgba`, placement metadata, deform meshes, and binary mask data remain resident for editing,
saving, and export.

## Files and submodules
- `mod.rs`: module wiring and public re-exports for `TypingTabState`,
  `TypingTopPanelState`, and `TypingPanelLayout`.
- `tab.rs`: main tab state, `CanvasHooks` implementation, overlay runtime/storage,
  create/edit render workers, overlay transform/deform tools, layout editor helpers,
  `text_info.json` normalization, image import, and export composition.
- `panel.rs`: floating create/edit/action panels, text/effect controls, font discovery,
  preview render worker, preset persistence, inline tag helpers, and edit request queue.
- `mask.rs`: per-page binary clipping masks stored as `mask_page_{idx}.png`,
  tiled mask preview textures, brush/fill editing, async loading/saving, and export
  snapshots.
- `auto_typing.rs`: optical center computation for rendered overlays and region-growing
  bubble detection from the shared composited page cache.
- `render_next/`: text rendering subsystem. Its public contract is
  `render_next::types::*` plus `render_next::render_text_to_image`; callers in this
  directory should treat its layout, wrap, raster, formula, and effects modules as
  renderer internals.

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
- Deformation is represented by a high-resolution page-space mesh. Perspective, bend,
  frame, grid, and brush tools edit the shared mesh rather than storing separate tool
  parameters as persistent transform state.
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
- To change text/image overlay behavior on the canvas, edit `tab.rs`.
- To change create/edit UI, presets, font loading, inline tag controls, or effect cards,
  edit `panel.rs`.
- To change clipping mask loading, painting, fill, save, or export snapshots, edit
  `mask.rs`.
- To change automatic centering over bubbles, edit `auto_typing.rs`.
- To change text layout/raster/effects behavior, use the `render_next/` public contract
  first and keep call-site changes in this directory typed through `TextRenderParams`.
  See `render_next/MODULE_README.md` and nested renderer readmes before editing
  renderer internals.
- To change persisted overlay schema, update the parser, normalizer, writer, and export
  path in `tab.rs`, and update this document if the contract changes.
