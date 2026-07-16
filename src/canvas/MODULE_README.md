# Module: src/canvas

## Purpose
This directory implements the shared egui canvas used by translation, cleaning, and typing.
It owns page layout, viewport navigation, bubble editing, and the runtime layer for clean
overlay display and editing. Tabs customize behavior through hooks instead of owning separate
canvas interaction code.

## Architecture
`CanvasView` is the facade used by tabs. The canvas keeps tab-specific behavior behind
`CanvasHooks`, while shared page, bubble, overlay, and settings runtime state lives in
submodules. Expensive clean-overlay tiling and settings writes run through background
workers; GPU texture upload is throttled on the GUI thread. Clean-overlay GPU tile caches
report memory snapshots and can be evicted under memory pressure; CPU overlay images stay
owned by the model/runtime so edits and export payloads remain intact.

Bubble editing is split between runtime state and UI layers. `bubble_runtime.rs` owns pending
upserts/deletes, shared-model sync, undo/redo snapshots, clipboard flows, and failed-write
preservation. Bubbles have a domain class (`TextBubble` or `ImageBubble`); text bubbles may render
as aside/on-top/default, while image bubbles always use the aside layout path. `bubble_aside_ui.rs`
and `bubble_on_top_ui.rs` only handle layout, hit rectangles, focus, drag, and resize widgets.

An image bubble is a *group* of text areas (`RuntimeBubble.text_areas`, persisted via
`extra["text_areas"]`; see `parse_image_text_areas` / `serialize_image_text_areas` in `helpers.rs`).
The red `rect_coords` is the single image-area rectangle: drawn red, movable, and resizable via 8
handles; it is not a text area. For page-crop bubbles it is the crop region — `crop_rect` is kept
equal to `rect_coords` on save and `helpers::image_area_rect_from_bubble` resolves the crop region
as the image area, so the canvas owns the only red rect (the translation tab draws no separate crop
overlay). Every text area (including area 0) is an independent image-space sub-box clamped inside
`rect_coords`, with its own anchor (inside its rect), text, resize handles, and palette color
(`image_area_palette`, a reverse rainbow from blue). The bubble side comes from
`image_bubble_side_from_areas` (sign of `Σ(anchor_u − 0.5)`). Editable, one card holds the preview,
one framed row block per area (`Оригинал`/`Описание`/`Перевод`), an "add area" button, then the
action row; each area draws its own colored rect, anchor point, and link line (aimed at that block's
center). The aside column is built from `AsideItem`s, not raw bubble ids: a read-only image bubble
splits into one `AsideItem` per area, each rendered as an ordinary text-only aside card placed by
its own anchor side. Drag routing (`AsideDragTarget`): a row block moves only its area; the card body
outside blocks, or empty space inside the red rect, moves the red rect (areas + anchors follow); an
area rect on the page moves that area; an anchor point moves only inside its own area.

The canvas owns a collapsible bottom-center keyboard-shortcut hint overlay, drawn centrally in
`scene.rs::draw_canvas_bottom_hint` right after the floating controls. Content is opt-in per tab:
the owning tab pushes `Option<CanvasBottomHint>` (label + key rows) every frame via
`set_bottom_hint`, and `None` hides it. Rows are FIXED, hand-authored, localized pairs (built in
`app.rs::build_translation_hint_rows` / `build_typing_hint_rows` from `canvas.bottom_hint.*` `t!`
keys), NOT derived from the hotkey registry; rebuilding them per frame re-localizes on a runtime
language switch. Only Translation and Typing set a hint; the Cleaning tab leaves `bottom_hint` at
`None`, so the overlay is not drawn there. The overlay is bottom-pinned just above the horizontal
scrollbar (or the inner viewport bottom when no bar is drawn) and never overlaps the bar. Collapsed
shows only an up-arrow toggle; expanded slides a popup panel up with the arrow riding above it.
`bottom_hint_collapsed` is the live user toggle: seeded once from `user_config` via
`set_bottom_hint_collapsed` at tab construction (default expanded) and read back with
`bottom_hint_collapsed()` for persistence — persisted only for Translation and Typing (Cleaning has
no hint). Per-tab content and config persistence live in the tabs, not here.
`scene.canvas_bottom_hint_rect` (last-frame rect) occludes canvas zoom/drag input under the hint.

The top-left canvas controls panel (`scene.rs::draw_canvas_controls`) is a movable, collapsible
auto-sized `Area`. Its zoom row also carries the canvas-shortcuts hover chip
(`canvas.shortcuts_hint.*`), right-aligned via `egui::containers::Sides` in
`draw_zoom_and_shortcuts_row`; the chip lists the canvas-intrinsic navigation keys (zoom/pan/scroll)
and is hover-only, so it has no click and no persisted state. Layout invariant: `Sides` (like a bare
`right_to_left` layout) expands to the parent's full available width, which inside an auto-sized
`Area` is the whole screen and would stretch the panel. So the chip row is allocated to exactly
`scene.controls_content_width` — the panel's natural width, measured each frame from the rows that do
NOT contain the chip (header, checkbox, opacity slider). That exclusion is required: measuring the
stretched chip row would feed its width back in and grow the panel by one item-spacing per frame,
and re-measuring the chip-free rows every frame is what lets the panel shrink again under a shorter
locale. When adding a row to this panel, fold its width into `natural_width` unless it is
chip-stretched.

Clean overlays enter through `CleanOverlaysModel`. Normal canvas visibility uses the
model's shared visibility flag. A canvas may also set a local clean-overlay visibility
override for UI-only cases such as the typing tab; local overrides must not mutate the
shared model or change cleaning-tab visibility.

Viewport sync across translation, cleaning, and typing is explicit. `MangaApp` owns the
shared `CanvasViewportSnapshot`, publishes it only from the active canvas after that
canvas is drawn, and applies it only to the canvas being entered. Inactive canvases must
not be scrolled or re-anchored every frame.

`CanvasView::focus_page` is deferred navigation: it applies the zoom immediately and records
a `pending_focus` request that the draw pass resolves into a scroll offset once the target
page's world rect (and its center, when not passed explicitly) is known — so focusing a canvas
that has never been laid out (e.g. opening a page in a not-yet-visited tab from the page
manager) scrolls on its first frames instead of being silently dropped. Exactly one request is
pending at a time; a newer `focus_page` or `apply_viewport_snapshot` call replaces or drops it.

Source page geometry is separate from source page GPU residency. Scene layout and hit testing use
`PageImageInfo` dimensions supplied by `MangaApp`; `PageTexture` only represents optional tiled GPU
handles for source imagery. NEAREST source textures are materialized lazily while pixel inspection
is active and are dropped outside the active page window.

Pixel inspection has a single DPI-correct trigger: `device_pixels_per_source` (`zoom *
pixels_per_point`) compared against `PIXEL_INSPECTION_MIN_DEVICE_PX`, exposed as
`pixel_inspection_recommended`. The same notion drives NEAREST sampling for source tiles, the clean
overlay, and the cleaning text mask, plus the pixel grid, so a magnified source pixel looks identical
across layers. The grid is drawn in one late overlay pass (`draw_pixel_grid_overlay`), not in base
layers. Overlay and text-mask tile draws viewport-cull tiles against the visible clip rect.

Directed zoom is anchored in content/world space and clamps the requested horizontal
scroll offset to the current scrollable range. The canvas creates horizontal scroll range
before the visual strip fully reaches viewport width, so anchor compensation has a stable
X range before the old overflow point.

## Files and submodules
- `mod.rs`: public facade, hook trait, render orchestration, and synchronization with
  shared models.
- `scene.rs`: page strip layout, viewport interaction, page hit-testing, and canvas UI.
- `overlay_runtime.rs`: clean overlay CPU/GPU runtime state, background preparation, and
  local/shared visibility state.
- `bubble_runtime.rs`: runtime bubble state, model synchronization, undo/redo, and clipboard.
- `bubble_aside_ui.rs`: aside bubble column layout and interactions. Layout runs as
  `build_aside_desired_slots` (measure) -> `pack_aside_slots` (pure vertical packing) ->
  `draw_aside_slots`. `draw_aside_side` picks single- or two-column layout per side: with
  `CanvasState::aside_second_column` on and enough free span for both columns plus gaps to stay
  inside the viewport, a side splits into near/far columns. Distribution is near-priority
  (`split_near_priority`): isolated bubbles stay near, only overlapping clusters split alternately
  near/far. Columns are equal width (min width, hugging the ribbon, when stretching is off; up to
  max width when on). Far links stay roughly horizontal while the near column packs invisible spacers
  at far anchor heights so its cards spread and far links thread the gaps.
- `bubble_on_top_ui.rs`: on-page bubble widgets, focus controls, move, and resize handling.
- `settings.rs`: canvas settings snapshots and persistence worker. Its `user_config.json` half writes
  through `config::update_user_config_file` (the serialized read-modify-write boundary), never a bare
  `fs::write`: this worker runs off the GUI thread and an unlocked read-modify-write here could drop
  the ORT SIGILL guard marker written concurrently under the same lock.
- `helpers.rs`: stateless geometry, image, and text helper functions.
- `types.rs`: passive DTOs and runtime payload types.
- `view_transform.rs`: `ViewTransform` world<->screen affine map (`screen = world * scale + translation`). The `ScrollArea` still allocates the page strip and owns scrolling, but each page's authoritative screen `image_rect` and its `page_in_view` visibility are now produced by this transform: `reserve_canvas_page_frame` establishes one per-frame transform from the first laid-out page (`scale == state.zoom`, `translation = old_image_left_top - world_min*scale`) and maps every page through `world_rect_to_screen`. A once-guarded equivalence check warns if the transform-derived rect drifts >0.5px from the old ad-hoc rect. Future increments will remove the `ScrollArea` and make the transform the sole camera.
- `workers.rs`: background worker startup for overlay preparation, autosave, and settings.

## Contracts and invariants
- Do not block the GUI thread with image decoding, disk I/O, long computation, or worker waits.
- Do not hold shared model locks while rendering, calling hooks, or doing heavy work.
- Keep page pixels, scene coordinates, screen coordinates, and UV coordinates explicit.
- Overlay buffers and masks must validate width, height, and buffer length before use.
- Shared visibility changes belong in `CleanOverlaysModel`; tab-local visibility must stay
  inside the specific `CanvasView`.
- Canvas scroll areas need per-instance egui ids. Cross-tab viewport sync must go through
  `CanvasViewportSnapshot`, not shared egui `ScrollArea` memory.
- The page strip in `scene.rs` deliberately zeroes the ambient `item_spacing` while
  allocating page rows so screen tops stay linear in world space and the `ViewTransform`
  (`screen = world*scale + translation`) can reproduce them. Inter-page gaps come only from
  the explicit `edge_margin`/`page_spacing` settings, never from theme spacing. The spacing
  is restored before drawing aside/on-top bubbles, whose inner widgets inherit the ui style.
- `CanvasHooks` callbacks must stay lightweight and must not mutate shared models while canvas
  locks are held. Use typed canvas APIs or tab-owned worker/event channels for heavier work.
- Vertical-scrollbar marks are tab-owned. After `draw_canvas_scene` lays out the strip,
  `mod.rs::render_scrollbar_marks` asks the active tab via `CanvasHooks::canvas_scrollbar_marks`
  (default none) and paints the returned marks onto the native vertical bar with
  `widgets::paint_marks_on_bar`, then re-draws the handle on top so it stays visible. The
  `egui::ScrollArea::both` engine is untouched (both axes scroll natively). Tabs position marks in
  content space via `CanvasScrollbarContext::content_y` (`world_y * zoom` from
  `scene.page_world_rects`); the canvas owns geometry, the tab owns mark content.
- Bubble persistence is routed through `BubblesModel` saver tasks; canvas runtime should keep
  unsaved runtime edits explicit until they are flushed to the model.
- The periodic overlay autosave has an explicit shutdown flag and is joined before structural page
  operations or app teardown; no autosave writer may survive a page-index transaction.
- Bubble undo/redo is delegated to the generic `ms-actions` engine
  (`bubble_runtime.rs::bubble_history: ActionHistory<BubbleSnapshotOp>`; the op lives in
  `bubble_action.rs`). It is a behavior-preserving FULL snapshot op, not a field-level patch:
  each op holds `Arc<Vec<Bubble>>` before/after snapshots and reverses by `BubblesModel::reset`.
  Mutation is observer-style — the call site mutates the model directly, then history is recorded.
  `capture_bubble_history_before_mutation` stages the pre-mutation snapshot + revision in
  `pending_history_before`; the next capture (or an undo/redo) finalizes it into a recorded op via
  `finalize_pending_history`, using the then-current state as the mutation's `after`. Recording is
  deduplicated by revision (monotonic, bumped per mutation): a staged snapshot whose revision still
  matches the current model produced no mutation and records nothing, so one gesture is one op. The
  engine enforces the `BUBBLE_HISTORY_LIMIT` cap and truncates the redo branch on a fresh record.
  `flush_bubble_upserts_to_model`
  debounces positional model writes while a continuous drag/resize gesture is active
  (`aside_drag_state` / `on_top_drag_state` / `active_rect_handle` / `active_area_handle`): the
  runtime bubble still follows the pointer each frame, but the model is written only on release,
  so one gesture yields exactly one undo entry and one model commit. Gesture-end handlers must
  re-insert the dragged id into `pending_upsert` so the final position commits. If the dragged
  widget stops being rendered mid-drag (its page scrolls fully off-screen) egui never delivers
  `drag_stopped()`, so the per-frame `mod.rs::commit_lingering_drag_gestures_on_pointer_up`
  fallback (run in `draw` after the scene pass, only when the primary pointer is up) is the
  data-loss guard: it routes aside/on-top drags through `finish_*_drag` and mirrors the rect/area
  handle `drag_stopped` paths (`pending_upsert.insert` + clear `active_*_handle`). It is the single
  source of truth for that commit and is skipped for a normally-finishing gesture, which already
  cleared its state before the fallback runs, so each gesture commits exactly once.
- `hook_bubbles_revision()` is a cheap `u64` fingerprint of the bubble set `hook_bubbles_snapshot`
  would build: it folds `BubblesModel::revision()` with the runtime-only set
  (`runtime_bubbles` count + `next_bubble_id`), so a runtime-only, not-yet-flushed bubble bumps it.
  Use it for equality gating between frames, not for ordering.
- `page_bubbles_bucketed(page)` buckets all runtime bubbles of a page into the four
  `(side, type)` aside/on-top columns in a single pass. It is the sole bubble-column scanner;
  consumers read the relevant `(side, type)` column from one bucketed result per page per pass
  instead of re-scanning runtime bubbles once per column.
- Per-bubble image caches on `CanvasView` (`image_bubble_meta_cache`,
  `image_bubble_preview_cache`, keyed by bubble id) must be evicted whenever a bubble is fully
  removed. `remove_runtime_bubble` is the single full-removal path and owns that eviction, so
  deleted ids never leak and a reused id cannot serve a stale fingerprint/preview.
- `BubbleClass::Image` is not a display type. It must not resolve through on-top display settings;
  image-specific metadata belongs in bubble `extra`.
- Source page GPU residency is verified manually for now because `egui::TextureHandle` creation
  and eviction require a live GUI context; pure tests should target memory-manager policy instead.
- Clean-overlay memory eviction may drop only reconstructable GPU texture pages. It must not drop
  `overlay_images`, prepared worker payloads currently being uploaded, or shared model state.

## Editing map
- To change clean overlay visibility, upload, tiling, or editing runtime, edit
  `overlay_runtime.rs` and the facade methods in `mod.rs`.
- To change page layout, scrolling, zooming, or context menus, edit `scene.rs`.
- To change source page GPU residency or NEAREST inspection behavior, edit `scene.rs`,
  `mod.rs`, and the source-page texture owner in `app.rs`.
- To change bubble editing behavior, start in `bubble_runtime.rs` and the relevant
  bubble UI module.
- To change canvas hook contracts, public runtime DTOs, or persisted canvas settings, start in
  `types.rs`, `mod.rs`, and `settings.rs`.
- To change background preparation or settings-save threading, edit `workers.rs` and the caller
  runtime module that owns the channel.
