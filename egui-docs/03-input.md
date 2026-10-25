# 03 — Input: events, pointer, keyboard, scroll

Target version: **egui 0.36.2** (`Cargo.toml:46`). Two widely-remembered APIs **no longer exist** in
0.36 and this project has explicit replacements for both — read the "Removed" sections before writing
any input code.

## Reading input

```rust
// egui-0.36.2/src/context.rs:991 / :1003
pub fn input<R>(&self, reader: impl FnOnce(&InputState) -> R) -> R;
pub fn input_mut<R>(&self, writer: impl FnOnce(&mut InputState) -> R) -> R;
```
The closure holds a lock — keep it tiny, copy the values out, and **never call another `ctx.input*`,
`ui.*`, or repaint from inside it** (re-entrant deadlock). The repo's pattern is to destructure in one
shot and copy the values out, e.g. `crates/ms-widgets/src/input_util.rs:56`.

`InputState` (`egui-0.36.2/src/input_state/mod.rs:213`) — public fields:

| field | line | notes |
|---|---|---|
| `raw: RawInput` | :215 | what the backend sent this frame |
| `pointer: PointerState` | :218 | mouse / single-finger touch |
| `smooth_scroll_delta: Vec2` | :241 | ramped over frames; **zeroed while the zoom modifier is held** |
| `pixels_per_point: f32` | :263 | |
| `max_texture_side: usize` | :268 | |
| `time: f64`, `unstable_dt`, `predicted_dt`, `stable_dt` | :271/:277/:285/:310 | use `stable_dt` for animation |
| `focused: bool` | :315 | window has keyboard focus |
| `modifiers: Modifiers` | :318 | 0.36: folded from this frame's `Event::ModifiersChanged` (:431-433), reset on focus loss (:447); `RawInput` has no `modifiers` field any more |
| `keys_down: HashSet<Key>` | :323 | |
| `events: Vec<Event>` | :326 | in-order events this frame |

Private-with-accessor: `zoom_factor_delta` → `zoom_delta()` (:562) and `zoom_delta_2d()` (:585);
`rotation_radians` → `rotation_delta()` (:617); `viewport_rect` → `viewport_rect()` (:524) /
`content_rect()` (:510); `smooth_scroll_delta()` also exists as a method (:550).
Also: `is_scrolling()` (:638), `time_since_last_scroll()` (:644), `key_pressed/key_down/key_released`
(:746/:769/:774), `multi_touch()` (:834).

## REMOVED: `InputState::raw_scroll_delta`

`grep -rn raw_scroll_delta egui-0.36.2/src/` → **0 hits**. It existed in 0.31
(`egui-0.31.1/src/input_state/mod.rs:136`). It is gone.

Why it matters: under the zoom modifier (Ctrl/Cmd) egui diverts the wheel into `zoom_delta` and leaves
`smooth_scroll_delta` at zero, so Ctrl+wheel handlers that read `smooth_scroll_delta` see nothing.

**Project replacement — call this, do not reinvent it:**

```rust
// crates/ms-widgets/src/input_util.rs:31
pub fn raw_wheel_delta(input: &egui::InputState) -> egui::Vec2
```
It sums the frame's `Event::MouseWheel { delta, .. }` values (`crates/ms-widgets/src/input_util.rs:32-39`).
Sign matches `smooth_scroll_delta` (positive Y = content moved down). The magnitude is
**unit-dependent** — `MouseWheelUnit::{Point, Line, Page}`
(`egui-0.36.2/src/data/input/mouse_wheel_unit.rs`) — so only use it for sign/threshold decisions,
never as a pixel distance. Unit tests: `crates/ms-widgets/src/input_util.rs:63-95`.

Callers: the page manager's boards and viewer (`crates/ms-tab-page-manager/src/{crop,split,stitch,viewer}.rs`).
The input-migrated code (canvas, Translation / Cleaning / Typing, tools) never reads the wheel itself:
a wheel binding is an `ms-input` axis (`view.zoom`, brush size, rotation), whose router reads the raw
`Event::MouseWheel` the same way and reports notches (see "Project layer: `ms-input`" below).

Caveat (pre-existing duplication, not a pattern to copy): the wheel widgets each carry a private clone
of the same helper, `raw_wheel_events_delta` — `crates/ms-widgets/src/wheel_slider.rs:438`,
`crates/ms-widgets/src/wheel_spin_box.rs:284`, `crates/ms-widgets/src/wheel_combo_box.rs:326`. They do **not** call
`input_util`. New code should call `ms_widgets::input_util::raw_wheel_delta`.

## REMOVED: `Context::is_pointer_over_area` — and `is_pointer_over_egui` is a trap here

`grep -rn is_pointer_over_area egui-0.36.2/src/` → **0 hits** (it existed at
`egui-0.31.1/src/context.rs:2581`). The surviving `Context::is_pointer_over_egui`
(`egui-0.36.2/src/context.rs:2943`) is **not** a drop-in: with a space-filling `CentralPanel` the root
ui's available rect is empty, so it reports `true` for every point over the central content. That
permanently suppressed deselect-on-empty-click in the typing tab.

**Project replacement:** `ms_input::Condition::PointerOnView`. A viewer publishes its surface to the
input router each frame; the router (`crates/ms-input/src/router.rs`, `surface_at`) counts the
pointer as on that view only where `Context::layer_id_at` (`egui-0.36.2/src/context.rs:3104`)
reports an `Order::Background` layer or the view's own capture layer (`Order` at
`egui-0.36.2/src/layers.rs:10`: `Background, Middle, Foreground, Tooltip, Debug`). That answers
"is a `Window`/menu/popup/tooltip/dock panel drawn above this point", which is the question canvas
code actually has. Canvas actions declare the condition; nothing tests the layer by hand.

Which to use: `Condition::PointerOnView` for "did this canvas click land on bare canvas";
`egui_wants_keyboard_input()` (`context.rs:2986`) for "is a text field eating my key presses".

## PointerState

`egui-0.36.2/src/input_state/mod.rs:987`. All fields are **private** — use the accessors:

```rust
latest_pos() -> Option<Pos2>      // :1310  last known position (even if the button is down elsewhere)
hover_pos()  -> Option<Pos2>      // :1316  == latest_pos
interact_pos() -> Option<Pos2>    // :1326  press origin while dragging, else latest_pos
delta() -> Vec2                   // :1259  movement this frame
velocity() -> Vec2                // :1276
press_origin() -> Option<Pos2>    // :1291
total_drag_delta() -> Option<Vec2>// :1296
primary_down() -> bool            // :1539  (= button_down(PointerButton::Primary), :1481)
any_down() / any_pressed() / any_released() / any_click()   // :1414 / :1368 / :1373 / :1419
primary_pressed() / primary_clicked()                        // :1392 / :1465
is_decidedly_dragging() -> bool   // :1515  (down/released, not a click candidate)
is_moving() / is_still()          // :1348 / :1341
```

## Response-based input — the correct default

Prefer `Response` over raw pointer reads: egui already resolved occlusion, layer order and widget
interception for you. A raw `pointer.interact_pos()` read fires **through** windows, modals and
overlays drawn on top of your widget (see `06-overlays.md`).

```rust
// egui-0.36.2/src/response.rs
clicked() -> bool                    // :184   (clicked_by(button) :197, secondary_clicked :211,
                                     //         double_clicked :237)
hovered() -> bool                    // :320   pointer over it AND nothing covering it
contains_pointer() -> bool           // :333   inside the rect and not covered
dragged() -> bool                    // :433   (drag_started :393, drag_stopped :445)
drag_delta() -> Vec2                 // :456   frame delta, already scaled by the layer transform
drag_motion() -> Vec2                // :488   raw pointer motion (ignores layer transform)
interact_pointer_pos() -> Option<Pos2> // :546 pointer pos, only while interacting
hover_pos() -> Option<Pos2>          // :573
is_pointer_button_down_on() -> bool  // :595
changed() -> bool                    // :613
```
`drag_delta()` returns `Vec2::ZERO` unless `dragged()` (`response.rs:457-465`) — no need to guard it.

## Sense — a bitflags struct in 0.36

`egui-0.36.2/src/sense.rs:4` — `pub struct Sense(u8)` with `bitflags!` (:6): `HOVER = 0` (:9),
`CLICK = 1<<0` (:12), `DRAG = 1<<1` (:15), `FOCUSABLE = 1<<2` (:21). It is a bitflag struct, not the
old `struct { click, drag, focusable }` of ancient egui, so `Sense::CLICK | Sense::DRAG` works.

Constructors: `Sense::hover()` (:45), `Sense::click()` (:60), `Sense::drag()` (:68),
`Sense::click_and_drag()` (:81), `Sense::focusable_noninteractive()` (:52).
Queries: `interactive()` (:87), `senses_click()` (:92), `senses_drag()` (:97), `is_focusable()` (:102).

`hover()` senses nothing — the widget still gets `hovered()`/`contains_pointer()`, but never
`clicked()`/`dragged()`. Pass the sense to `ui.allocate_exact_size` / `allocate_rect` (see `02-painting.md`).

## Keyboard

```rust
// egui-0.36.2/src/data/input/modifiers.rs:19
pub struct Modifiers { pub alt, pub ctrl, pub shift, pub mac_cmd, pub command: bool }
// consts NONE :75, ALT :83, CTRL :90, SHIFT :97, MAC_CMD :106, COMMAND :115
// matches_logically(pattern) :211  — ignores extra modifiers per egui's rules
// matches_exact(pattern) :253
```
Always compare shortcuts against `command`, not `ctrl`, so macOS ⌘ works (`modifiers.rs:33-37`).

```rust
// egui-0.36.2/src/data/input/keyboard_shortcut.rs:11
pub struct KeyboardShortcut { pub modifiers: Modifiers, pub logical_key: Key }  // ::new(..) :18

// egui-0.36.2/src/input_state/mod.rs
consume_shortcut(&mut self, &KeyboardShortcut) -> bool   // :735 — removes the event so nothing else sees it
consume_key(&mut self, Modifiers, Key) -> bool           // :722
count_and_consume_key(..) -> usize                       // :691
key_pressed(Key) / key_down(Key) / key_released(Key)     // :746 / :769 / :774
```
`consume_*` need `ctx.input_mut(..)`; the non-consuming reads take `ctx.input(..)`.

### Project layer: `ms-input` — every binding goes HERE, not ad hoc

The studio's keys, modifiers, wheel axes and pointer gestures are actions of the `ms-input` crate
(`crates/ms-input/MODULE_README.md`); the Translation, Cleaning and Typing tabs, the canvas and the
tools read no key, modifier or wheel themselves. Do not sprinkle `input.key_pressed(..)` /
`consume_key(..)` — that bypasses rebinding, alternatives, contexts, conflicts, text-field
suppression and the displayed labels.

- **Declare** (pure model, no `Context`): an `ActionSpec` of one `ActionKind`
  (`crates/ms-input/src/action.rs:40`: `Command` edge, `Mode` held, `Axis` continuous, `Gesture`
  on a declared `TargetSpec`, :187) with an `ActionMeta` (:106: id = persistence key, title key,
  section, `Scope` :58, `when` conditions, default `Binding`s), collected in `Declarations`
  (:254). Owners: the shared viewer set `ms_input::view::declarations`
  (`crates/ms-input/src/view.rs:66`), the brush set in `ms-tools`, each tab's
  `StudioTab::input_declarations` (`crates/ms-studio-tab/src/lib.rs:38`) and the session's own;
  merged by `build_input_registry` (`src/studio.rs:1129`) into one `InputRegistry`
  (`crates/ms-input/src/registry.rs:147`, `new` :171) with the user's overrides applied
  (`apply_overrides` :360; codec `crates/ms-input/src/persist.rs`, `load_overrides` :339,
  `user_config` -> `Hotkeys` format `HOTKEYS_FORMAT` :47).
- **Contexts**: `Condition` (`crates/ms-input/src/context.rs:38`: `NoPopup`, `PointerOnView`,
  `ViewFocus`, `Context(ContextId)`, `NotContext(..)`). A tab publishes its current contexts
  (tool, edit mode, open window) through `StudioTab::input_contexts`
  (`crates/ms-studio-tab/src/lib.rs:46`), passed in `FrameEnv` (`crates/ms-input/src/frame.rs:45`).
- **Frame order**: `InputRouter::begin_frame(ctx, &FrameEnv)` (`crates/ms-input/src/router.rs:203`)
  runs ONCE per frame before any tab draws (`src/studio.rs:923`); it turns `Event::Key` (physical
  key), modifiers, pointer buttons, `MouseWheel` and `Zoom` into matcher events
  (`MatchState::step`, `crates/ms-input/src/matching.rs:139`), consumes only matched keys and owned
  wheel input, and returns the `InputFrame` (`crates/ms-input/src/frame.rs:217`) every consumer
  reads: `fired` (:269), `mode_active` (:281), `axis` / `axis_samples` (:289/:294),
  `command_held` (:313), `claimed` (:319), `canvas_wide_armed` (:327), `gesture_for` (:360).
  Key chords never match while a `TextEdit` has focus; commands never fire while a popup is open.
- **Viewers** publish a `ViewSurface` (`crates/ms-input/src/frame.rs:66`) with
  `publish_view_surface` (:100) while drawing; the next `begin_frame` uses it for `PointerOnView`,
  command focus and canvas-wide gesture claims (claimed at the press, through the viewer's capture
  `Area`). Targeted gestures keep egui's hit test and ask `InputFrame::gesture_for(response, ..)`.
  The canvas consumes its frame in `CanvasView::apply_input_frame`
  (`crates/ms-canvas/src/lib.rs:1982`).
- **Labels**: hint and menu text names a binding only through `InputRegistry::label`
  (`crates/ms-input/src/label.rs:158`, alternatives joined by " / "), `label_with_prefix` (:171) or
  `param_group_label` (:226); words come from `input.*` keys. `crates/ms-i18n/tests/binding_text.rs`
  fails on a binding spelled in the migrated locale namespaces.
- **Conflicts** on one scope level and cross-level shadows: `InputRegistry::conflicts` /
  `conflicts_for` / `shadows_for` (`crates/ms-input/src/registry.rs:547/:553/:559`), shown by
  Settings › Hotkeys (`src/tabs/settings/hotkeys/`).
- **Capture** (settings pane): `capture_begin` / `capture_set_mouse_zone` / `capture_finish` /
  `capture_cancel` (`crates/ms-input/src/router.rs:313/:320/:338/:347`); the pane applies the
  result with `InputRegistry::set_bindings` (`crates/ms-input/src/registry.rs:436`) through
  `InputRouter::registry_mut` (`router.rs:191`) and persists it on a worker (`write_override`,
  `persist.rs:569`).
- **Touch**: finger gestures are `ms_input::touch` (section "Touch on canvases" below); a
  long-press drag reaches the input router as the `Touch(LongPressDrag)` alternative of a
  canvas-wide gesture (`InputFrame::touch_selection_available`, `frame.rs:334`).

**Rule: a new shortcut or tool gesture = a declared action + a read of the `InputFrame` in its
owner.** The PS editor, page manager and launcher are not migrated yet and still read raw keys.

## Event enum

`egui-0.36.2/src/data/input/event.rs:17`. Variants worth knowing:

```rust
Copy (:19), Cut (:22), Paste(String) (:25), Text(String) (:30),
Key { key: Key, physical_key: Option<Key>, pressed: bool, repeat: bool, modifiers: Modifiers } (:37),
ModifiersChanged(Modifiers) (:73),   // new in 0.36
PointerMoved(Pos2) (:76), PointerButton { .. } (:85), PointerGone (:104),
Zoom(f32) (:116), Rotate(f32) (:119), Ime(ImeEvent) (:122), Touch { .. } (:126),
MouseWheel { unit: MouseWheelUnit, delta: Vec2, phase: TouchPhase, modifiers: Modifiers } (:150),
WindowFocused(bool) (:175), AccessKitActionRequest(..) (:178),
Screenshot { viewport_id: ViewportId, user_data: UserData, image: Arc<ColorImage> } (:181)
```
`Text` is the character stream (use it for text entry); `Key` is the physical/logical press-release
stream (use it for shortcuts). `physical_key` ignores keymaps — only for game-style WASD.

`Event::Screenshot` powers the viewport eyedropper: `crates/ms-widgets/src/viewport_color_selector.rs` requests
a frame with `ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(token)))`
(:129-131) and picks the reply back up in `poll_screenshot_events` (:139-159), matching
`user_data.data.downcast_ref::<u64>()` against its own token before keeping `Arc<ColorImage>`.

## Text-edit cursor types (0.35 change)

- `epaint::text::ByteIndex(pub usize)` (`epaint-0.36.2/src/text/index.rs:19`) — slices a `&str`.
- `epaint::text::CharIndex(pub usize)` (`index.rs:31`) — counts chars. Distinct newtypes on purpose.
- `CCursor { index: CharIndex, .. }` (`epaint-0.36.2/src/text/cursor.rs:10`), `CCursor::new(impl Into<CharIndex>)` (:23).
- `CCursorRange::as_sorted_char_range() -> core::ops::Range<CharIndex>`
  (`egui-0.36.2/src/text_selection/cursor_range.rs:52`) — in older egui this was `Range<usize>`.
  Unwrap the newtype at the boundary, as the repo does:
  `.map(|range| range.start.0..range.end.0)` (`src/tabs/typing/panel/create_edit.rs:944-948`;
  selection is written back with `CCursorRange::two(CCursor::new(a), CCursor::new(b))` at :935).

## Touch on canvases

Facts the canvas finger gestures rely on (owner: `ms_input::touch`,
`crates/ms-input/src/touch/`):

- **egui-winit emulates the mouse from ONE touch.** `on_touch` pushes `Event::Touch` first and
  then, only for the touch that owns the emulated pointer (`pointer_touch_id`, the first touch of
  any device), Start → `PointerMoved` + `PointerButton{Primary, pressed}`, Move → `PointerMoved`,
  End → `PointerButton{released}` + `PointerGone`, Cancel → `PointerGone`
  (`egui-winit-0.36.2/src/lib.rs:878-935`; the fork's copy is
  `crates/egui/crates/egui-winit/src/lib.rs` `on_touch`, :920). The release carries the last
  MOVE position (`on_mouse_button_input` uses `pointer_pos_in_points`), not the End position.
- **`eframe::App::raw_input_hook`** (`eframe-0.36.2/src/epi.rs:279`) sees that `RawInput` before
  egui builds its pointer state, so removing the emulated events there means egui never sees a
  press, click, drag or long-touch from that finger. It also runs for child viewports: check
  `raw_input.viewport_id`.
- **egui's own touch gestures are device-wide**: `InputState::multi_touch()`
  (`egui-0.36.2/src/input_state/mod.rs:834`) mixes every finger, including one resting on a
  panel; a long touch needs the emulated pointer pressed (`is_long_touch`, :917) and becomes
  `Response::secondary_clicked()` (`egui-0.36.2/src/response.rs:211`).
- **`Context::layer_id_at` over bare canvas returns the background layer**
  (`Some(LayerId::background())`, pinned by the `ms_input::touch` test
  `layer_id_at_over_bare_canvas_is_the_background_layer`), a Foreground area as itself, and the
  top modal layer while a modal is open (`egui-0.36.2/src/memory/mod.rs:838-845`). It walks only
  layers that have AREA state (`Areas::layer_id_at`, `egui-0.36.2/src/memory/mod.rs:1245-1267`): a
  bare `ctx.layer_painter(LayerId::new(Order::Middle, …))` is never reported, so it neither blocks
  a claim nor needs a capture registration (pinned by `bare_layer_painter_does_not_block_a_claim`).
  An `Area` over a canvas (a tool's capture overlay) does block it until the tool registers it
  with `ms_input::touch::publish_capture_layer`; widgets inside such an area that a finger must still
  operate are exempted with `ms_input::touch::publish_hole`.
- **egui's hit test grows every widget by `interact_radius`** read from the GLOBAL style
  (`egui-0.36.2/src/context.rs:486`), so a touch hole meant to cover a small handle is the
  handle's hit rect expanded by that radius (`ms-canvas` `note_touch_handle_hole`).
- **`Popup::context_menu` anchors at the hover position of the opening frame**
  (`egui-0.36.2/src/containers/popup.rs:248-260`, stored by the `pub(crate)`
  `Memory::open_popup_at`, :522-523). A claimed finger has no hover position, so canvas menus use
  `ms_input::touch::context_menu_popup(response, long_press_at)`, which records the anchor
  itself and is identical for the mouse (characterization test in `crates/ms-input/src/touch/tests.rs`).
- **A repaint requested before the pass is lost**: `begin_pass_repaint_logic` resets the delay
  (`egui-0.36.2/src/context.rs:99-115`), so a timer decided in the hook is requested from inside
  the pass (`ms_input::touch::take_touch_output`).
- **`ScrollArea`'s kinetic fling cannot be driven from outside**: its velocity is the private
  `State::vel` (getter only, `egui-0.36.2/src/containers/scroll_area.rs:44,84`), set from
  `pointer.velocity()` on `drag_stopped` and decayed with friction 1000 pt/s², stop speed
  20 pt/s and `dt.at_most(0.1)` (:829, :859-877). A claimed finger never drags it, so
  `ms_input::touch` reruns the same law itself over an `egui::util::History<Pos2>` with egui's pointer
  window (`History::new(2..1000, 0.1)`, velocity only with ≥3 samples over >0.01 s,
  `input_state/mod.rs:1079,1234`).

## Editing map

- Finger pan / pinch / fling / tap / long-press on a canvas → `ms_input::touch` (publish a
  `TouchSurface`, take `TouchOutput`, open menus with `context_menu_popup`); never read
  `multi_touch()` or touch events directly. A tab mode with a selection drag on a migrated canvas
  → give its canvas-wide gesture a `Touch(LongPressDrag)` alternative (the router claims it);
  never synthesize a mouse press.
- Wheel/zoom on a migrated canvas → an `ms-input` axis (`view.zoom` is shared); elsewhere (page
  manager) `ms_widgets::input_util::raw_wheel_delta` (`crates/ms-widgets/src/input_util.rs:31`).
- "Did the click hit bare canvas?" → declare the action with `ms_input::Condition::PointerOnView`
  (`crates/ms-input/src/context.rs`); the router's `surface_at` does the z-order test.
- New hotkey, held modifier, wheel axis or tool gesture → declare it in its owner's `ms-input`
  declarations and read the `InputFrame` (`crates/ms-input/MODULE_README.md`, "Editing map");
  display it with `InputRegistry::label`.
- Widget-local interaction → allocate with the right `Sense` and read the `Response`; do not read
  `ctx.input().pointer` for widget hit-testing.
- Screenshot/eyedropper flow → `crates/ms-widgets/src/viewport_color_selector.rs`.
- Wheel widgets (slider/spin box/combo box) → `crates/ms-widgets/src/wheel_*.rs` (each has a local wheel-delta
  clone; prefer `input_util` in new code).
