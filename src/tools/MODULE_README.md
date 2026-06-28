# Module: src/tools

## Purpose
This directory contains small shared low-level tool primitives that are useful across UI tools but
do not belong to a single tab. The current module provides reusable mask-brush behavior for
editing `egui::ColorImage` and binary alpha buffers.

## Architecture
`mod.rs` is the public boundary. It exports `MaskBrush` from `mask_brush.rs`.

`MaskBrush` stores only brush configuration and short-lived wheel gesture state. It handles brush
radius changes from Shift+wheel and size hotkeys, draws a circle cursor in an egui image viewport,
and paints continuous stroke segments by stamping filled circles along the segment.

The painting functions are synchronous pixel-buffer helpers. Callers are responsible for invoking
them only on appropriately scoped buffers, typically small masks, scratch overlays, or worker-owned
images. They perform bounds clipping and return early on invalid binary-mask dimensions.

## Files and submodules
- `mod.rs`: shared-tool module exports.
- `mask_brush.rs`: `MaskBrush`, internal ColorImage painting helpers, binary-mask painting helpers,
  radius input handling, and cursor drawing.

## Contracts and invariants
- `MaskBrush` is UI/tool state, not durable project state. Do not serialize it into project files.
- `paint_mask_segment` writes transparent pixels when erasing and white pixels when painting.
- `paint_binary_mask_segment` writes `0` when erasing and `255` when painting. The caller must pass
  a buffer whose logical length is at least `mask_width * mask_height`; invalid dimensions are a
  no-op.
- Public helpers operate in image pixel coordinates. Callers must convert scene/screen/UV
  coordinates before calling them.
- These helpers must not perform file I/O, model access, backend calls, or shared-model mutation.
- Keep this directory independent of tab-specific state. If behavior needs project paths, canvas
  state, or cleaning/typing-specific policy, it belongs in that tab module.

## Editing map
- To change shared brush radius controls, cursor rendering, or stroke stamping, edit
  `mask_brush.rs`.
- To expose another low-level reusable tool primitive, add its module here and re-export only the
  narrow API needed by callers.
- To change cleaning-specific mask editor behavior, edit `src/tabs/cleaning/tools/base.rs` instead.
