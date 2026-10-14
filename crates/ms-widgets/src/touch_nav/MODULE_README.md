# Module: crates/ms-widgets/src/touch_nav

## Purpose
The one owner of finger input on canvases. A finger that starts on a canvas never reaches a
tool: one finger pans, two or more pan and pinch-zoom, a long-press opens the canvas context
menu (lifted without moving) or starts a typed selection drag (moved after arming). Ordinary UI
(panels, menus, dialogs, bubble widgets) does not distinguish input and is left to egui. Mouse
and pen are unchanged (Space+drag, tools, right-click menus).

## Architecture
```text
shell raw_input_hook ──► TouchRouter::route (ROOT viewport only, before egui's pointer state)
   freeze last frame's TouchSurface / capture layers ─► claim on Touch{Start}
   strip the claimed touch's egui-winit mouse emulation ─► gesture state machine
   store per-owner TouchOutput in ctx temp data
canvas frame ──► publish_surface / publish_capture_layer / publish_hole (for next frame)
             ──► take_touch_output (navigation, long_press_armed_at, long_press_at, selection)
             ──► context_menu_popup(response, long_press_at).show(..)
```
- `mod.rs`: public types, constants, the publication and take-once delivery channel.
- `router.rs`: `TouchRouter` — the egui adapter `route` and the pure core `route_events`.
- `menu.rs`: `context_menu_popup`, the single open path of canvas context menus.
- `tests.rs`: pure event-list tests and headless `Context::run_ui` tests.

## Gesture contract (per claimed touch)
- **Claim** on `Touch{Start}` only: inside a surface rect published last frame, outside its
  holes (the surface's own plus any `publish_hole` of that owner — for chrome a tool draws after
  the canvas published, e.g. the buttons and handles of a capture overlay), with the top layer
  the background layer (or `None`) or one of that owner's capture layers. A bare
  `ctx.layer_painter` on a higher order is not an `Area` and never blocks a claim
  (`Context::layer_id_at` skips layers without area state). A claim lasts until End/Cancel; a claim start closes any open popup.
- **Pending** (sole claimed touch of its owner, no mouse/pen button held): no navigation while
  within `InputOptions::max_click_dist` of the start. Moving beyond it pans, catching up the
  whole distance from the start. After `LONG_PRESS_ARM_S` it **arms**
  (`long_press_armed_at` = start, every frame while held).
- **Armed, lifted within the slop** → `long_press_at` = start (open the menu there).
- **Armed, moved beyond the slop** → `TouchSelection` from the start point (`began` on its first
  frame, `end` Released on lift); when the surface has `long_press_selection == false` the drag
  pans instead.
- **Second claimed finger of the same owner**: before arming → both pan/pinch; after arming →
  the long-press (or selection, `end` Cancelled) is cancelled and the first finger is spent
  (never navigates again); the second finger pans alone.
- **Mouse/pen button held** (a press that is not a touch's emulation): no navigation, no
  long-press; a press cancels a pending/armed long-press or a selection.
- **Focus loss** drops every claim (a running selection reports Cancelled).

## Contracts and invariants
- Pairing: a release is removed only when its press never reached egui (stripped, or never
  emitted). The router can never leave a button stuck.
- Emulation matching follows egui-winit `on_touch` (fork `crates/egui/crates/egui-winit/src/lib.rs`):
  the `Touch` event first, then for the pointer-owning touch Start = `PointerMoved` + press,
  Move = `PointerMoved`, End = release + `PointerGone`, Cancel = `PointerGone`. Move/press are
  matched by position, the release by variant only (it carries the last MOVE position).
- Proximity rule (`PROMOTION_RADIUS_PT`, `PROMOTION_GRACE_S`): non-adjacent presses/moves next
  to a claimed or just-ended touch are platform-promoted copies and are stripped, the release of
  a stripped press with them. Inert on Linux and Android; values unverified (Windows is gated off).
- Navigation counts only touches present at both frame start and frame end, so adding or lifting
  a finger never jumps. Apply it as zoom about `prev_centroid`, then move `prev_centroid` onto
  `centroid`.
- Outputs live one frame: the router replaces them each frame; a consumer must take them every
  frame it publishes a surface (`take_touch_output` also requests the repaint that arms a pending
  long-press on time — a repaint requested in the hook would be reset by
  `begin_pass_repaint_logic`, `egui-0.36.2/src/context.rs:99-115`).
- Hook-time reads (`layer_id_at`, popup memory) use egui memory of `Memory::viewport_id`, which
  is ROOT between root passes only while the app has no deferred viewports (it shows immediate
  ones only).
- `owner` ids are content-named (stable across `MangaApp` rebuilds in one `egui::Context`).

## Known limits
- egui-winit gives the emulated pointer to the FIRST touch of any device (`on_touch`,
  `pointer_touch_id`): while a claimed finger rests on the canvas no other finger can tap UI;
  a tap on the long-press menu works once the long-press finger is lifted.
- egui-winit `simulate_touch_screen` (mouse posing as touch) is not supported: its events come in
  the reverse order. The app does not enable it.
- A touch whose End/Cancel never arrives stays claimed until a focus loss or a new Start with the
  same id.
- `LONG_PRESS_ARM_S` is a tuned value awaiting the device test; haptic feedback is not provided.

## Editing map
- Claim / strip / state machine / navigation math → `router.rs`.
- New output field or publication → `mod.rs` (`TouchOutput`, `TouchSurface`) + `router.rs`
  `finish_frame`.
- Context-menu open path → `menu.rs`.
- Consumers: the shell hook (`src/shell/`), `ms-canvas` and the PS editor; capture layers in the
  tab crates.
