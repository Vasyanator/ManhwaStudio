# Module: crates/ms-widgets/src/touch_nav

## Purpose
The one owner of finger input on canvases. A finger that starts on a canvas never reaches a
tool: one finger pans (axis-locked, with a fling after the lift), two or more pan and pinch-zoom
freely, a short still touch is reported as a tap, a long-press opens the canvas context menu
(lifted without moving) or starts a typed selection drag (moved after arming). Ordinary UI
(panels, menus, dialogs, bubble widgets) does not distinguish input and is left to egui. Mouse
and pen are unchanged (Space+drag, tools, right-click menus).

## Architecture
```text
shell raw_input_hook ──► TouchRouter::route (ROOT viewport only, before egui's pointer state)
   freeze last frame's TouchSurface / capture layers ─► claim on Touch{Start}
   strip the claimed touch's egui-winit mouse emulation ─► gesture state machine
   store per-owner TouchOutput in ctx temp data
canvas frame ──► publish_surface / publish_capture_layer / publish_hole (for next frame)
             ──► take_touch_output (navigation, tap_at, long_press_armed_at, long_press_at, selection)
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
  within `InputOptions::max_click_dist` of the start. Moving beyond it starts a pan (below).
  After `LONG_PRESS_ARM_S` it **arms** (`long_press_armed_at` = start, every frame while held).
- **Tap**: a Pending touch lifted (not cancelled) within the click distance, before the arm
  time, with no button held and without having stopped a fling → `tap_at` = start, lift frame
  only. Consumers use it for dismiss/commit decisions a mouse click outside would make and for
  selecting their own layers (Typing text layers); never to place or paint.
- **Axis lock** (lone finger): the pan does not move the page until the finger is
  `AXIS_LOCK_DECIDE_PT` (10 pt) from its anchor (the press point; under a held pen/mouse button
  the anchor follows the finger, so the gesture restarts where the palm lifted). Then
  `classify_axis` decides once, sticky until lift: vertical when `|dx| <= |dy|·tan 22.5°`,
  horizontal mirrored, free otherwise; the decision frame catches up the whole distance along
  the chosen axis, later frames pan only that axis's component.
- **Fling**: lifting a lone locked finger that was still moving starts a glide along its axis
  with egui's `ScrollArea` kinetic law (velocity over a 0.1 s history, ≥3 samples spanning
  >0.01 s; friction 1000 pt/s², stop below 20 pt/s, `dt` ≤ 0.1 s —
  `egui-0.36.2/src/containers/scroll_area.rs:829,859-877`, `input_state/mod.rs:1079,1234`). It is
  emitted as ordinary `TouchNavigation` frames with `touches == 0`, starting the frame after the
  lift, each asking for the next frame at once. A free (diagonal) pan or any two-finger gesture
  never flings. Stopped by: a new claimed touch of that owner (which is then never a tap), a
  mouse/pen press, a wheel or `Zoom` event, focus loss, gestures switched off, the owner not
  publishing its surface.
- **Armed, lifted within the slop** → `long_press_at` = start (open the menu there).
- **Armed, moved beyond the slop** → `TouchSelection` from the start point (`began` on its first
  frame, `end` Released on lift); when the surface has `long_press_selection == false` the drag
  pans instead. The flag is re-read from every publication of the owner until the drag starts
  (not only at claim), so a mode that ended while the finger rested makes the drag pan; once
  selecting, the drag stays a selection until lift or cancel. A consumer whose selection exists
  only at some points (Typing: a hold on the selected text layer) publishes the flag in the frame
  `long_press_armed_at` first appears: the drag cannot start before the next frame, which reads
  that publication (pinned by `selection_mode_published_in_the_arming_frame_selects_the_next_drag`).
- **Second claimed finger of the same owner**: every claim of the owner becomes a free pan for the
  rest of the gesture (an undecided one catches up from its anchor); before arming → both
  pan/pinch; after arming →
  the long-press (or selection, `end` Cancelled) is cancelled and the first finger is spent
  (never navigates again); the second finger pans alone.
- **Mouse/pen button held** (a press that is not a touch's emulation): no navigation, no
  long-press; a press cancels a pending/armed long-press or a selection.
- **Focus loss** drops every claim (a running selection reports Cancelled) and stops every fling.

## Contracts and invariants
- Pairing: a release is removed only when its press never reached egui (stripped, or never
  emitted). The router can never leave a button stuck.
- Emulation matching follows egui-winit `on_touch` (fork `crates/egui/crates/egui-winit/src/lib.rs`):
  the `Touch` event first, then for the pointer-owning touch Start = `PointerMoved` + press,
  Move = `PointerMoved`, End = release + `PointerGone`, Cancel = `PointerGone`. Move/press are
  matched by position, the release by variant only (it carries the last MOVE position) — so the
  End/Cancel adjacency is consumed only for the touch whose Start was followed by the emulated
  press (the pointer owner, claimed or not); after any other touch a following release or
  `PointerGone` is a real pen/mouse event and takes the ordinary button bookkeeping.
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
- On/off is live, never by recreating the router: `TouchRouter::set_enabled` records a request
  that `route` applies at a frame start. On applies at once from a fresh state; off waits until
  no claim and no stripped promoted press is left, so every release whose press was stripped is
  still stripped (a disabled router strips nothing). Either switch resets every gesture field;
  switching off also drops undelivered outputs. The shell feeds it
  `PlatformFeature::CanvasTouchGestures` AND the `General.touch_gestures` setting every frame.
- Publishing is live only while an ENABLED router runs on the ROOT viewport: every routing call
  writes a ctx flag (set when enabled, removed when disabled) and `publish_*` are no-ops without
  it, because only the router's freeze step drains the lists (the wasm entry has no hook).

## Known limits
- egui-winit gives the emulated pointer to the FIRST touch of any device (`on_touch`,
  `pointer_touch_id`): while a claimed finger rests on the canvas no other finger can tap UI;
  a tap on the long-press menu works once the long-press finger is lifted.
- egui-winit `simulate_touch_screen` (mouse posing as touch) is not supported: its events come in
  the reverse order. The app does not enable it.
- A touch whose End/Cancel never arrives stays claimed until a focus loss or a new Start with the
  same id; a pending switch-off waits for it too.
- `LONG_PRESS_ARM_S` and `AXIS_LOCK_DECIDE_PT` are tuned values awaiting the device test; haptic
  feedback is not provided.
- The fling starts one frame after the lift (egui's own `ScrollArea` integrates its first step in
  the release frame); the lift frame's last finger movement, like every lifted finger's, is not
  applied.
- The drag mode is last frame's publication: a mode that ends in the very frame the drag starts
  still yields a selection the owner may refuse (no "refused" back-channel); the finger then
  neither pans nor selects until lifted.

## Editing map
- Claim / strip / state machine / navigation math / axis lock / fling / tap → `router.rs`.
- New output field or publication → `mod.rs` (`TouchOutput`, `TouchSurface`) + `router.rs`
  `finish_frame`.
- Context-menu open path → `menu.rs`.
- Consumers: the shell hook (`src/shell/`), `ms-canvas` and the PS editor; capture layers in the
  tab crates.
