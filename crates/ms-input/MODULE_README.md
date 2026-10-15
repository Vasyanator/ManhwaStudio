# Module: crates/ms-input

## Purpose
The unified input model of the studio: which physical inputs exist, which actions code declares,
how the user rebinds them (any number of alternatives per action), how presses are matched to
actions, which bindings conflict or shadow each other, how a binding is displayed, and how
overrides persist in `user_config` -> `Hotkeys`. One owner for bindings, binding labels and
their conflicts: no tab, canvas or tool spells a binding itself.

## Architecture
Layer: the `ms-page-ops / ms-sysprobe / ms-onnx-runtime / ms-window-geometry / ms-os-integration`
line, BELOW `ms-widgets`. Dependencies: `egui` (the model uses only `egui::Key`; the runtime and
`touch/` read the `egui::Context`), `ms-config` (`AppTab`,
`update_user_config_file`), `ms-i18n`, `ms-log`, `serde_json`, `thiserror`, `anyhow` (the
mutator boundary of `update_user_config_file` only). Forbidden: `ms-widgets`, any tab crate,
`ms-studio-tab`, the root package (they depend on this crate).

Data flow: each owner (the view set here, later tabs / tools / the session) returns
`Declarations`; the studio merges them into one `InputRegistry` (validated), applies the persisted
`HotkeyOverrides` (pre-read and, for a version-1 section, migrated by `load_overrides` on a
worker), and a runtime feeds `MatchEvent`s plus a per-frame `ActiveSet` into a `MatchState`.
Labels and conflict texts are read from the registry and cached by `revision()`.

Two layers in one crate: the pure MODEL (everything above, no egui `Context`) and the egui
RUNTIME on top of it (`router.rs`, `frame.rs`, `capture.rs`) plus the raw-stage touch router
(`touch/`). The runtime never re-implements matching: it turns egui events into `MatchEvent`s
and asks the matcher. Consumers (studio root, canvas, tabs, settings pane) are not in this
crate; every consumer (shell hook, canvas, tabs, PS editor) names `ms_input::touch` directly.

## Files and submodules
- `src/lib.rs`: crate root, lints (`clippy::pedantic`), i18n macros, re-exports.
- `src/button.rs`: `Button` (`Modifier`, `Key` = physical key, `Mouse`, `Touch`, `Unknown`).
- `src/action.rs`: `ActionId`, `ActionKind`, `Scope` / `ScopeLevel`, `ActionMeta`, the four
  specs, `FixedAction`, `TargetSpec`, `Declarations`.
- `src/binding.rs`: alternatives per kind (`Binding`), `Chord` (mode refs + buttons),
  `ButtonChord`, `BindingError`, `resolve_chord`.
- `src/context.rs`: `ContextId`, `Condition`, `ActiveSet`, `may_be_co_active`.
- `src/registry.rs`: `InputRegistry` (`new`, `empty` — the infallible inert registry of a session
  whose declarations failed —, `apply_overrides`, `set_bindings`, `reset`, queries), `OverrideEntry`, `DeclarationError`, `OverrideWarning`.
- `src/matching.rs`: `MatchState::step` and the mode / gesture / axis queries.
- `src/conflicts.rs`: `compute` -> `ConflictReport` (`Conflict`, `Shadow`).
- `src/label.rs`: `label`, `alternative_label`, `chord_label`, `param_group_label`,
  `conflict_text`, `shadow_text`, `button_label`.
- `src/persist.rs`: format-2 codec, `load_overrides` (+ v1 migration), `write_override`,
  `write_migrated_section`.
- `src/view.rs`: the shared `view.*` set (zoom modifier / pan modes, zoom commands — `=`,
  `Shift+=` and the keypad `+` for zoom-in, all under `ViewFocus` —, zoom axis, zoom drag on
  `view.surface`, fixed scroll rows).
- `src/router.rs`: `InputRouter` — `begin_frame` (events -> matcher -> `InputFrame`, key / wheel
  consumption, claims, axes, focus loss) and the capture entry points.
- `src/frame.rs`: `FrameEnv`, `ViewId`, `ViewSurface` + `publish_view_surface`, `InputFrame` and
  its outputs (`Fired`, `AxisSample`, `ViewFocus`, `ClaimedGesture`, `GestureHit`), `gesture_for`.
- `src/capture.rs`: binding capture (`CaptureSlot`, `CaptureStatus`, `CaptureError`, the
  recording state machine, `captured_alternatives`).
- `src/touch/`: the canvas touch-gesture router (raw-input stage, own `MODULE_README.md`);
  `peek_selection` is its non-consuming read for the input router.
- `src/tests/`: unit tests per contract (matching, conflicts, registry, labels, persistence, and
  `runtime_tests.rs`: headless `Context::run_ui` frames over synthetic `RawInput`).

## Contracts and invariants
- **Kinds are code-fixed**: Command (edge), Mode (held), Axis (continuous), Gesture (pointer
  gesture on a declared target). An override must match the action's kind.
- **Alternatives**: unlimited per action; `[]` = explicitly unbound. A command alternative is a
  sequence of chords; a chord = mode references + buttons. A mode alternative has NO references
  (rejected by the codec and by the type). Overrides may contain mode references; each must name
  a declared Mode. A reference stands for the mode's CURRENT buttons.
- **Limits** (`binding.rs`; overrides are untrusted input): at most 256 alternatives per action
  (`MAX_ALTERNATIVES`, a corruption guard, not a usability limit: alternatives are unlimited in practice), 4 mode references per chord (`MAX_MODE_REFS_PER_CHORD`) and 64 resolved
  button combinations per alternative, summed over its chords (`MAX_COMBINATIONS_PER_BINDING`,
  counted without materializing). `set_bindings` refuses an over-limit list, and refuses a mode
  change that would push a current dependent binding over the limit; `apply_overrides` skips such
  an entry whole with an `OverrideWarning` + log (mode entries first, in id order, then the rest
  against the new modes). `resolve_chord` never yields more than 64 sets whatever it is given
  (safety net for `reset`, which is never refused). Conflict analysis expands accepted modes up to
  `CONFLICT_EXPANSION_LIMIT` (1024) sets per chord and lists cut-short actions in
  `ConflictReport::truncated`.
- **`beats`** must be acyclic: a self-edge or cycle is a `DeclarationError::BeatsCycle`.
- **Matching** (full rules in the `matching.rs` header): physical keys; the held modifier set
  must equal the chord's exactly (modifiers of active ACCEPTED modes tolerated); a command fires
  on the completing press (a modifier may complete it) and that button stops activating modes
  until released (Z vs Ctrl+Z); ranking = longest sequence, most explained held buttons,
  [gestures: target priority], innermost scope, declaration order; sequences have no timeout;
  key chords never match while a text edit has focus; commands never fire while a popup is open;
  modes match by subset and `beats` orders them.
- **Conflicts** (`conflicts.rs` header): computed over every alternative pair of co-active
  actions on ONE scope level, on the same effective button sets the matcher explains (resolved
  chord, also united with any choice of accepted-mode alternatives) (equal chords, sequence prefixes, equal axis / gesture inputs, a
  one-chord command equal to a mode chord) plus reserved clipboard chords (Cmd+C/X/V; on Windows
  also Ctrl+Insert, Shift+Delete, Shift+Insert). Across levels an overlap is a `Shadow`
  (information, the inner action wins). Modes ordered by `beats` or by chord subset never
  conflict. Different tabs, contradictory contexts and contexts of one exclusive group are never
  co-active. Reported in full (`conflicts()`) and per action (`conflicts_for`, `shadows_for`).
- **Labels** (`label.rs` header): alternatives joined by " / ", chord parts by "+", sequence
  chords by ", "; a mode reference prints its current chord, parenthesized when it has several
  alternatives; parameter groups print "1-6"; empty prints `app.hotkey.no_binding`. Every word
  comes from an `input.*` key, one literal key per name (the i18n orphan test requires it).
- **Persistence**: `{"format": 2, "actions": {id: [alternative, ...]}}`, overridden actions only,
  each with its full list; an override equal to the defaults is never stored (`set_bindings`
  returns a delete). Version 1 (no `format`, non-empty) migrates purely in `load_overrides`:
  zoom ids of both tabs -> `view.zoom_*` (first non-default wins), OCR selection modes ->
  `translation.ocr.{quick,advanced}_mode`, the rest 1:1. Fields are read with the old decoder's
  exact tolerance (malformed `enabled` = true, bad shortcut = absent, unknown modifier-only token
  = absent; a disabled entry migrates to `[]` whatever else it holds); entries equal to their v1
  default and non-object entries are dropped one by one (logged), never the section. A section of an
  unknown / newer format is ignored and never rewritten. Writers go through
  `ms_config::update_user_config_file` (serialized RMW), block, and run on workers only.
- **Errors**: typed (`DeclarationError`, `BindingError`, `DecodeError`, `PersistError`);
  skipped overrides are logged and returned as `OverrideWarning`s.
- **Tests** never touch a real config: in-memory JSON, or a unique `std::env::temp_dir()`
  directory removed by the test.

## Runtime (router, frame, capture)
- **Frame order**: the studio calls `InputRouter::begin_frame(ctx, &FrameEnv)` ONCE per frame,
  before any tab draws and before anything reads keys, the wheel or drags; every consumer then
  reads the returned `InputFrame` (`fired`, `command_held`, `mode_active`, `axis` /
  `axis_samples` / `axis_for_view` / `axis_samples_for_view`, `claimed`, `canvas_wide_armed`,
  `gesture_for`, `touch_selection_available`). There is no end-of-frame
  call: publications are frozen by the next `begin_frame`. A rerun pass of egui's multi-pass
  (`request_discard`) calls it again with no events; it then behaves like a new frame (claims
  report `Active`, the first pass's publications are the frozen surfaces).
- **Events**: `Event::Key` -> `Button::Key(physical_key, else key)`; modifier KEYS (Shift /
  Ctrl / Alt / Super Left/Right) are not key buttons — the Shift / Command / Alt roles come from
  each event's `modifiers` and `ModifiersChanged`; the Super role from held Super keys (Control on
  macOS, where ⌘ is Command). Side-specific modifier keys are therefore never matched as keys.
  Pointer buttons -> `Button::Mouse`; `MouseWheel` -> `WheelY` / `WheelX`; `Zoom` -> `Pinch`
  (not `zoom_delta()`, which also folds Ctrl+wheel and multi-touch); pointer delta of a running
  mouse claim -> `DragX` / `DragY`.
- **Consumption**: only matched key presses (fired or advancing a sequence), with their repeats
  and release, and wheel input an axis owns. Wheel ownership is per component: an event whose
  every non-zero component is owned is removed, one with only some owned is replaced by a copy
  with those components zeroed. Pointer events are never removed (P2: claims work through the
  viewer's capture `Area`). Per smoothed axis (raw components mapped through egui's
  scroll-folding modifiers), `smooth_scroll_delta` is zeroed while only owned wheel input feeds
  it, also in later frames while egui ramps it out (`is_scrolling`), until input egui keeps
  reaches that axis.
- **Activity**: key chords never match while `Context::text_edit_focused` (P1); commands never
  fire while `Popup::is_any_open || Context::any_popup_open`; `FrameEnv::modal_open` and a running
  capture suppress everything (the matcher still tracks held buttons). Window focus loss resets
  held state, consumed keys, press snapshots, wheel remainders, cancels claims and drops the
  capture's forming chord.
- **View surfaces** (one-frame lag, like `touch/`): viewers call `publish_view_surface` while
  drawing; the next `begin_frame` freezes them. "Pointer on view" = inside the rect, outside its
  holes, and `Context::layer_id_at` is the background layer, `None` or the surface's
  `capture_layer` — dock panels, popups, menus and windows occlude. `Condition::ViewFocus` holds
  while some view takes focus-less input (pointer on a view, or a `primary` surface published):
  the zoom commands need it, so a tab that draws no viewer keeps their keys. Overlapping surfaces: among
  those passing that rule, the one whose `capture_layer` is the top layer wins, else the one
  published LATER in the frame. Focus of commands and axes =
  the view under the pointer at the pointer, else the first `primary` view at its centre (P7).
- **Claims** (P2): a canvas-wide gesture is a `Drag` or `Click` binding on a `canvas_wide`
  target (`ClaimedGesture::kind`; a `Drag` wins when both match the press). A `Click` is a press
  action: claimed and acted on at the press (`Started`), never waiting to see whether the pointer
  moves. It is ARMED (`canvas_wide_armed`) while the pointer is on the view and a press would
  match; the viewer then raises its capture `Area`. Only a press starts a claim: an input that did
  not match at its press is never claimed later, even if its condition starts to hold while the
  button stays down. A pointer press inside last frame's surface that matches becomes CLAIMED: `Started` (press frame), `Active`, `Finished(Commit)` on the button release,
  `Finished(policy)` when a mode referenced by the WINNING alternative (`GestureMatch::modes`) is
  released mid-gesture
  (`ModeSpec::on_release_mid_gesture`, Cancel wins), `Finished(Cancel)` on focus loss or
  suppression. One claim per view; a finished claim is reported for one frame. Other kinds on
  canvas-wide targets, and every targeted gesture, resolve through `gesture_for`.
- **`gesture_for`**: per pointer button the router keeps a press snapshot (matcher state +
  activity at the press) until the frame after its release; the gesture is matched with that
  snapshot (press-time chord, `AtPress` params), `LatestDuringGesture` params are sampled now.
- **Touch alternatives** (P8): for every surface with a `touch_owner`, the router peeks the touch
  router's selection drag (`touch::peek_selection`) and claims a canvas-wide
  `Touch(LongPressDrag)` `Drag` binding with it (`Finished` per the selection's end; a claim whose
  owner got no selection this frame is cancelled). `touch_selection_available(view)` feeds
  `TouchSurface::long_press_selection`.
- **Command held**: `InputFrame::command_held(id)` is true while the button that fired `id` stays
  down (the matcher's fired-by record: auto-repeats keep it, release / focus loss end it).
- **Axis values** (`AxisSample`, one per axis AND source per frame — `source` decides the unit, so
  a frame feeding one axis from the wheel and a pinch reports two samples; `axis` returns the
  first, consumers of several sources iterate `axis_samples`): wheel `delta` in notches (Line / Page value, Point value / 40)
  times sensitivity, sign as egui (inverted when the binding says so); `steps` = ±1 per Line /
  Page event, a persistent 40-pt accumulator for Point events (`WHEEL_NOTCH_PT`); pinch `delta` =
  ln(zoom factor); drag `delta` in points.
- **Capture** (P6, `capture.rs` header): `capture_begin(id, slot)` during a frame; recording
  starts with the next `begin_frame`, buttons held at the start are ignored until released; a
  chord is recorded when everything is released (modifier-only and mouse chords included); mouse
  presses count only inside `capture_set_mouse_zone`; `capture_finish` returns the action's new
  alternative list (template rules: mode references, axis source, gesture kind stay), the pane
  applies it with `set_bindings`; `capture_cancel` drops it. Keys are consumed while recording; Esc
  is recorded like any key.
- **Registry ownership**: the router holds the registry in an `Arc` shared with the current
  `InputFrame`; `registry_mut` copies on write, so an edit applies from the next frame.

## Editing map
- New button family: `button.rs`, its token in `persist.rs`, its name in `label.rs` + locales.
- New matching rule: `matching.rs` (+ the header contract) and `tests/matching_tests.rs`.
- New conflict rule: `conflicts.rs` and `tests/conflict_tests.rs`.
- Changing the on-disk format: `persist.rs` (bump `HOTKEYS_FORMAT`, keep a migration), and the
  `PROTOCOL_VERSION` rule of `PROJECT_RULES.md`.
- Shared viewer actions: `view.rs`.
- Event translation, consumption, claims, axes: `router.rs` (+ `tests/runtime_tests.rs`).
- A new per-frame output or view-surface field: `frame.rs`.
- Capture recording or the template rules: `capture.rs`.
- Finger gestures (claim, strip, pan / pinch / long-press): `touch/` (own `MODULE_README.md`).
