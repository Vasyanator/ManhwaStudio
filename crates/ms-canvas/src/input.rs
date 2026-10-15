/*
File: crates/ms-canvas/src/input.rs

Purpose:
The canvas' side of the unified input system (`ms-input`): what `CanvasView` declares (bubble
history commands, the clipboard rows it cannot rebind, its pointer gestures and their targets),
the contexts it asks its owning tab to publish, its view identity, the binding-label snapshot of
its own shortcuts chip, and the small adapters that turn this frame's `InputFrame` into canvas
decisions (targeted gestures, menu opening, zoom factors).

Key items:
- `declarations()`: the canvas' `Declarations` (scope `Canvas`), registered once by the studio.
- `EDITABLE`, `ZOOM_ACCEPTED`: the canvas contexts (`CanvasView::input_contexts`).
- the `ActionId` / `TargetId` constants of the set, with the target priorities.
- `CanvasLabels`: the shortcuts-chip rows, rebuilt only when the registry revision or the UI
  locale changes.
- `CanvasView::{input_contexts, view_id}` and the per-frame helpers the scene / bubble passes
  use (`gesture_hit`, `drag_hit`, `gesture_clicked`, `bubble_menu_popup`).
- `wheel_zoom_factor`, `zoom_drag_factor`: the pure zoom laws.

Notes:
- The shared viewer set (`view.*`: zoom keys / axis / drag, pan, scroll) is `ms_input::view`'s;
  the canvas only READS it from the frame (see `lib.rs::apply_input_frame`).
- Clipboard Copy / Cut / Paste stay egui-winit platform events (`bubble_runtime.rs`
  `capture_clipboard_events`); they are declared here as fixed, read-only rows only.
- Every pointer gesture defaults to the LEFT button: right / middle drags no longer move bubbles
  or handles. Menus are the right-button click gestures.
*/

use eframe::egui;
use egui::{Id, LayerId, Order, Pos2, Response};
use ms_i18n::LocaleTag;
use ms_input::{
    ActionId, ActionMeta, ActionSpec, Binding, Button, Chord, CommandSpec, Condition, ContextId, Declarations, FixedAction, GestureKind, GestureSpec,
    HitPhase, InputRegistry, ModifierKey, MouseButton, Scope, TargetId, TargetSpec, ViewId, view,
};
use ms_widgets::touch_nav::context_menu_popup;

use super::{CanvasHintRow, CanvasView};

/// Context: the canvas edits bubbles (Translation). Gates the bubble history commands, the bubble
/// drag gestures and the page menu (the bubble menu and selection also serve read-only canvases).
pub const EDITABLE: ContextId = ContextId("canvas.editable");
/// Context: the canvas executes the shared zoom commands and the zoom axis (its zoom is not
/// blocked by its tab). The studio registers `view.zoom_in` / `zoom_out` / `zoom_reset` and
/// `view.zoom` under it, so while a tab blocks canvas zoom (advanced recognition open, a cleaning
/// editor window, a selected typing layer) those keys and Ctrl+wheel reach the tab's own readers.
pub const ZOOM_ACCEPTED: ContextId = ContextId("canvas.zoom_accepted");

/// Command: undo the last bubble edit.
pub const BUBBLE_UNDO: ActionId = ActionId("canvas.bubble.undo");
/// Command: redo the last undone bubble edit.
pub const BUBBLE_REDO: ActionId = ActionId("canvas.bubble.redo");
/// Command: duplicate the selected bubble below it.
pub const BUBBLE_DUPLICATE: ActionId = ActionId("canvas.bubble.duplicate");
/// Fixed: copy (platform clipboard event).
pub const BUBBLE_COPY: ActionId = ActionId("canvas.bubble.copy");
/// Fixed: cut (platform clipboard event).
pub const BUBBLE_CUT: ActionId = ActionId("canvas.bubble.cut");
/// Fixed: paste (platform clipboard event).
pub const BUBBLE_PASTE: ActionId = ActionId("canvas.bubble.paste");

/// Gesture: select a bubble by clicking it.
pub const BUBBLE_SELECT: ActionId = ActionId("canvas.bubble.select");
/// Gesture: open the bubble menu.
pub const BUBBLE_MENU: ActionId = ActionId("canvas.bubble.menu");
/// Gesture: drag a selected bubble's body / card (moves its anchor; an image bubble's card moves
/// the grabbed area or the whole image rect).
pub const BUBBLE_DRAG: ActionId = ActionId("canvas.bubble.drag");
/// Gesture: drag the on-top move handle (moves the whole bubble rect).
pub const BUBBLE_MOVE: ActionId = ActionId("canvas.bubble.move");
/// Gesture: drag an aside bubble's page rect.
pub const BUBBLE_MOVE_RECT: ActionId = ActionId("canvas.bubble.move_rect");
/// Gesture: drag a bubble rect resize handle.
pub const BUBBLE_RESIZE: ActionId = ActionId("canvas.bubble.resize");
/// Gesture: drag an image bubble's red image rect (its areas follow).
pub const IMAGE_RECT_MOVE: ActionId = ActionId("canvas.image_area.move_rect");
/// Gesture: drag one image area.
pub const IMAGE_AREA_MOVE: ActionId = ActionId("canvas.image_area.move");
/// Gesture: drag an image area's anchor point.
pub const IMAGE_AREA_MOVE_ANCHOR: ActionId = ActionId("canvas.image_area.move_anchor");
/// Gesture: drag an image area resize handle.
pub const IMAGE_AREA_RESIZE: ActionId = ActionId("canvas.image_area.resize");
/// Gesture: click a page (deselect, or place the bubble in move mode).
pub const PAGE_CLICK: ActionId = ActionId("canvas.page.click");
/// Gesture: open the page menu.
pub const PAGE_MENU: ActionId = ActionId("canvas.page.menu");

/// Target: a bubble rect resize handle.
pub const TARGET_HANDLE: TargetId = TargetId("canvas.bubble.handle");
/// Target: an image area resize handle.
pub const TARGET_IMAGE_AREA_HANDLE: TargetId = TargetId("canvas.image_area.handle");
/// Target: an image area anchor point.
pub const TARGET_IMAGE_AREA_ANCHOR: TargetId = TargetId("canvas.image_area.anchor");
/// Target: an image area rect.
pub const TARGET_IMAGE_AREA: TargetId = TargetId("canvas.image_area");
/// Target: an image bubble's red image rect.
pub const TARGET_IMAGE_RECT: TargetId = TargetId("canvas.image_rect");
/// Target: the on-top move handle.
pub const TARGET_MOVE_HANDLE: TargetId = TargetId("canvas.bubble.move_handle");
/// Target: an aside bubble's page rect.
pub const TARGET_RECT: TargetId = TargetId("canvas.bubble.rect");
/// Target: a bubble's own widget — the on-top body or the aside card, with their text fields.
pub const TARGET_BODY: TargetId = TargetId("canvas.bubble.body");
/// Target: a page row.
pub const TARGET_PAGE: TargetId = TargetId("canvas.page");

const SECTION_BUBBLES: &str = "canvas.input.section_bubbles";
const SECTION_PAGE: &str = "canvas.input.section_page";

/// The declared targets with their priorities: where several are hit, the higher one wins (a
/// handle over the rect it resizes, an area over the image rect holding it, ...).
const TARGETS: [(TargetId, u16); 9] = [
    (TARGET_HANDLE, 90),
    (TARGET_IMAGE_AREA_HANDLE, 85),
    (TARGET_IMAGE_AREA_ANCHOR, 84),
    (TARGET_IMAGE_AREA, 83),
    (TARGET_IMAGE_RECT, 82),
    (TARGET_MOVE_HANDLE, 80),
    (TARGET_RECT, 75),
    (TARGET_BODY, 60),
    (TARGET_PAGE, 10),
];

fn meta(id: ActionId, title_key: &'static str, section_key: &'static str, when: Vec<Condition>, defaults: Vec<Binding>) -> ActionMeta {
    ActionMeta { id, title_key, section_key, scope: Scope::Canvas, when, defaults }
}

fn command_key(key: egui::Key) -> Chord {
    Chord::new([Button::Modifier(ModifierKey::Command), Button::Key(key)])
}

fn bubble_command(id: ActionId, title_key: &'static str, defaults: Vec<Binding>) -> ActionSpec {
    ActionSpec::Command(CommandSpec { meta: meta(id, title_key, SECTION_BUBBLES, vec![Condition::Context(EDITABLE)], defaults), accepts: Vec::new(), repeat: false })
}

fn gesture(id: ActionId, title_key: &'static str, section_key: &'static str, target: TargetId, button: MouseButton, kind: GestureKind, editable_only: bool) -> ActionSpec {
    let when = if editable_only { vec![Condition::Context(EDITABLE)] } else { Vec::new() };
    ActionSpec::Gesture(GestureSpec {
        meta: meta(id, title_key, section_key, when, vec![Binding::gesture(Chord::default(), Button::Mouse(button), kind)]),
        target,
        accepts: Vec::new(),
    })
}

/// The clipboard chords egui-winit turns into `Event::Copy` / `Cut` / `Paste` on this platform
/// (`egui-winit-0.36.2/src/lib.rs:1418-1433`): Cmd+key everywhere, plus the Windows extras.
fn clipboard_defaults(key: egui::Key, windows_extra: Option<[Button; 2]>) -> Vec<Binding> {
    let mut defaults = vec![Binding::command(command_key(key))];
    if cfg!(target_os = "windows")
        && let Some(extra) = windows_extra
    {
        defaults.push(Binding::command(Chord::new(extra)));
    }
    defaults
}

/// The canvas' input declarations (scope `Canvas`): bubble undo / redo / duplicate commands, the
/// fixed clipboard rows, and the pointer gestures on the canvas targets.
#[must_use]
pub fn declarations() -> Declarations {
    use GestureKind::{Click, Drag};
    use MouseButton::{Left, Right};
    let actions = vec![
        bubble_command(BUBBLE_UNDO, "canvas.input.undo_title", vec![Binding::command(command_key(egui::Key::Z))]),
        bubble_command(
            BUBBLE_REDO,
            "canvas.input.redo_title",
            vec![
                Binding::command(Chord::new([Button::Modifier(ModifierKey::Command), Button::Modifier(ModifierKey::Shift), Button::Key(egui::Key::Z)])),
                Binding::command(command_key(egui::Key::Y)),
            ],
        ),
        bubble_command(BUBBLE_DUPLICATE, "canvas.input.duplicate_title", vec![Binding::command(command_key(egui::Key::D))]),
        // Read-only canvases keep the bubble menu (its copy items) and the card selection.
        gesture(BUBBLE_SELECT, "canvas.input.select_title", SECTION_BUBBLES, TARGET_BODY, Left, Click, false),
        gesture(BUBBLE_MENU, "canvas.input.bubble_menu_title", SECTION_BUBBLES, TARGET_BODY, Right, Click, false),
        gesture(BUBBLE_DRAG, "canvas.input.drag_title", SECTION_BUBBLES, TARGET_BODY, Left, Drag, true),
        gesture(BUBBLE_MOVE, "canvas.input.move_title", SECTION_BUBBLES, TARGET_MOVE_HANDLE, Left, Drag, true),
        gesture(BUBBLE_MOVE_RECT, "canvas.input.move_rect_title", SECTION_BUBBLES, TARGET_RECT, Left, Drag, true),
        gesture(BUBBLE_RESIZE, "canvas.input.resize_title", SECTION_BUBBLES, TARGET_HANDLE, Left, Drag, true),
        gesture(IMAGE_RECT_MOVE, "canvas.input.image_rect_move_title", SECTION_BUBBLES, TARGET_IMAGE_RECT, Left, Drag, true),
        gesture(IMAGE_AREA_MOVE, "canvas.input.image_area_move_title", SECTION_BUBBLES, TARGET_IMAGE_AREA, Left, Drag, true),
        gesture(IMAGE_AREA_MOVE_ANCHOR, "canvas.input.image_area_anchor_title", SECTION_BUBBLES, TARGET_IMAGE_AREA_ANCHOR, Left, Drag, true),
        gesture(IMAGE_AREA_RESIZE, "canvas.input.image_area_resize_title", SECTION_BUBBLES, TARGET_IMAGE_AREA_HANDLE, Left, Drag, true),
        // Page clicks also reach read-only canvases (`CanvasHooks::on_page_click`).
        gesture(PAGE_CLICK, "canvas.input.page_click_title", SECTION_PAGE, TARGET_PAGE, Left, Click, false),
        gesture(PAGE_MENU, "canvas.input.page_menu_title", SECTION_PAGE, TARGET_PAGE, Right, Click, true),
    ];
    let shift = Button::Modifier(ModifierKey::Shift);
    let command = Button::Modifier(ModifierKey::Command);
    let fixed = vec![
        FixedAction {
            meta: meta(BUBBLE_COPY, "canvas.input.copy_title", SECTION_BUBBLES, Vec::new(), clipboard_defaults(egui::Key::C, Some([command, Button::Key(egui::Key::Insert)]))),
        },
        FixedAction { meta: meta(BUBBLE_CUT, "canvas.input.cut_title", SECTION_BUBBLES, Vec::new(), clipboard_defaults(egui::Key::X, Some([shift.clone(), Button::Key(egui::Key::Delete)]))) },
        FixedAction { meta: meta(BUBBLE_PASTE, "canvas.input.paste_title", SECTION_BUBBLES, Vec::new(), clipboard_defaults(egui::Key::V, Some([shift, Button::Key(egui::Key::Insert)]))) },
    ];
    let targets = TARGETS.iter().map(|&(id, priority)| TargetSpec { id, priority, canvas_wide: false }).collect();
    Declarations { actions, targets, fixed, exclusive_contexts: Vec::new() }
}

/// The view identity of the canvas whose scroll area uses `scroll_salt` (content-named, so it is
/// stable across `StudioApp` rebuilds in one `egui::Context` and distinct per tab canvas).
#[must_use]
pub fn view_id_for(scroll_salt: &str) -> ViewId {
    ViewId(Id::new(("ms_canvas.view", scroll_salt)))
}

/// The layer of the canvas' capture `Area` for `scroll_salt` (see `CanvasView::draw_capture_area`).
#[must_use]
pub(crate) fn capture_layer_for(scroll_salt: &str) -> LayerId {
    LayerId::new(Order::Background, Id::new(("ms_canvas.capture", scroll_salt)))
}

/// Zoom factor of `steps` wheel notches of `view.zoom` (×1.1 per notch, the canvas' wheel law).
#[must_use]
pub fn wheel_zoom_factor(steps: i32) -> f32 {
    1.1_f32.powi(steps)
}

/// Zoom factor of a horizontal zoom drag of `dx` points: `exp(dx * 0.005)`, clamped to 0.5..=2 per
/// frame (the canvas' drag law).
#[must_use]
pub fn zoom_drag_factor(dx: f32) -> f32 {
    (dx * 0.005).exp().clamp(0.5, 2.0)
}

/// The binding-label snapshot of the canvas' own shortcuts chip: one row per shared viewer action
/// (title + binding label, alternatives joined by " / "). Rebuilt by [`CanvasLabels::refresh`] only
/// when the registry revision or the UI locale changed.
#[derive(Debug, Default)]
pub struct CanvasLabels {
    built_for: Option<(u64, LocaleTag)>,
    chip_rows: Vec<CanvasHintRow>,
}

/// The viewer actions the shortcuts chip lists, in display order.
const CHIP_ACTIONS: [ActionId; 8] = [view::ZOOM, view::ZOOM_IN, view::ZOOM_OUT, view::ZOOM_RESET, view::ZOOM_DRAG, view::PAN, view::SCROLL, view::SCROLL_HORIZONTAL];

impl CanvasLabels {
    /// Rebuilds the rows from `registry` when its revision or the active UI locale changed since
    /// the last build. Returns whether it rebuilt.
    pub fn refresh(&mut self, registry: &InputRegistry) -> bool {
        let key = (registry.revision(), ms_i18n::active_locale());
        if self.built_for.as_ref() == Some(&key) {
            return false;
        }
        self.chip_rows = CHIP_ACTIONS.iter().map(|&id| CanvasHintRow::new(registry.title(id), registry.label(id))).collect();
        self.built_for = Some(key);
        true
    }

    /// The shortcuts-chip rows of the last build (empty before the first `refresh`).
    #[must_use]
    pub fn chip_rows(&self) -> &[CanvasHintRow] {
        &self.chip_rows
    }
}

/// The phases of one targeted drag gesture this frame. `active` is also true on the start frame
/// (egui reports `drag_started` and `dragged` together).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DragHit {
    pub(crate) started: bool,
    pub(crate) active: bool,
    pub(crate) stopped: bool,
}

/// Whether a press of the secondary button belongs to `action` (a menu binding) or not, for the
/// menu-opening rule of [`CanvasView::bubble_menu_popup`].
fn foreign_secondary_click(response: &Response, owned: bool) -> bool {
    response.secondary_clicked() && !response.long_touched() && !owned
}

impl CanvasView {
    /// The contexts the owning tab publishes for this canvas (`StudioTab::input_contexts`):
    /// [`EDITABLE`] while it edits bubbles, [`ZOOM_ACCEPTED`] while its zoom is not blocked. Read
    /// before the frame's `begin_frame`, so a block set during a draw applies from the next frame.
    #[must_use]
    pub fn input_contexts(&self) -> Vec<ContextId> {
        let mut contexts = Vec::with_capacity(2);
        if self.editable {
            contexts.push(EDITABLE);
        }
        if !self.scene.zoom_blocked {
            contexts.push(ZOOM_ACCEPTED);
        }
        contexts
    }

    /// The canvas' view identity (published surface, axis / command focus, claims).
    #[must_use]
    pub fn view_id(&self) -> ViewId {
        view_id_for(self.scroll_area_id_salt)
    }

    /// The targeted gesture `response` reports this frame among `candidates` (target + whether the
    /// pointer hits it), resolved with the chord held at the press. `None` outside a draw.
    pub(crate) fn gesture_hit(&self, response: &Response, candidates: &[(TargetId, bool)]) -> Option<ms_input::GestureHit> {
        self.input_frame.as_ref().and_then(|frame| frame.gesture_for(response, candidates))
    }

    /// The phases of drag gesture `action` on `response` when it wins among `candidates`.
    pub(crate) fn drag_hit_among(&self, response: &Response, candidates: &[(TargetId, bool)], action: ActionId) -> DragHit {
        match self.gesture_hit(response, candidates) {
            Some(hit) if hit.action == action => DragHit {
                started: hit.phase == HitPhase::DragStarted,
                active: matches!(hit.phase, HitPhase::DragStarted | HitPhase::Dragging),
                stopped: hit.phase == HitPhase::DragStopped,
            },
            Some(_) | None => DragHit::default(),
        }
    }

    /// The phases of drag gesture `action` on `response`, the widget of `target` alone.
    pub(crate) fn drag_hit(&self, response: &Response, target: TargetId, action: ActionId) -> DragHit {
        self.drag_hit_among(response, &[(target, true)], action)
    }

    /// Whether click gesture `action` on `target` happened on `response` this frame.
    pub(crate) fn gesture_clicked(&self, response: &Response, target: TargetId, action: ActionId) -> bool {
        self.gesture_hit(response, &[(target, true)]).is_some_and(|hit| hit.action == action && hit.phase == HitPhase::Clicked)
    }

    /// The bubble menu of `response` for this frame, or `None` when it must not be built.
    ///
    /// The menu is opened through the one canvas menu path (`touch_nav::context_menu_popup`): by
    /// [`BUBBLE_MENU`] on `target`, or by egui's long touch on a widget a finger reaches (an aside
    /// card is a touch hole). A secondary click that the binding does not own (another modifier
    /// held, the menu rebound elsewhere) skips the popup for this frame, so it neither opens nor
    /// stays open. A binding on another button opens the popup at the pointer. Returns
    /// `(opened_now, popup)`.
    pub(crate) fn bubble_menu_popup<'r>(&self, response: &'r Response, target: TargetId) -> Option<(bool, egui::Popup<'r>)> {
        let owned = self.gesture_clicked(response, target, BUBBLE_MENU);
        if foreign_secondary_click(response, owned) {
            return None;
        }
        let opened_now = owned || response.long_touched();
        Some((opened_now, context_menu_popup(response, Self::menu_anchor_for_binding(response, owned))))
    }

    /// The page-menu decision of `response` this frame: `Some(true)` when [`PAGE_MENU`] fired,
    /// `Some(false)` when nothing about the menu happened, `None` when egui reports a secondary
    /// click that the binding does not own (the menu must be skipped this frame).
    pub(crate) fn page_menu_clicked(&self, response: &Response) -> Option<bool> {
        let owned = self.gesture_clicked(response, TARGET_PAGE, PAGE_MENU);
        if foreign_secondary_click(response, owned) {
            return None;
        }
        Some(owned)
    }

    /// The pointer anchor a non-secondary menu binding passes to `context_menu_popup` (the helper
    /// opens on a secondary click by itself, anchored at the pointer).
    pub(crate) fn menu_anchor_for_binding(response: &Response, opened_by_binding: bool) -> Option<Pos2> {
        (opened_by_binding && !response.secondary_clicked()).then(|| response.interact_pointer_pos().or_else(|| response.hover_pos())).flatten()
    }
}

/// Whether `frame` holds a pinch this frame (`Event::Zoom`), read without consuming: `view.zoom`
/// samples carry no source, and a pinch is the only one with `steps == 0` worth applying (a
/// sub-notch wheel accumulates into later steps). Removal condition: `ms_input::AxisSample` gains
/// its source.
pub(crate) fn pinch_this_frame(ctx: &egui::Context) -> bool {
    ctx.input(|input| input.events.iter().any(|event| matches!(event, egui::Event::Zoom(_))))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ms_input::{ActiveSet, Binding, Button, Chord, Declarations, GestureKind, InputRegistry, MatchEvent, MatchState, ModifierKey, MouseButton, TargetId, view};

    use super::*;

    /// The studio's view set plus the canvas declarations, as the studio registers them.
    fn registry() -> InputRegistry {
        let mut declarations = Declarations::default();
        declarations.extend(view::declarations());
        declarations.extend(super::declarations());
        match InputRegistry::new(declarations) {
            Ok(registry) => registry,
            Err(err) => panic!("the canvas declarations must be valid: {err}"),
        }
    }

    fn editable_env() -> ActiveSet {
        ActiveSet { contexts: BTreeSet::from([EDITABLE, ZOOM_ACCEPTED]), ..ActiveSet::default() }
    }

    fn english() {
        if let Err(err) = ms_i18n::set_locale(&ms_i18n::LocaleTag::english()) {
            panic!("embedded English catalog must load: {err}");
        }
    }

    /// The gesture a press of `button` + `kind` resolves to among the HIT `targets`.
    fn resolve(state: &MatchState, registry: &InputRegistry, button: MouseButton, kind: GestureKind, targets: &[TargetId], env: &ActiveSet) -> Option<ActionId> {
        state.match_gesture(registry, &Button::Mouse(button), kind, targets, env).map(|matched| matched.action)
    }

    #[test]
    fn the_canvas_declarations_are_valid_and_conflict_free_with_the_view_set() {
        let registry = registry();
        let canvas_conflicts: Vec<_> = registry.conflicts().conflicts.iter().filter(|conflict| conflict.a.0.starts_with("canvas.") || conflict.b.is_some_and(|b| b.0.starts_with("canvas."))).collect();
        assert!(canvas_conflicts.is_empty(), "{canvas_conflicts:?}");
        for target in [TARGET_HANDLE, TARGET_IMAGE_AREA_HANDLE, TARGET_IMAGE_AREA_ANCHOR, TARGET_IMAGE_AREA, TARGET_IMAGE_RECT, TARGET_MOVE_HANDLE, TARGET_RECT, TARGET_BODY, TARGET_PAGE] {
            let declared = registry.target(target).unwrap_or_else(|| panic!("{target:?} must be declared"));
            assert!(!declared.canvas_wide, "{target:?}: canvas targets resolve through `gesture_for`, never claims");
        }
    }

    /// The declared priorities decide what a press on overlapping parts drives: a handle beats the
    /// rect it resizes, an anchor beats its area, an area beats the image rect holding it.
    #[test]
    fn gesture_candidates_resolve_by_target_priority() {
        let registry = registry();
        let state = MatchState::default();
        let env = editable_env();
        let drag = |targets: &[TargetId]| resolve(&state, &registry, MouseButton::Left, GestureKind::Drag, targets, &env);
        assert_eq!(drag(&[TARGET_RECT, TARGET_HANDLE]), Some(BUBBLE_RESIZE));
        assert_eq!(drag(&[TARGET_RECT]), Some(BUBBLE_MOVE_RECT));
        assert_eq!(drag(&[TARGET_IMAGE_RECT, TARGET_IMAGE_AREA, TARGET_IMAGE_AREA_ANCHOR]), Some(IMAGE_AREA_MOVE_ANCHOR));
        assert_eq!(drag(&[TARGET_IMAGE_RECT, TARGET_IMAGE_AREA]), Some(IMAGE_AREA_MOVE));
        assert_eq!(drag(&[TARGET_IMAGE_AREA, TARGET_IMAGE_AREA_HANDLE]), Some(IMAGE_AREA_RESIZE));
        assert_eq!(drag(&[TARGET_IMAGE_RECT]), Some(IMAGE_RECT_MOVE));
        assert_eq!(drag(&[TARGET_BODY]), Some(BUBBLE_DRAG));
        assert_eq!(drag(&[TARGET_MOVE_HANDLE]), Some(BUBBLE_MOVE));
        assert_eq!(drag(&[]), None, "nothing hit, nothing resolves");
    }

    /// Only the left button drags (S-047 (8)); the right button clicks open the menus; a page
    /// click, the bubble menu and the bubble selection reach read-only canvases too, the drags and
    /// the page menu need `EDITABLE`.
    #[test]
    fn buttons_and_contexts_gate_the_canvas_gestures() {
        let registry = registry();
        let state = MatchState::default();
        let env = editable_env();
        for target in [TARGET_BODY, TARGET_HANDLE, TARGET_MOVE_HANDLE, TARGET_IMAGE_RECT] {
            for button in [MouseButton::Right, MouseButton::Middle] {
                assert_eq!(resolve(&state, &registry, button, GestureKind::Drag, &[target], &env), None, "{button:?} drag on {target:?}");
            }
        }
        assert_eq!(resolve(&state, &registry, MouseButton::Right, GestureKind::Click, &[TARGET_BODY], &env), Some(BUBBLE_MENU));
        assert_eq!(resolve(&state, &registry, MouseButton::Left, GestureKind::Click, &[TARGET_BODY], &env), Some(BUBBLE_SELECT));
        assert_eq!(resolve(&state, &registry, MouseButton::Right, GestureKind::Click, &[TARGET_PAGE], &env), Some(PAGE_MENU));
        let read_only = ActiveSet::default();
        assert_eq!(resolve(&state, &registry, MouseButton::Left, GestureKind::Click, &[TARGET_PAGE], &read_only), Some(PAGE_CLICK));
        assert_eq!(resolve(&state, &registry, MouseButton::Left, GestureKind::Drag, &[TARGET_BODY], &read_only), None);
        assert_eq!(resolve(&state, &registry, MouseButton::Right, GestureKind::Click, &[TARGET_PAGE], &read_only), None);
        assert_eq!(resolve(&state, &registry, MouseButton::Right, GestureKind::Click, &[TARGET_BODY], &read_only), Some(BUBBLE_MENU));
        assert_eq!(resolve(&state, &registry, MouseButton::Left, GestureKind::Click, &[TARGET_BODY], &read_only), Some(BUBBLE_SELECT));
    }

    /// Gestures match exact modifier sets: Shift+drag on a bubble is not a bubble drag, and the
    /// zoom modifier's drag is the view's canvas-wide zoom drag, never a bubble target's.
    #[test]
    fn a_held_modifier_takes_the_press_away_from_bubble_targets() {
        let registry = registry();
        let env = editable_env();
        let mut state = MatchState::default();
        let _shift = state.step(&registry, &MatchEvent::Press { button: Button::Modifier(ModifierKey::Shift), repeat: false }, &env);
        assert_eq!(resolve(&state, &registry, MouseButton::Left, GestureKind::Drag, &[TARGET_BODY], &env), None);
        let mut state = MatchState::default();
        let _ctrl = state.step(&registry, &MatchEvent::Press { button: Button::Modifier(ModifierKey::Command), repeat: false }, &env);
        assert_eq!(resolve(&state, &registry, MouseButton::Left, GestureKind::Drag, &[TARGET_BODY], &env), None);
        let on_view = ActiveSet { pointer_on_view: true, ..env };
        assert_eq!(resolve(&state, &registry, MouseButton::Left, GestureKind::Drag, &[view::SURFACE], &on_view), Some(view::ZOOM_DRAG));
    }

    /// The history commands fire only while the tab publishes `EDITABLE`, and Ctrl+Shift+Z / Ctrl+Y
    /// redo while Ctrl+Z undoes.
    #[test]
    fn history_commands_need_the_editable_context() {
        let registry = registry();
        let press = |state: &mut MatchState, button: Button, env: &ActiveSet| -> Vec<ActionId> {
            state
                .step(&registry, &MatchEvent::Press { button, repeat: false }, env)
                .into_iter()
                .filter_map(|outcome| match outcome {
                    ms_input::MatchOutcome::Fired(fired) => Some(fired.action),
                    ms_input::MatchOutcome::SequencePending => None,
                })
                .collect()
        };
        let env = editable_env();
        let mut state = MatchState::default();
        assert!(press(&mut state, Button::Modifier(ModifierKey::Command), &env).is_empty());
        assert_eq!(press(&mut state, Button::Key(egui::Key::Z), &env), vec![BUBBLE_UNDO]);
        let mut state = MatchState::default();
        press(&mut state, Button::Modifier(ModifierKey::Command), &env);
        press(&mut state, Button::Modifier(ModifierKey::Shift), &env);
        assert_eq!(press(&mut state, Button::Key(egui::Key::Z), &env), vec![BUBBLE_REDO]);
        let mut state = MatchState::default();
        press(&mut state, Button::Modifier(ModifierKey::Command), &env);
        assert_eq!(press(&mut state, Button::Key(egui::Key::Y), &env), vec![BUBBLE_REDO]);
        let mut state = MatchState::default();
        let read_only = ActiveSet::default();
        press(&mut state, Button::Modifier(ModifierKey::Command), &read_only);
        assert!(press(&mut state, Button::Key(egui::Key::Z), &read_only).is_empty());
    }

    /// The chip snapshot is rebuilt on a registry revision change (and only then), shows the
    /// current binding, and never carries help (it renders inside a tooltip).
    #[test]
    fn label_snapshot_rebuilds_on_revision() {
        english();
        let mut registry = registry();
        let mut labels = CanvasLabels::default();
        assert!(labels.refresh(&registry), "the first refresh builds");
        assert!(!labels.refresh(&registry), "an unchanged registry keeps the snapshot");
        assert_eq!(labels.chip_rows().len(), CHIP_ACTIONS.len());
        assert!(labels.chip_rows().iter().all(|row| row.help.is_none()), "chip rows must not carry help");
        let before = labels.chip_rows()[1].keys.clone();
        assert_eq!(before, registry.label(view::ZOOM_IN));
        let rebound = vec![Binding::command(Chord::new([Button::Key(egui::Key::F2)]))];
        if let Err(err) = registry.set_bindings(view::ZOOM_IN, rebound) {
            panic!("F2 is a valid zoom-in binding: {err}");
        }
        assert!(labels.refresh(&registry), "a binding change rebuilds");
        assert_eq!(labels.chip_rows()[1].keys, registry.label(view::ZOOM_IN));
        assert_ne!(labels.chip_rows()[1].keys, before);
    }

    #[test]
    fn zoom_laws() {
        assert!((wheel_zoom_factor(1) - 1.1).abs() < 1e-6);
        assert!((wheel_zoom_factor(-1) - 1.0 / 1.1).abs() < 1e-6);
        assert!((wheel_zoom_factor(2) - 1.21).abs() < 1e-5);
        assert!((wheel_zoom_factor(0) - 1.0).abs() < f32::EPSILON);
        assert!((zoom_drag_factor(0.0) - 1.0).abs() < f32::EPSILON);
        assert!((zoom_drag_factor(10_000.0) - 2.0).abs() < f32::EPSILON, "clamped at 2 per frame");
        assert!((zoom_drag_factor(-10_000.0) - 0.5).abs() < f32::EPSILON, "clamped at 0.5 per frame");
    }

    /// The canvas asks its tab for `EDITABLE` while it edits and for `ZOOM_ACCEPTED` while its zoom
    /// is not blocked; view ids differ per canvas and are content-named.
    #[test]
    fn contexts_and_view_ids_follow_the_canvas_state() {
        let mut canvas = CanvasView::default();
        assert_eq!(canvas.input_contexts(), vec![EDITABLE, ZOOM_ACCEPTED]);
        canvas.set_zoom_blocked(true);
        assert_eq!(canvas.input_contexts(), vec![EDITABLE]);
        canvas.editable = false;
        canvas.set_zoom_blocked(false);
        assert_eq!(canvas.input_contexts(), vec![ZOOM_ACCEPTED]);
        let first = canvas.view_id();
        canvas.set_scroll_area_id_salt("other_canvas_scroll");
        assert_ne!(canvas.view_id(), first);
        assert_eq!(canvas.view_id(), view_id_for("other_canvas_scroll"));
    }
}
