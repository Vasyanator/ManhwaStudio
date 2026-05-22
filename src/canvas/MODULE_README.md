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
preservation. `bubble_aside_ui.rs` and `bubble_on_top_ui.rs` only handle layout, hit rectangles,
focus, drag, and resize widgets.

Clean overlays enter through `CleanOverlaysModel`. Normal canvas visibility uses the
model's shared visibility flag. A canvas may also set a local clean-overlay visibility
override for UI-only cases such as the typing tab; local overrides must not mutate the
shared model or change cleaning-tab visibility.

Viewport sync across translation, cleaning, and typing is explicit. `MangaApp` owns the
shared `CanvasViewportSnapshot`, publishes it only from the active canvas after that
canvas is drawn, and applies it only to the canvas being entered. Inactive canvases must
not be scrolled or re-anchored every frame.

Source page geometry is separate from source page GPU residency. Scene layout and hit testing use
`PageImageInfo` dimensions supplied by `MangaApp`; `PageTexture` only represents optional tiled GPU
handles for source imagery. NEAREST source textures are materialized lazily while pixel inspection
is active and are dropped outside the active page window.

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
- `bubble_aside_ui.rs`: aside bubble column layout and interactions.
- `bubble_on_top_ui.rs`: on-page bubble widgets, focus controls, move, and resize handling.
- `settings.rs`: canvas settings snapshots and persistence worker.
- `helpers.rs`: stateless geometry, image, and text helper functions.
- `types.rs`: passive DTOs and runtime payload types.
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
- `CanvasHooks` callbacks must stay lightweight and must not mutate shared models while canvas
  locks are held. Use typed canvas APIs or tab-owned worker/event channels for heavier work.
- Bubble persistence is routed through `BubblesModel` saver tasks; canvas runtime should keep
  unsaved runtime edits explicit until they are flushed to the model.
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
