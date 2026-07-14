# 03 — Input: events, pointer, keyboard, scroll

Target version: **egui 0.35.0** (`Cargo.toml:63`). Two widely-remembered APIs **no longer exist** in
0.35 and this project has explicit replacements for both — read the "Removed" sections before writing
any input code.

## Reading input

```rust
// egui-0.35.0/src/context.rs:925 / :937
pub fn input<R>(&self, reader: impl FnOnce(&InputState) -> R) -> R;
pub fn input_mut<R>(&self, writer: impl FnOnce(&mut InputState) -> R) -> R;
```
The closure holds a lock — keep it tiny, copy the values out, and **never call another `ctx.input*`,
`ui.*`, or repaint from inside it** (re-entrant deadlock). The repo's pattern is to destructure in one
shot: `src/tabs/typing/tab.rs:721-728`.

`InputState` (`egui-0.35.0/src/input_state/mod.rs:215`) — public fields:

| field | line | notes |
|---|---|---|
| `raw: RawInput` | :217 | what the backend sent this frame |
| `pointer: PointerState` | :220 | mouse / single-finger touch |
| `smooth_scroll_delta: Vec2` | :243 | ramped over frames; **zeroed while the zoom modifier is held** |
| `pixels_per_point: f32` | :265 | |
| `max_texture_side: usize` | :270 | |
| `time: f64`, `unstable_dt`, `predicted_dt`, `stable_dt` | :273/:279/:287/:312 | use `stable_dt` for animation |
| `focused: bool` | :317 | window has keyboard focus |
| `modifiers: Modifiers` | :320 | state at start of frame |
| `keys_down: HashSet<Key>` | :325 | |
| `events: Vec<Event>` | :328 | in-order events this frame |

Private-with-accessor: `zoom_factor_delta` → `zoom_delta()` (:559) and `zoom_delta_2d()` (:582);
`rotation_radians` → `rotation_delta()` (:614); `viewport_rect` → `viewport_rect()` (:521) /
`content_rect()` (:507); `smooth_scroll_delta()` also exists as a method (:547).
Also: `is_scrolling()` (:635), `time_since_last_scroll()` (:641), `key_pressed/key_down/key_released`
(:743/:766/:771), `multi_touch()` (:831).

## REMOVED: `InputState::raw_scroll_delta`

`grep -rn raw_scroll_delta egui-0.35.0/src/` → **0 hits**. It existed in 0.31
(`egui-0.31.1/src/input_state/mod.rs:136`). It is gone.

Why it matters: under the zoom modifier (Ctrl/Cmd) egui diverts the wheel into `zoom_delta` and leaves
`smooth_scroll_delta` at zero, so Ctrl+wheel handlers that read `smooth_scroll_delta` see nothing.

**Project replacement — call this, do not reinvent it:**

```rust
// src/input_util.rs:34
pub fn raw_wheel_delta(input: &egui::InputState) -> egui::Vec2
```
It sums the frame's `Event::MouseWheel { delta, .. }` values (`src/input_util.rs:35-43`). Sign matches
`smooth_scroll_delta` (positive Y = content moved down). The magnitude is **unit-dependent** —
`MouseWheelUnit::{Point, Line, Page}` (`egui-0.35.0/src/data/input/mouse_wheel_unit.rs`) — so only use
it for sign/threshold decisions, never as a pixel distance. Unit tests: `src/input_util.rs:66-98`.

Callers: `src/canvas/mod.rs:1507`, `src/tabs/typing/tab.rs:725` and `:788`,
`src/tabs/typing/tab/selection_rasters.rs:338`, `:616`, `src/tabs/cleaning/tools/base.rs:1234`,
`src/tabs/translation/adv_rec.rs:962`.

Caveat (pre-existing duplication, not a pattern to copy): the wheel widgets each carry a private clone
of the same helper, `raw_wheel_events_delta` — `src/widgets/wheel_slider.rs:438`,
`src/widgets/wheel_spin_box.rs:284`, `src/widgets/wheel_combo_box.rs:326`. They do **not** call
`input_util`. New code should call `crate::input_util::raw_wheel_delta`.

## REMOVED: `Context::is_pointer_over_area` — and `is_pointer_over_egui` is a trap here

`grep -rn is_pointer_over_area egui-0.35.0/src/` → **0 hits** (it existed at
`egui-0.31.1/src/context.rs:2581`). The surviving `Context::is_pointer_over_egui`
(`egui-0.35.0/src/context.rs:2841`) is **not** a drop-in: with a space-filling `CentralPanel` the root
ui's available rect is empty, so it reports `true` for every point over the central content. That
permanently suppressed deselect-on-empty-click in the typing tab (see the doc comment at
`src/input_util.rs:45-56`).

**Project replacement:**

```rust
// src/input_util.rs:58
pub fn pointer_over_floating_area(ctx: &egui::Context) -> bool
// = ctx.layer_id_at(interact_pos).is_some_and(|l| l.order != egui::Order::Background)
```
Built on `Context::layer_id_at` (`egui-0.35.0/src/context.rs:3002`) and `Order`
(`egui-0.35.0/src/layers.rs:10`: `Background, Middle, Foreground, Tooltip, Debug`). It answers
"is a `Window`/menu/popup/tooltip drawn above this point", which is the question canvas code actually
has. Callers: `src/tabs/typing/tab.rs:684`, `src/tabs/typing/tab/draw_page.rs:1139`,
`src/tabs/typing/tab/selection_rasters.rs:1224`.

Which to use: `pointer_over_floating_area` for "did this canvas click land on bare canvas";
`egui_wants_keyboard_input()` (`context.rs:2884`) for "is a text field eating my key presses".

## PointerState

`egui-0.35.0/src/input_state/mod.rs:984`. All fields are **private** — use the accessors:

```rust
latest_pos() -> Option<Pos2>      // :1307  last known position (even if the button is down elsewhere)
hover_pos()  -> Option<Pos2>      // :1313  == latest_pos
interact_pos() -> Option<Pos2>    // :1323  press origin while dragging, else latest_pos
delta() -> Vec2                   // :1256  movement this frame
velocity() -> Vec2                // :1273
press_origin() -> Option<Pos2>    // :1288
total_drag_delta() -> Option<Vec2>// :1293
primary_down() -> bool            // :1536  (= button_down(PointerButton::Primary), :1478)
any_down() / any_pressed() / any_released() / any_click()   // :1411 / :1365 / :1370 / :1416
primary_pressed() / primary_clicked()                        // :1389 / :1462
is_decidedly_dragging() -> bool   // :1512  (down/released, not a click candidate)
is_moving() / is_still()          // :1345 / :1338
```

## Response-based input — the correct default

Prefer `Response` over raw pointer reads: egui already resolved occlusion, layer order and widget
interception for you. A raw `pointer.interact_pos()` read fires **through** windows, modals and
overlays drawn on top of your widget (see `06-overlays.md`).

```rust
// egui-0.35.0/src/response.rs
clicked() -> bool                    // :183   (clicked_by(button) :196, secondary_clicked :210,
                                     //         double_clicked :236)
hovered() -> bool                    // :313   pointer over it AND nothing covering it
contains_pointer() -> bool           // :326   inside the rect and not covered
dragged() -> bool                    // :416   (drag_started :386, drag_stopped :428)
drag_delta() -> Vec2                 // :439   frame delta, already scaled by the layer transform
drag_motion() -> Vec2                // :471   raw pointer motion (ignores layer transform)
interact_pointer_pos() -> Option<Pos2> // :529 pointer pos, only while interacting
hover_pos() -> Option<Pos2>          // :556
is_pointer_button_down_on() -> bool  // :575
changed() -> bool                    // :593
```
`drag_delta()` returns `Vec2::ZERO` unless `dragged()` (`response.rs:440-448`) — no need to guard it.

## Sense — a bitflags struct in 0.35

`egui-0.35.0/src/sense.rs:4` — `pub struct Sense(u8)` with `bitflags!` (:6): `HOVER = 0` (:9),
`CLICK = 1<<0` (:12), `DRAG = 1<<1` (:15), `FOCUSABLE = 1<<2` (:21). It is a bitflag struct, not the
old `struct { click, drag, focusable }` of ancient egui, so `Sense::CLICK | Sense::DRAG` works.

Constructors: `Sense::hover()` (:45), `Sense::click()` (:60), `Sense::drag()` (:68),
`Sense::click_and_drag()` (:81), `Sense::focusable_noninteractive()` (:52).
Queries: `interactive()` (:87), `senses_click()` (:92), `senses_drag()` (:97), `is_focusable()` (:102).

`hover()` senses nothing — the widget still gets `hovered()`/`contains_pointer()`, but never
`clicked()`/`dragged()`. Pass the sense to `ui.allocate_exact_size` / `allocate_rect` (see `02-painting.md`).

## Keyboard

```rust
// egui-0.35.0/src/data/input/modifiers.rs:19
pub struct Modifiers { pub alt, pub ctrl, pub shift, pub mac_cmd, pub command: bool }
// consts NONE :75, ALT :83, CTRL :90, SHIFT :97, MAC_CMD :106, COMMAND :115
// matches_logically(pattern) :211  — ignores extra modifiers per egui's rules
// matches_exact(pattern) :253
```
Always compare shortcuts against `command`, not `ctrl`, so macOS ⌘ works (`modifiers.rs:33-37`).

```rust
// egui-0.35.0/src/data/input/keyboard_shortcut.rs:11
pub struct KeyboardShortcut { pub modifiers: Modifiers, pub logical_key: Key }  // ::new(..) :18

// egui-0.35.0/src/input_state/mod.rs
consume_shortcut(&mut self, &KeyboardShortcut) -> bool   // :732 — removes the event so nothing else sees it
consume_key(&mut self, Modifiers, Key) -> bool           // :719
count_and_consume_key(..) -> usize                       // :688
key_pressed(Key) / key_down(Key) / key_released(Key)     // :743 / :766 / :771
```
`consume_*` need `ctx.input_mut(..)`; the non-consuming reads take `ctx.input(..)`.

### Project layer: `InputManagerV2` — add hotkeys HERE, not ad hoc

`src/input_manager_v2.rs`:
- `HotkeySpecV2` (:40) — registration record: `id`, `title`, `section`, `default_shortcut:
  Option<egui::KeyboardShortcut>`, `default_modifier_only: Option<ModifierOnlyV2>`, `scope:
  HotkeyScopeV2` (`Global` | `Tab(AppTab)`, :34), `active_when_input: bool`.
- `ModifierOnlyV2` (:58) — `Ctrl | Alt | Shift`: binds that are *just* a held modifier, which
  `KeyboardShortcut` cannot express. `matches()` (:73) treats `ctrl || command` as Ctrl.
  Query them per-frame with `modifier_only_active(ctx, id)` (:257) — they are level-triggered, and
  `collect_triggered` deliberately skips them (:286-288).
- `InputManagerV2::register` (:122), `set_shortcut` (:168), `set_modifier_only` (:198),
  `clear_binding` (:184), `reset_to_default` (:214), `shortcut_text` (:243); overrides persist via
  `load_overrides` (:150) / `save_hotkey_override` (:311).
- `collect_triggered(ctx, active_tab) -> Vec<String>` (:270) — the dispatcher. It skips disabled and
  out-of-scope commands, skips everything when `ctx.egui_wants_keyboard_input()` unless
  `active_when_input`, then `ctx.input_mut(|i| i.consume_shortcut(&shortcut))` and fires **only on the
  rising edge** (`last_shortcut_held`, :293-304) so a held key does not repeat.

Dispatch: `MangaApp::dispatch_hotkeys` (`src/app.rs:1562`) calls `collect_triggered` and routes each id
through `MangaApp::execute_hotkey_command` (`src/app.rs:1571`).

**Rule: a new hotkey = a new `HotkeySpecV2` + an arm in `execute_hotkey_command`.** Do not sprinkle
`input.key_pressed(..)` — that bypasses rebinding, scoping, text-field suppression and edge-triggering.

## Event enum

`egui-0.35.0/src/data/input/event.rs:17`. Variants worth knowing:

```rust
Copy (:19), Cut (:22), Paste(String) (:25), Text(String) (:30),
Key { key: Key, physical_key: Option<Key>, pressed: bool, repeat: bool, modifiers: Modifiers } (:37),
PointerMoved(Pos2) (:73), PointerButton { .. } (:82), PointerGone (:101),
Zoom(f32) (:113), Rotate(f32) (:116), Ime(ImeEvent) (:119), Touch { .. } (:123),
MouseWheel { unit: MouseWheelUnit, delta: Vec2, phase: TouchPhase, modifiers: Modifiers } (:147),
WindowFocused(bool) (:172), AccessKitActionRequest(..) (:175),
Screenshot { viewport_id: ViewportId, user_data: UserData, image: Arc<ColorImage> } (:178)
```
`Text` is the character stream (use it for text entry); `Key` is the physical/logical press-release
stream (use it for shortcuts). `physical_key` ignores keymaps — only for game-style WASD.

`Event::Screenshot` powers the viewport eyedropper: `src/widgets/viewport_color_selector.rs` requests
a frame with `ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(token)))`
(:129-131) and picks the reply back up in `poll_screenshot_events` (:139-159), matching
`user_data.data.downcast_ref::<u64>()` against its own token before keeping `Arc<ColorImage>`.

## Text-edit cursor types (0.35 change)

- `epaint::text::ByteIndex(pub usize)` (`epaint-0.35.0/src/text/index.rs:19`) — slices a `&str`.
- `epaint::text::CharIndex(pub usize)` (`index.rs:31`) — counts chars. Distinct newtypes on purpose.
- `CCursor { index: CharIndex, .. }` (`epaint-0.35.0/src/text/cursor.rs:10`), `CCursor::new(impl Into<CharIndex>)` (:23).
- `CCursorRange::as_sorted_char_range() -> std::ops::Range<CharIndex>`
  (`egui-0.35.0/src/text_selection/cursor_range.rs:52`) — in older egui this was `Range<usize>`.
  Unwrap the newtype at the boundary, as the repo does:
  `.map(|range| range.start.0..range.end.0)` (`src/tabs/typing/panel/create_edit.rs:944-948`;
  selection is written back with `CCursorRange::two(CCursor::new(a), CCursor::new(b))` at :935).

## Editing map

- Wheel/zoom on any canvas → `crate::input_util::raw_wheel_delta` (`src/input_util.rs:34`).
- "Did the click hit bare canvas?" → `crate::input_util::pointer_over_floating_area` (`src/input_util.rs:58`).
- New hotkey → register a `HotkeySpecV2` (`src/input_manager_v2.rs`) + handle it in
  `MangaApp::execute_hotkey_command` (`src/app.rs:1571`).
- Widget-local interaction → allocate with the right `Sense` and read the `Response`; do not read
  `ctx.input().pointer` for widget hit-testing.
- Screenshot/eyedropper flow → `src/widgets/viewport_color_selector.rs`.
- Wheel widgets (slider/spin box/combo box) → `src/widgets/wheel_*.rs` (each has a local wheel-delta
  clone; prefer `input_util` in new code).
