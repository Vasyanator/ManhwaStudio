# Module: src/tabs/ps_editor/tools

## Purpose
Tool subsystem for the PS-like editor. Defines the `PsTool` trait and the concrete tools the tab
routes pointer input to. The tab owns a `Vec<Box<dyn PsTool>>`, so adding a tool does not touch the
tab orchestration beyond registration.

## Architecture
`mod.rs` defines the contract:
- `PsToolId`: stable tool identity (toolbar selection + hotkeys).
- `PsToolContext`: per-frame mutable access to the active `LayerStack` and page `Selection`, plus
  resolved pointer/button state in image pixel coordinates.
- `ToolOutcome`: what changed (image-space `DirtyRect` for tile invalidation, `selection_changed`).
- `PsTool`: `interact` (one frame of input), `draw_overlay` (screen-space cursor/preview),
  `options_ui` (tool panel controls), and the `as_brush_mut` downcast hook used by the tab to
  forward brush wheel/size gestures without a full `Any` downcast.

Tools never touch GPU textures, files, shared models, or the backend. They mutate the in-memory
stack/selection only; the tab converts `ToolOutcome::dirty` into `TiledTexture` re-uploads.

## Files and submodules
- `mod.rs`: trait, context, outcome, and `PsToolId`.
- `brush.rs`: `BrushTool` — round color brush on the active editable raster layer, clipped by the
  selection. Reuses `crate::tools::MaskBrush` for radius gesture/size shortcuts and cursor sizing.
- `select.rs`: `SelectTool` — rectangle marquee or freehand lasso (one struct, `SelectMode`),
  building the page `Selection`.

## Contracts and invariants
- The brush only paints when `LayerStack::active_editable_mut()` returns a layer (locked base
  layers are skipped). Erasing writes transparent pixels; painting replaces with the brush color.
- Selection-clip: when a selection is active, pixels outside it are left untouched.
- `interact` must report the modified region via `ToolOutcome::dirty` so only affected tiles
  re-upload; reporting too small a rect leaves stale pixels on screen.
- Coordinates in `PsToolContext::pointer_image` are image pixels (fractional); tools round as
  needed. `draw_overlay` receives the `ViewTransform` to map image→screen.

## Editing map
- To add a tool: implement `PsTool`, add a `PsToolId`, and register it in
  `PsEditorTabState::default`. Add a hotkey in `PsEditorTabState::handle_hotkeys` if wanted.
- To change brush behavior (color, size, erase, clipping), edit `brush.rs`.
- To change selection shapes, edit `select.rs` and `super::selection`.
