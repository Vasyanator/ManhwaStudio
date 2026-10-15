/*
File: crates/ms-input/src/view.rs

Purpose:
The one shared `view.*` action set every page viewer uses (Translation / Cleaning / Typing
canvases, the advanced-recognition window, the legacy region editor): zoom commands, the zoom
and pan modes, the zoom axis, the zoom drag, and egui's own scrolling as fixed read-only rows.

Key items:
- the `ActionId` / `TargetId` constants of the set;
- `declarations()`: the set as one `Declarations` block (scope `View`).

Notes:
- Zoom commands carry no `PointerOnView` condition: their focus is the view under the pointer or,
  without one, the active tab's primary view (centre anchor), so Ctrl+= works anywhere. They do
  need `ViewFocus` (some view is published): a tab without a viewer keeps its keys. The axis and
  the drag require the pointer on a view, so Ctrl+wheel over a floating panel does nothing. Each
  viewer acts only on input whose focus is ITS view, so viewers drawn over each other (the
  advanced-recognition window over its canvas) never zoom together.
*/

use crate::action::{ActionId, ActionMeta, ActionSpec, AxisSpec, CommandSpec, Declarations, FixedAction, GestureSpec, ModeEffect, ModeSpec, ReleasePolicy, Scope, TargetId, TargetSpec};
use crate::binding::{AxisSource, Binding, Chord, GestureKind};
use crate::button::{Button, ModifierKey, MouseButton};
use crate::context::Condition;

/// Mode: the modifier that turns keys / wheel / drag into zoom (Ctrl, or Z).
pub const ZOOM_MODIFIER: ActionId = ActionId("view.zoom_modifier");
/// Mode: drag pans the view (Space), effective from the drag start.
pub const PAN: ActionId = ActionId("view.pan");
/// Command: zoom in one step.
pub const ZOOM_IN: ActionId = ActionId("view.zoom_in");
/// Command: zoom out one step.
pub const ZOOM_OUT: ActionId = ActionId("view.zoom_out");
/// Command: reset the zoom.
pub const ZOOM_RESET: ActionId = ActionId("view.zoom_reset");
/// Axis: continuous zoom (wheel under the zoom modifier, trackpad pinch).
pub const ZOOM: ActionId = ActionId("view.zoom");
/// Gesture: drag to zoom (LMB drag under the zoom modifier).
pub const ZOOM_DRAG: ActionId = ActionId("view.zoom_drag");
/// Fixed: vertical scrolling (egui).
pub const SCROLL: ActionId = ActionId("view.scroll");
/// Fixed: horizontal scrolling (egui).
pub const SCROLL_HORIZONTAL: ActionId = ActionId("view.scroll_horizontal");
/// The whole surface of a view (canvas-wide, lowest priority).
pub const SURFACE: TargetId = TargetId("view.surface");

const SECTION: &str = "input.section.view";

fn meta(id: ActionId, title_key: &'static str, when: Vec<Condition>, defaults: Vec<Binding>) -> ActionMeta {
    ActionMeta { id, title_key, section_key: SECTION, scope: Scope::View, when, defaults }
}

fn key(key: egui::Key) -> Button {
    Button::Key(key)
}

/// A zoom command: active only while some view takes focus-less input (`Condition::ViewFocus`), so
/// on a tab that draws no viewer its keys are left to that tab.
fn zoom_command(id: ActionId, title_key: &'static str, defaults: Vec<Binding>) -> ActionSpec {
    ActionSpec::Command(CommandSpec { meta: meta(id, title_key, vec![Condition::ViewFocus], defaults), accepts: Vec::new(), repeat: false })
}

/// The `view.*` declarations.
#[must_use]
pub fn declarations() -> Declarations {
    let under_zoom = |buttons: Vec<Button>| Chord::new(buttons).with_modes([ZOOM_MODIFIER]);
    let actions = vec![
        ActionSpec::Mode(ModeSpec {
            meta: meta(
                ZOOM_MODIFIER,
                "input.view.zoom_modifier_title",
                Vec::new(),
                vec![Binding::mode([Button::Modifier(ModifierKey::Command)]), Binding::mode([key(egui::Key::Z)])],
            ),
            effect: ModeEffect::Immediate,
            on_release_mid_gesture: ReleasePolicy::Commit,
            beats: Vec::new(),
            param: None,
        }),
        ActionSpec::Mode(ModeSpec {
            meta: meta(PAN, "input.view.pan_title", Vec::new(), vec![Binding::mode([key(egui::Key::Space)])]),
            effect: ModeEffect::OnDragStart,
            on_release_mid_gesture: ReleasePolicy::Commit,
            beats: Vec::new(),
            param: None,
        }),
        zoom_command(
            ZOOM_IN,
            "input.view.zoom_in_title",
            vec![
                Binding::command(under_zoom(vec![key(egui::Key::Equals)])),
                Binding::command(under_zoom(vec![Button::Modifier(ModifierKey::Shift), key(egui::Key::Equals)])),
                // The keypad `+` (and layouts with a `+` key) report `Key::Plus` as the physical key.
                Binding::command(under_zoom(vec![key(egui::Key::Plus)])),
            ],
        ),
        zoom_command(ZOOM_OUT, "input.view.zoom_out_title", vec![Binding::command(under_zoom(vec![key(egui::Key::Minus)]))]),
        zoom_command(ZOOM_RESET, "input.view.zoom_reset_title", vec![Binding::command(under_zoom(vec![key(egui::Key::Num0)]))]),
        ActionSpec::Axis(AxisSpec {
            meta: meta(
                ZOOM,
                "input.view.zoom_title",
                vec![Condition::PointerOnView],
                vec![Binding::axis(AxisSource::WheelY, under_zoom(Vec::new())), Binding::axis(AxisSource::Pinch, Chord::default())],
            ),
        }),
        ActionSpec::Gesture(GestureSpec {
            meta: meta(
                ZOOM_DRAG,
                "input.view.zoom_drag_title",
                vec![Condition::PointerOnView],
                vec![Binding::gesture(under_zoom(Vec::new()), Button::Mouse(MouseButton::Left), GestureKind::Drag)],
            ),
            target: SURFACE,
            accepts: Vec::new(),
        }),
    ];
    let fixed = vec![
        FixedAction { meta: meta(SCROLL, "input.view.scroll_title", vec![Condition::PointerOnView], vec![Binding::axis(AxisSource::WheelY, Chord::default())]) },
        FixedAction {
            meta: meta(
                SCROLL_HORIZONTAL,
                "input.view.scroll_horizontal_title",
                vec![Condition::PointerOnView],
                vec![Binding::axis(AxisSource::WheelY, Chord::new([Button::Modifier(ModifierKey::Shift)])), Binding::axis(AxisSource::WheelX, Chord::default())],
            ),
        },
    ];
    Declarations { actions, targets: vec![TargetSpec { id: SURFACE, priority: 0, canvas_wide: true }], fixed, exclusive_contexts: Vec::new() }
}
