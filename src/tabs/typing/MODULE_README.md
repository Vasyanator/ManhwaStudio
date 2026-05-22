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

1. `ProjectData` provides page paths and `paths.text_images_dir`.
2. `TypingTextOverlayLayer` loads `text_images/text_info.json` and referenced PNG files
   on worker threads.
3. The GUI thread uploads decoded overlay images to egui textures within a per-frame
   budget and draws them through the canvas hook layer.
4. Create/edit panel changes are converted to `TextRenderParams` and rendered by
   `render_next::render_text_to_image` in background workers.
5. Finished text or image overlays are appended to the runtime layer, written as PNGs
   in `text_images/`, and serialized back to `text_info.json`.
6. Export workers compose page source, shared clean overlay snapshots, text/image
   overlays, deform meshes, and optional typing masks into final page images.

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
- `text_info.json` is the source of persistent overlay placement and render metadata.
  New code must preserve existing legacy normalization paths for `style/static`,
  `transform_uv`, and older render-data shapes.
- Text overlays store both placement fields and `render_data`; image overlays use the
  same runtime layer but do not expose text effects in the panel.
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
