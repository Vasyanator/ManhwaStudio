# Module: src/tabs/cleaning/tools/region_edit_v2

## Purpose
The reusable on-canvas region-editing framework of the cleaning tab. It replaces the detached
region-editor window flow (not `RegionEditToolBase` itself, which stays untouched) with a
selection FRAME drawn over the page strip: eight resize handles, a drag strip above it, N mask
layers and a processed-result layer inside it, and a button row plus a status line below it.
Design and the decisions behind every rule here: `dev-docs/region_edit_v2_plan.md`.

## Architecture
Four layers, deliberately separated so that almost everything is testable without a window:

```
geometry.rs   pure maths      size constraints, hitbox, viewport clamp, page transition, arrow
layers.rs     pixels          MaskStack (N L8 layers + tinted previews) and ResultLayer
input.rs      hit geometry    handle rects, and the move/resize maths a drag performs
render.rs     paint only      strokes, handles, chrome plates, status text, off-screen arrow
frame.rs      the pass        RegionFrame: state, the per-frame pass, the reported intent
```

The authoritative frame state is `(page_idx, rect_px: OverlayRectPx)` in SOURCE PAGE PIXELS.
The screen rectangle is re-derived every frame from `CanvasView::page_scene_rect` and
`CanvasView::zoom()` and is never stored as truth, which is what makes the frame's page-pixel
footprint stable across zoom and scroll.

One pass per frame, run by the host tool from `CleaningTool::draw_overlay_ui` — the only hook
that owns the context, the canvas and the project at once:

1. `usable_viewport_for(hitbox, canvas.visible_scene_rect(), panel_rects)` — the dock panels are
   cut out RELATIVE to the frame's current hitbox (see the contract below). Before the frame has
   been placed there is no hitbox yet, so the placement step passes the viewport as its own
   hitbox.
2. Place the frame if it has none yet (centred on the current page).
3. Free frame only: `choose_page` may re-anchor it, then `keep_in_view_delta` clamps it and the
   correction is written back into `rect_px`.
4. If the hitbox left the viewport entirely, paint the off-screen arrow and stop.
5. Otherwise one `egui::Area` sized to the HITBOX senses the strip, the handles, the body and
   the chrome, applies what they did, and paints the result at the rectangle it settled on.

## Files and submodules
- `geometry.rs`: every rule of the design that is maths. GUI-free; uses `Rect`/`Pos2`/`Vec2` as
  plain geometry only and must never touch `Ui`, `Context`, `Painter` or a texture.
- `layers.rs`: `MaskStack` (per-layer L8 buffer, O(1) set-pixel counter, tinted preview, partial
  texture upload, per-stroke undo) and `ResultLayer`. No brush radius policy lives here.
- `input.rs`: `HandleKind`, the handle hit rects and arcs, and `moved_rect_px` /
  `resized_rect_px`. Every drag is measured from an anchor captured on `drag_started`, never
  accumulated per frame.
- `render.rs`: the colour constants and every paint call. Registers no hitbox, ever.
- `frame.rs`: `RegionFrame` and the per-frame pass; `FrameLock`, `FrameVisual`, `FrameHost`,
  `FrameOutcome`, `FrameButtons`.
- `mod.rs`: submodule declarations. No flattening re-exports — a `pub use` inside a private
  module of a binary crate is unreachable and only produces an unused-import warning.

## Contracts and invariants
- **The lock is derived, never assigned.** `Processing` > `ResultPending` > `MaskPainted` >
  `Free`. A locked frame cannot be moved, cannot be resized, and is NOT kept in view — it
  scrolls away and grows an arrow. This is what makes `MaskStack::resize` free to clear: a
  resize can never reach a stack that holds work.
- **A gesture in flight locks the frame too.** A paint or erase stroke whose button is still
  down derives `MaskPainted` even when the mask is empty, and `drag_active()` reports it. Both
  exist because erasing the last painted pixel mid-stroke would otherwise free the frame
  BEFORE the button is released, and the page transition, the keep-in-view clamp and the
  resize a page change performs would then run under the live stroke and throw its undo
  snapshot away. A move/resize drag keeps the keep-in-view clamp on purpose — that clamp IS
  "manual dragging stops at the viewport border" — and never coexists with a stroke.
- **A dock panel is cut from the viewport RELATIVE to the hitbox, per axis.** A cut is a
  full-width or full-height band, so it may only be charged to a panel that could actually hide
  the frame: a panel sharing neither the hitbox's columns nor its rows costs nothing, one sharing
  exactly one axis is cut from the edge it lies on, and one genuinely over the hitbox is cut from
  the edge that removes the LEAST AREA (`slab × viewport height` against `slab × viewport width`).
  Choosing by smallest SLAB instead is the defect this rule replaced: a right-docked panel that
  starts near the top of the viewport had a top slab shorter than its right slab, so it cut a
  full-width band and the frame could not be dragged above the panel's bottom edge. The cut set
  therefore depends on where the frame is and can change after a correction; that converges rather
  than oscillating, because a correction only pushes the hitbox AWAY from the edge that cut it and
  the clamp never pulls it back when a cut disappears (`geometry.rs`, the fixed-point test).
- **The keep-in-view correction is truncated toward zero.** Rounding it to the nearest page
  pixel overshoots, and at exactly half a pixel the frame alternates between two origins every
  frame; a sub-pixel residual overhang is tolerated instead, and tolerating it is what makes
  the clamp terminate. Drag deltas still round to nearest — the two live in separate functions
  in `input.rs`.
- **The handles live entirely OUTSIDE the frame.** Each of the eight is the part of a
  `HANDLE_RADIUS` disc that falls outside the frame — a half disc on a side midpoint, a
  three-quarter disc on a corner — and its hit rectangles cover exactly that and nothing more.
  The interior belongs to mask painting: a handle centred on the border reached half-way into
  it and swallowed strokes. Two consequences carry through the module. The hitbox grows by
  `FrameChrome::handle_margin` on every side, so the keep-in-view clamp cannot park a handle
  beyond the viewport border and the handles stay inside the `Area` that senses them; and the
  chrome rows are measured from the handles' outer edge, never from the frame's. A CORNER's
  area is L-shaped, so it is sensed through TWO `ui.interact` rectangles with distinct ids —
  the drag state is keyed by `HandleKind`, so either of them starts the same resize.
- **The chrome does not inherit the frame's screen width.** The rows are at least
  `FrameChrome::min_row_w` wide, widened symmetrically about the frame's centre, and the
  hitbox grows with them; a status sentence that still does not fit is elided, never spilled
  over the artwork. The rows are laid out from the HITBOX, which is also what the viewport
  clamp holds on screen.
- **The frame is never the only way to resolve a pending result.** A result-pending frame is
  locked and its own button row is only as wide as the frame is on screen, so the host tool's
  main dock panel must offer «Применить» and «Отменить» as well. Both surfaces go through
  `FrameButtons` and `FrameOutcome`; a panel queues through `request_apply` / `request_cancel`
  and the next pass re-checks the same enablement table. Queued requests are folded at the TOP
  of the pass, so they survive the off-screen early return — which is exactly the state that
  strands the user otherwise.
- **`block_canvas_zoom()` must stay `false` in the host tool.** That flag also disables the
  clean-overlay undo shortcuts for the whole session (`tab.rs`). Block precisely instead:
  `RegionFrame::captures_pointer` over the hitbox, and `drag_active()` for
  `block_canvas_drag_scroll_on_primary`.
- **The `Area` is sized to the hitbox, never to the viewport.** A viewport-sized area makes
  egui report the pointer as "over an area" everywhere and kills canvas wheel scrolling. It
  sits on `Order::Middle`, below the dock panels on `Order::Foreground`.
- **Hover and drags go through a `Response`.** Never test a raw pointer position against a rect
  to decide hover (`egui-docs/06-overlays.md` §5); the two raw reads that remain — the pointer
  of a drag already claimed through a `Response`, and the mouse button state — are gated on one.
- **Colours: red wins over green.** A locked frame whose size stopped satisfying the consumer is
  drawn red and its status line says it must be released first.
- **The frame applies nothing.** `update` borrows the canvas SHARED and reports intent through
  `FrameOutcome`; the tool performs it with `&mut CanvasView`, and must refuse a result whose
  size differs from `rect_px` — `replace_overlay_region_px` silently rescales.
- **Reuse, never copy.** Pointer-to-pixel and overlay-chunk conversions come from `tools/base.rs`
  (`pub(super)`); brush radius policy comes from `crate::tools::MaskBrush`.
- **Painting is refused while a result is pending or work is running**: the mask then describes
  work already handed over. It is also refused while a canvas zoom modifier (Ctrl/Cmd/`Z`) is
  held, because Ctrl+drag over the frame zooms the page and must not leave a stroke behind.
- **The brush is the region editor's brush, and the frame answers its gestures itself.**
  Radius, wheel and the `-`/`=`/`+` shortcuts all live in `crate::tools::MaskBrush`; erasing
  follows the same rule the region editor uses (`stroke_erases`: the right button erases unless
  the left is held too, Shift+left erases, and the panel's `set_erase` mode erases). The
  gestures are handled INSIDE the pass — over the frame's hitbox for the shortcuts and
  Shift+wheel, and on the frame body for the brush ring — because `tab.rs` refuses to deliver
  a tool's key, wheel or cursor hook while the canvas pointer is occluded, and the frame
  occludes exactly its own hitbox (`captures_pointer`). The host tool's `on_key_event` /
  `on_wheel_event` cover the pointer OUTSIDE the frame; neither surface can be dropped.
- Every `t!` key of this module lives under `cleaning.region_frame.*`.

## Editing map
- To change what a size must satisfy, how the frame is clamped, or when it changes page:
  `geometry.rs` (all of it is unit-tested; add the test with the rule).
- To change what a handle drag does to the rectangle: `resized_rect_px` in `input.rs`.
- To change how big a handle is, where it may be grabbed, or how much of a disc it shows:
  `HANDLE_RADIUS`, `handle_hit_rects` and `handle_arc` in `input.rs` — the three agree by
  construction, and `render.rs` only turns the arc into a polygon.
- To change a colour, a plate, the grip or the arrow: `render.rs` only.
- To change the lock rules, the button enablement, the status line or the pass order:
  `frame.rs`.
- To change how a mask layer stores, previews or uploads its pixels: `layers.rs`.
- To add a consumer, build the tool beside this directory and drive `RegionFrame` from its
  `CleaningTool::draw_overlay_ui`; do not add tool-specific state here. The worked example is
  `../ai_editor/`, the framework's first consumer.
