//! The egui runtime contract (`router.rs` header, `frame.rs`, `capture.rs`): headless
//! `Context::run_ui` frames over synthetic `RawInput`, as in `touch/tests.rs`.

use std::collections::BTreeSet;

use egui::{Area, Context, Event, Id, LayerId, Modifiers, MouseWheelUnit, Order, PointerButton, Pos2, RawInput, Rect, Sense, TouchPhase, Ui, Vec2, pos2, vec2};

use super::{TAB, gesture, key, lmb, meta, registry_with_view, shift, target};
use crate::{
    ActionId, ActionSpec, Binding, Button, CaptureError, CaptureSlot, Chord, FrameEnv, GestureHit, GestureKind, GesturePhase, HitPhase, InputFrame,
    InputRouter, ModeEffect, ModeSpec, ModifierKey, MouseButton, ReleasePolicy, Scope, TargetId, TouchSource, ViewId, ViewSurface, captured_alternatives,
    publish_view_surface, view,
};

const SCREEN: Rect = Rect { min: Pos2 { x: 0.0, y: 0.0 }, max: Pos2 { x: 800.0, y: 600.0 } };
/// The view surface of every test (a hole in its bottom-right corner).
const VIEW_RECT: Rect = Rect { min: Pos2 { x: 0.0, y: 0.0 }, max: Pos2 { x: 500.0, y: 500.0 } };
const HOLE: Rect = Rect { min: Pos2 { x: 400.0, y: 400.0 }, max: Pos2 { x: 500.0, y: 500.0 } };
/// Where the occluding area of the occlusion tests sits (inside the view).
const FLOATING_AT: Pos2 = Pos2 { x: 100.0, y: 300.0 };
const ON_VIEW: Pos2 = Pos2 { x: 200.0, y: 100.0 };

const P: ActionId = ActionId("test.p");
const RECT_MODE: ActionId = ActionId("test.rect_mode");
const RECT: ActionId = ActionId("test.rect");
const TOUCH_RECT: ActionId = ActionId("test.touch_rect");
const MOVE: ActionId = ActionId("test.move");
const PLAIN_MOVE: ActionId = ActionId("test.plain_move");
const HANDLE: TargetId = TargetId("test.handle");
const H_SCROLL: ActionId = ActionId("test.h_scroll");
const COMMIT_MODE: ActionId = ActionId("test.commit_mode");
const CANCEL_MODE: ActionId = ActionId("test.cancel_mode");
const POLICY_RECT: ActionId = ActionId("test.policy_rect");
/// A canvas-wide RMB click (a press action).
const PRESS: ActionId = ActionId("test.press");
/// A canvas-wide MMB drag active only under the `GATE` context.
const GATED: ActionId = ActionId("test.gated");
const GATE: crate::ContextId = crate::ContextId("test.gate");
/// The second view of the two-view tests: overlaps the first one in x 300..500, y 0..300.
const SECOND_RECT: Rect = Rect { min: Pos2 { x: 300.0, y: 0.0 }, max: Pos2 { x: 700.0, y: 300.0 } };

fn view_id() -> ViewId {
    ViewId(Id::new("test.view"))
}

fn second_id() -> ViewId {
    ViewId(Id::new("test.view.second"))
}

fn touch_owner() -> Id {
    Id::new("test.view.touch")
}

fn surface(capture_layer: Option<LayerId>) -> ViewSurface {
    ViewSurface { id: view_id(), rect: VIEW_RECT, holes: vec![HOLE], capture_layer, primary: true, touch_owner: Some(touch_owner()) }
}

fn ctrl_mods() -> Modifiers {
    Modifiers { ctrl: true, command: true, ..Modifiers::NONE }
}

fn key_event(key: egui::Key, pressed: bool, repeat: bool, modifiers: Modifiers) -> Event {
    Event::Key { key, physical_key: Some(key), pressed, repeat, modifiers }
}

fn button_event(pos: Pos2, button: PointerButton, pressed: bool, modifiers: Modifiers) -> Event {
    Event::PointerButton { pos, button, pressed, modifiers }
}

fn wheel(unit: MouseWheelUnit, delta_y: f32, modifiers: Modifiers) -> Event {
    diagonal(unit, vec2(0.0, delta_y), modifiers)
}

fn diagonal(unit: MouseWheelUnit, delta: Vec2, modifiers: Modifiers) -> Event {
    Event::MouseWheel { unit, delta, phase: TouchPhase::Move, modifiers }
}

/// The wheel deltas left in the frame's events.
fn wheel_deltas(events: &[Event]) -> Vec<Vec2> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::MouseWheel { delta, .. } => Some(*delta),
            _ => None,
        })
        .collect()
}

/// The test declarations on top of the view set: a tab command `P`, a cancel-on-release mode
/// [Shift] with a canvas-wide drag under it, a touch-only canvas-wide drag, and two targeted
/// drags on one handle ([Shift]+LMB and plain LMB).
fn declarations() -> Vec<ActionSpec> {
    vec![
        ActionSpec::Command(crate::CommandSpec { meta: meta(P.0, Scope::Tab(TAB), Vec::new(), vec![Binding::command(Chord::new([key(egui::Key::P)]))]), accepts: Vec::new(), repeat: false }),
        ActionSpec::Mode(ModeSpec {
            meta: meta(RECT_MODE.0, Scope::Tab(TAB), Vec::new(), vec![Binding::mode([shift()])]),
            effect: ModeEffect::Immediate,
            on_release_mid_gesture: ReleasePolicy::Cancel,
            beats: Vec::new(),
            param: None,
        }),
        gesture(RECT.0, Scope::Tab(TAB), view::SURFACE.0, vec![Binding::gesture(Chord::default().with_modes([RECT_MODE]), lmb(), GestureKind::Drag)], Vec::new()),
        gesture(TOUCH_RECT.0, Scope::Tab(TAB), view::SURFACE.0, vec![Binding::gesture(Chord::default(), Button::Touch(TouchSource::LongPressDrag), GestureKind::Drag)], Vec::new()),
        gesture(MOVE.0, Scope::Tab(TAB), HANDLE.0, vec![Binding::gesture(Chord::new([shift()]), lmb(), GestureKind::Drag)], Vec::new()),
        gesture(PLAIN_MOVE.0, Scope::Tab(TAB), HANDLE.0, vec![Binding::gesture(Chord::default(), lmb(), GestureKind::Drag)], Vec::new()),
        crate::tests::axis(H_SCROLL.0, Scope::Tab(TAB), vec![Binding::axis(crate::AxisSource::WheelX, Chord::new([key(egui::Key::H)]))]),
        policy_mode(COMMIT_MODE, egui::Key::C, ReleasePolicy::Commit),
        policy_mode(CANCEL_MODE, egui::Key::V, ReleasePolicy::Cancel),
        // Two alternatives on one button, told apart by the ordinary key K.
        gesture(
            POLICY_RECT.0,
            Scope::Tab(TAB),
            view::SURFACE.0,
            vec![
                Binding::gesture(Chord::default().with_modes([COMMIT_MODE]), lmb(), GestureKind::Drag),
                Binding::gesture(Chord::new([key(egui::Key::K)]).with_modes([CANCEL_MODE]), lmb(), GestureKind::Drag),
            ],
            Vec::new(),
        ),
        gesture(PRESS.0, Scope::Tab(TAB), view::SURFACE.0, vec![Binding::gesture(Chord::default(), Button::Mouse(MouseButton::Right), GestureKind::Click)], Vec::new()),
        ActionSpec::Gesture(crate::GestureSpec {
            meta: meta(GATED.0, Scope::Tab(TAB), vec![crate::Condition::Context(GATE)], vec![Binding::gesture(Chord::default(), Button::Mouse(MouseButton::Middle), GestureKind::Drag)]),
            target: view::SURFACE,
            accepts: Vec::new(),
        }),
    ]
}

fn policy_mode(id: ActionId, key_button: egui::Key, policy: ReleasePolicy) -> ActionSpec {
    ActionSpec::Mode(ModeSpec {
        meta: meta(id.0, Scope::Tab(TAB), Vec::new(), vec![Binding::mode([key(key_button)])]),
        effect: ModeEffect::Immediate,
        on_release_mid_gesture: policy,
        beats: Vec::new(),
        param: None,
    })
}

fn env() -> FrameEnv {
    FrameEnv { active_tab: Some(TAB), contexts: Vec::new(), modal_open: false }
}

/// What a frame left for the widgets drawn after the router.
#[derive(Debug, Clone)]
struct Out {
    frame: InputFrame,
    events: Vec<Event>,
    smooth_scroll: Vec2,
    hit: Option<GestureHit>,
}

/// What the test draws in a frame, besides the router.
#[derive(Debug, Clone, Copy, Default)]
struct Draw {
    /// Publish the view surface.
    publish: bool,
    /// The capture layer of the published surface.
    capture_layer: Option<LayerId>,
    /// Draw a Foreground area at `FLOATING_AT`.
    floating: bool,
    /// The extra widget drawn after the router.
    widget: Widget,
    /// The second view surface, published AFTER the first one.
    second: Second,
    /// Where the floating area sits (default `FLOATING_AT`).
    floating_at: Option<Pos2>,
}

/// The second view of the two-view tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Second {
    #[default]
    None,
    /// Published without a capture layer.
    Plain,
    /// Published with the floating area's layer as its capture layer.
    CapturesFloating,
}

/// An extra widget of a test frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Widget {
    #[default]
    None,
    /// A focused single-line text edit.
    TextEdit,
    /// A drag-sensing handle around `ON_VIEW` whose gesture is resolved with `gesture_for`.
    Handle,
}

struct Bench {
    ctx: Context,
    router: InputRouter,
    time: f64,
    text: String,
}

impl Bench {
    fn new() -> Self {
        Self { ctx: Context::default(), router: InputRouter::new(registry_with_view(declarations(), vec![target(HANDLE.0, 50)])), time: 0.0, text: String::new() }
    }

    fn frame(&mut self, events: Vec<Event>, env: &FrameEnv, draw: Draw) -> Out {
        self.frame_with(events, env, draw, |_| {})
    }

    /// Runs one frame; `during` runs inside the pass right after `begin_frame` (capture calls).
    fn frame_with(&mut self, events: Vec<Event>, env: &FrameEnv, draw: Draw, mut during: impl FnMut(&mut InputRouter)) -> Out {
        self.time += 0.016;
        let raw = RawInput { screen_rect: Some(SCREEN), time: Some(self.time), events, ..RawInput::default() };
        let (router, text) = (&mut self.router, &mut self.text);
        let mut out: Option<Out> = None;
        self.ctx
            .run_ui(raw, |ui: &mut Ui| {
                let frame = router.begin_frame(ui.ctx(), env).clone();
                during(router);
                let (events, smooth_scroll) = ui.input(|input| (input.events.clone(), input.smooth_scroll_delta));
                let mut hit = None;
                if draw.widget == Widget::Handle {
                    let rect = Rect::from_center_size(ON_VIEW, vec2(40.0, 40.0));
                    let response = ui.interact(rect, Id::new("test.handle"), Sense::drag());
                    hit = frame.gesture_for(&response, &[(HANDLE, true), (TargetId("test.unhit"), false)]);
                }
                if draw.widget == Widget::TextEdit {
                    let response = ui.add(egui::TextEdit::singleline(text));
                    response.request_focus();
                }
                if draw.publish {
                    publish_view_surface(ui.ctx(), surface(draw.capture_layer));
                }
                let floating_layer = LayerId::new(Order::Foreground, Id::new("test.floating"));
                let second = |capture_layer| ViewSurface { id: second_id(), rect: SECOND_RECT, holes: Vec::new(), capture_layer, primary: false, touch_owner: None };
                match draw.second {
                    Second::None => {}
                    Second::Plain => publish_view_surface(ui.ctx(), second(None)),
                    Second::CapturesFloating => publish_view_surface(ui.ctx(), second(Some(floating_layer))),
                }
                if draw.floating {
                    Area::new(Id::new("test.floating")).order(Order::Foreground).fixed_pos(draw.floating_at.unwrap_or(FLOATING_AT)).show(ui.ctx(), |ui| {
                        ui.allocate_exact_size(vec2(80.0, 80.0), Sense::click());
                    });
                }
                out = Some(Out { frame, events, smooth_scroll, hit });
            })
            .drop_without_applying_deltas();
        match out {
            Some(out) => out,
            None => panic!("run_ui must call the frame closure"),
        }
    }
}

fn published() -> Draw {
    Draw { publish: true, ..Draw::default() }
}

fn key_events(events: &[Event]) -> usize {
    events.iter().filter(|event| matches!(event, Event::Key { .. })).count()
}

#[test]
fn a_matched_key_fires_once_and_is_consumed_with_its_repeats_and_release() {
    let mut bench = Bench::new();
    let out = bench.frame(vec![key_event(egui::Key::P, true, false, Modifiers::NONE), key_event(egui::Key::Q, true, false, Modifiers::NONE)], &env(), Draw::default());
    assert_eq!(out.frame.fired_commands().iter().map(|fired| fired.action).collect::<Vec<_>>(), vec![P]);
    assert_eq!(key_events(&out.events), 1, "only the unmatched Q press reaches egui: {:?}", out.events);
    let out = bench.frame(vec![key_event(egui::Key::P, true, true, Modifiers::NONE)], &env(), Draw::default());
    assert!(out.frame.fired(P).is_none(), "a non-repeat command does not re-fire on auto-repeat");
    assert_eq!(key_events(&out.events), 0, "the repeat of a consumed press is consumed too");
    let out = bench.frame(vec![key_event(egui::Key::P, false, false, Modifiers::NONE)], &env(), Draw::default());
    assert_eq!(key_events(&out.events), 0, "the release of a consumed press is consumed");
    let out = bench.frame(vec![key_event(egui::Key::Q, false, false, Modifiers::NONE)], &env(), Draw::default());
    assert_eq!(key_events(&out.events), 1, "an unmatched release passes");
}

#[test]
fn modifier_keys_are_roles_not_keys_and_exact() {
    let mut bench = Bench::new();
    let shift_mods = Modifiers::SHIFT;
    let events = vec![Event::ModifiersChanged(shift_mods), key_event(egui::Key::ShiftLeft, true, false, shift_mods), key_event(egui::Key::P, true, false, shift_mods)];
    let out = bench.frame(events, &env(), published());
    assert!(out.frame.fired(P).is_none(), "Shift+P must not fire P");
    assert!(out.frame.mode_active(RECT_MODE), "the Shift KEY event fed the Shift role");
    assert_eq!(key_events(&out.events), 2, "nothing matched, nothing consumed");
}

#[test]
fn key_chords_do_not_match_while_a_text_edit_has_focus() {
    let mut bench = Bench::new();
    let typing = Draw { widget: Widget::TextEdit, ..Draw::default() };
    bench.frame(Vec::new(), &env(), typing);
    bench.frame(Vec::new(), &env(), typing);
    let out = bench.frame(vec![key_event(egui::Key::P, true, false, Modifiers::NONE)], &env(), typing);
    assert!(out.frame.fired(P).is_none());
    assert_eq!(key_events(&out.events), 1, "the text edit gets the key");
}

#[test]
fn a_modal_prompt_suppresses_firing_but_tracks_held_buttons() {
    let mut bench = Bench::new();
    let modal = FrameEnv { modal_open: true, ..env() };
    let out = bench.frame(vec![Event::ModifiersChanged(Modifiers::SHIFT)], &modal, published());
    assert!(out.frame.suppressed());
    assert!(!out.frame.mode_active(RECT_MODE), "no mode is active under a modal prompt");
    let out = bench.frame(vec![key_event(egui::Key::P, true, false, Modifiers::SHIFT)], &modal, published());
    assert!(out.frame.fired(P).is_none());
    assert_eq!(key_events(&out.events), 1, "nothing is consumed under a modal prompt");
    let out = bench.frame(Vec::new(), &env(), published());
    assert!(out.frame.mode_active(RECT_MODE), "Shift pressed under the prompt is still known to be held");
}

#[test]
fn the_wheel_is_consumed_and_scroll_zeroed_only_when_an_axis_matched() {
    let mut bench = Bench::new();
    bench.frame(vec![Event::PointerMoved(ON_VIEW)], &env(), published());
    let out = bench.frame(vec![wheel(MouseWheelUnit::Line, 1.0, Modifiers::NONE)], &env(), published());
    assert!(out.frame.axis(view::ZOOM).is_none(), "plain wheel scrolls");
    assert_eq!(out.events.iter().filter(|event| matches!(event, Event::MouseWheel { .. })).count(), 1);
    assert_ne!(out.smooth_scroll, Vec2::ZERO, "egui keeps scrolling with an unmatched wheel");

    // Z is the second zoom modifier: egui would scroll, the router zooms instead.
    bench.frame(vec![key_event(egui::Key::Z, true, false, Modifiers::NONE)], &env(), published());
    let out = bench.frame(vec![wheel(MouseWheelUnit::Line, 1.0, Modifiers::NONE)], &env(), published());
    let sample = out.frame.axis_for_view(view::ZOOM, view_id()).copied();
    assert_eq!(sample.map(|sample| sample.steps), Some(1));
    assert_eq!(sample.and_then(|sample| sample.focus).map(|focus| focus.pos), Some(ON_VIEW), "focus = pointer over the view");
    assert!(!out.events.iter().any(|event| matches!(event, Event::MouseWheel { .. })), "a matched wheel event is consumed");
    assert_eq!(out.smooth_scroll, Vec2::ZERO, "and egui's smoothed scroll is zeroed");
    let out = bench.frame(Vec::new(), &env(), published());
    assert_eq!(out.smooth_scroll, Vec2::ZERO, "the smoothing remainder of a matched wheel never scrolls");
}

#[test]
fn point_wheel_steps_accumulate_whole_notches_across_frames() {
    let mut bench = Bench::new();
    bench.frame(vec![Event::PointerMoved(ON_VIEW), key_event(egui::Key::Z, true, false, Modifiers::NONE)], &env(), published());
    let out = bench.frame(vec![wheel(MouseWheelUnit::Point, 25.0, Modifiers::NONE)], &env(), published());
    assert_eq!(out.frame.axis(view::ZOOM).map(|sample| sample.steps), Some(0));
    let out = bench.frame(vec![wheel(MouseWheelUnit::Point, 25.0, Modifiers::NONE)], &env(), published());
    assert_eq!(out.frame.axis(view::ZOOM).map(|sample| sample.steps), Some(1), "50 pt = one 40-pt notch");
    assert!(out.frame.axis(view::ZOOM).is_some_and(|sample| (sample.delta - 25.0 / 40.0).abs() < 1e-5));
}

#[test]
fn a_wheel_over_no_view_is_left_to_egui() {
    let mut bench = Bench::new();
    bench.frame(vec![Event::PointerMoved(pos2(700.0, 550.0)), key_event(egui::Key::Z, true, false, Modifiers::NONE)], &env(), published());
    let out = bench.frame(vec![wheel(MouseWheelUnit::Line, 1.0, Modifiers::NONE)], &env(), published());
    assert!(out.frame.axis(view::ZOOM).is_none(), "view.zoom requires the pointer on a view");
}

#[test]
fn a_claim_needs_the_previous_frames_surface() {
    let mut bench = Bench::new();
    // The surface is first published in the press frame itself: too late.
    let press = vec![Event::ModifiersChanged(ctrl_mods()), Event::PointerMoved(ON_VIEW), button_event(ON_VIEW, PointerButton::Primary, true, ctrl_mods())];
    let out = bench.frame(press.clone(), &env(), published());
    assert!(out.frame.claimed(view_id()).is_none());
    bench.frame(vec![button_event(ON_VIEW, PointerButton::Primary, false, ctrl_mods())], &env(), published());

    let out = bench.frame(Vec::new(), &env(), published());
    assert!(out.frame.canvas_wide_armed(view_id()), "Ctrl held over the view arms the zoom drag");
    let out = bench.frame(vec![button_event(ON_VIEW, PointerButton::Primary, true, ctrl_mods())], &env(), published());
    let claim = out.frame.claimed(view_id()).cloned();
    assert_eq!(claim.as_ref().map(|claim| (claim.action, claim.phase, claim.button.clone())), Some((view::ZOOM_DRAG, GesturePhase::Started, lmb())));
    let moved = ON_VIEW + vec2(30.0, 0.0);
    let out = bench.frame(vec![Event::PointerMoved(moved)], &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| (claim.phase, claim.origin, claim.current)), Some((GesturePhase::Active, ON_VIEW, moved)));
    let out = bench.frame(vec![button_event(moved, PointerButton::Primary, false, ctrl_mods())], &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| claim.phase), Some(GesturePhase::Finished(ReleasePolicy::Commit)));
    let out = bench.frame(Vec::new(), &env(), published());
    assert!(out.frame.claimed(view_id()).is_none(), "a finished claim lives one frame");
}

#[test]
fn a_press_in_a_hole_or_without_a_chord_is_not_claimed() {
    let mut bench = Bench::new();
    bench.frame(vec![Event::ModifiersChanged(ctrl_mods())], &env(), published());
    let out = bench.frame(vec![Event::PointerMoved(HOLE.center()), button_event(HOLE.center(), PointerButton::Primary, true, ctrl_mods())], &env(), published());
    assert!(out.frame.claimed(view_id()).is_none(), "holes are not the view");
    assert!(out.frame.view_under_pointer().is_none());
    bench.frame(vec![button_event(HOLE.center(), PointerButton::Primary, false, ctrl_mods()), Event::ModifiersChanged(Modifiers::NONE)], &env(), published());
    let out = bench.frame(vec![Event::PointerMoved(ON_VIEW), button_event(ON_VIEW, PointerButton::Primary, true, Modifiers::NONE)], &env(), published());
    assert!(out.frame.claimed(view_id()).is_none(), "plain LMB has no canvas-wide gesture here");
}

#[test]
fn a_foreground_area_occludes_the_view_unless_it_is_the_capture_layer() {
    let mut bench = Bench::new();
    let over_area = FLOATING_AT + vec2(20.0, 20.0);
    let occluded = Draw { publish: true, floating: true, ..Draw::default() };
    bench.frame(Vec::new(), &env(), occluded);
    bench.frame(vec![Event::PointerMoved(over_area)], &env(), occluded);
    let out = bench.frame(vec![Event::ModifiersChanged(ctrl_mods())], &env(), occluded);
    assert!(out.frame.view_under_pointer().is_none(), "a floating area over the view occludes it");
    assert!(!out.frame.canvas_wide_armed(view_id()));
    let out = bench.frame(vec![button_event(over_area, PointerButton::Primary, true, ctrl_mods())], &env(), occluded);
    assert!(out.frame.claimed(view_id()).is_none(), "no claim through the area");
    bench.frame(vec![button_event(over_area, PointerButton::Primary, false, ctrl_mods())], &env(), occluded);

    let own = Draw { publish: true, capture_layer: Some(LayerId::new(Order::Foreground, Id::new("test.floating"))), floating: true, ..Draw::default() };
    bench.frame(Vec::new(), &env(), own);
    let out = bench.frame(Vec::new(), &env(), own);
    assert_eq!(out.frame.view_under_pointer(), Some(view_id()), "the view's own capture layer does not occlude it");
    let out = bench.frame(vec![button_event(over_area, PointerButton::Primary, true, ctrl_mods())], &env(), own);
    assert!(out.frame.claimed(view_id()).is_some());
}

#[test]
fn releasing_the_gesture_mode_ends_the_claim_per_its_policy() {
    let mut bench = Bench::new();
    bench.frame(vec![Event::PointerMoved(ON_VIEW)], &env(), published());
    // test.rect_mode cancels on release.
    let out = bench.frame(vec![Event::ModifiersChanged(Modifiers::SHIFT), button_event(ON_VIEW, PointerButton::Primary, true, Modifiers::SHIFT)], &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| claim.action), Some(RECT));
    let out = bench.frame(vec![Event::ModifiersChanged(Modifiers::NONE)], &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| claim.phase), Some(GesturePhase::Finished(ReleasePolicy::Cancel)));
    bench.frame(vec![button_event(ON_VIEW, PointerButton::Primary, false, Modifiers::NONE)], &env(), published());

    // view.zoom_modifier commits on release.
    let out = bench.frame(vec![Event::ModifiersChanged(ctrl_mods()), button_event(ON_VIEW, PointerButton::Primary, true, ctrl_mods())], &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| claim.action), Some(view::ZOOM_DRAG));
    let out = bench.frame(vec![Event::ModifiersChanged(Modifiers::NONE)], &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| claim.phase), Some(GesturePhase::Finished(ReleasePolicy::Commit)));
}

#[test]
fn focus_loss_resets_held_state_and_cancels_claims() {
    let mut bench = Bench::new();
    bench.frame(vec![Event::PointerMoved(ON_VIEW)], &env(), published());
    let out = bench.frame(vec![Event::ModifiersChanged(Modifiers::SHIFT), button_event(ON_VIEW, PointerButton::Primary, true, Modifiers::SHIFT)], &env(), published());
    assert!(out.frame.mode_active(RECT_MODE));
    assert!(out.frame.claimed(view_id()).is_some());
    let out = bench.frame(vec![Event::WindowFocused(false)], &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| claim.phase), Some(GesturePhase::Finished(ReleasePolicy::Cancel)));
    assert!(!out.frame.mode_active(RECT_MODE), "no release arrives after a focus loss; the mode is dropped");
}

#[test]
fn gesture_for_resolves_with_the_chord_held_at_the_press() {
    let mut bench = Bench::new();
    let handle = Draw { widget: Widget::Handle, ..Draw::default() };
    bench.frame(vec![Event::PointerMoved(ON_VIEW)], &env(), handle);
    // A drag-only widget starts its drag at the press (egui needs no movement without a click
    // sense); Shift is released right after: the press-time chord still decides every frame.
    let mut hits: Vec<GestureHit> =
        bench.frame(vec![Event::ModifiersChanged(Modifiers::SHIFT), button_event(ON_VIEW, PointerButton::Primary, true, Modifiers::SHIFT)], &env(), handle).hit.into_iter().collect();
    let mut at = ON_VIEW;
    for _ in 0..4 {
        at += vec2(10.0, 0.0);
        hits.extend(bench.frame(vec![Event::ModifiersChanged(Modifiers::NONE), Event::PointerMoved(at)], &env(), handle).hit);
    }
    let first = hits.first().cloned();
    assert_eq!(first.map(|hit| (hit.action, hit.phase, hit.button)), Some((MOVE, HitPhase::DragStarted, MouseButton::Left)), "{hits:?}");
    assert!(hits.iter().skip(1).all(|hit| hit.action == MOVE && hit.phase == HitPhase::Dragging), "{hits:?}");
    let stop = bench.frame(vec![button_event(at, PointerButton::Primary, false, Modifiers::NONE)], &env(), handle).hit;
    assert_eq!(stop.map(|hit| (hit.action, hit.phase)), Some((MOVE, HitPhase::DragStopped)), "the snapshot lives through the release frame");

    // Without Shift at the press, the plain drag wins.
    bench.frame(vec![Event::PointerMoved(ON_VIEW)], &env(), handle);
    let mut hits: Vec<GestureHit> = bench.frame(vec![button_event(ON_VIEW, PointerButton::Primary, true, Modifiers::NONE)], &env(), handle).hit.into_iter().collect();
    let mut at = ON_VIEW;
    for _ in 0..4 {
        at += vec2(10.0, 0.0);
        hits.extend(bench.frame(vec![Event::PointerMoved(at)], &env(), handle).hit);
    }
    assert_eq!(hits.first().map(|hit| hit.action), Some(PLAIN_MOVE), "{hits:?}");
}

#[test]
fn a_touch_selection_drives_the_touch_alternative() {
    let mut bench = Bench::new();
    bench.frame(Vec::new(), &env(), published());
    let out = bench.frame(Vec::new(), &env(), published());
    assert!(out.frame.touch_selection_available(view_id()), "a canvas-wide long-press-drag gesture is bound");
    let origin = pos2(50.0, 60.0);
    let selection = crate::touch::TouchSelection { origin, pos: origin + vec2(5.0, 0.0), began: true, end: None };
    crate::touch::deliver_for_test(&bench.ctx, touch_owner(), crate::touch::TouchOutput { selection: Some(selection), ..crate::touch::TouchOutput::default() });
    let out = bench.frame(Vec::new(), &env(), published());
    let claim = out.frame.claimed(view_id()).cloned();
    assert_eq!(claim.map(|claim| (claim.action, claim.phase, claim.origin, claim.button)), Some((TOUCH_RECT, GesturePhase::Started, origin, Button::Touch(TouchSource::LongPressDrag))));
    let end = crate::touch::TouchSelection { origin, pos: origin + vec2(40.0, 0.0), began: false, end: Some(crate::touch::TouchSelectionEnd::Released) };
    crate::touch::deliver_for_test(&bench.ctx, touch_owner(), crate::touch::TouchOutput { selection: Some(end), ..crate::touch::TouchOutput::default() });
    let out = bench.frame(Vec::new(), &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| (claim.phase, claim.current)), Some((GesturePhase::Finished(ReleasePolicy::Commit), origin + vec2(40.0, 0.0))));
}

#[test]
fn capture_ignores_its_starting_frame_and_records_a_modifier_only_chord() {
    let mut bench = Bench::new();
    // The click on the capture button: pressed in this frame, capture started inside it.
    bench.frame_with(vec![button_event(ON_VIEW, PointerButton::Primary, true, Modifiers::NONE)], &env(), Draw::default(), |router| {
        router.capture_begin(RECT_MODE, CaptureSlot::Replace(0));
        router.capture_set_mouse_zone(Some(SCREEN));
    });
    let out = bench.frame(vec![button_event(ON_VIEW, PointerButton::Primary, false, Modifiers::NONE)], &env(), Draw::default());
    assert!(out.frame.suppressed());
    assert_eq!(bench.router.capture_status().map(|status| status.chords.len()), Some(0), "the starting click is not recorded");
    bench.frame(vec![Event::ModifiersChanged(Modifiers::SHIFT), key_event(egui::Key::ShiftLeft, true, false, Modifiers::SHIFT)], &env(), Draw::default());
    assert_eq!(bench.router.capture_status().map(|status| status.chords.len()), Some(0), "a chord is recorded only once all is released");
    bench.frame(vec![Event::ModifiersChanged(Modifiers::NONE), key_event(egui::Key::ShiftLeft, false, false, Modifiers::NONE)], &env(), Draw::default());
    assert_eq!(bench.router.capture_finish(), Ok(vec![Binding::mode([shift()])]));
    assert!(bench.router.capture_status().is_none());
}

#[test]
fn capture_records_keys_and_mouse_buttons_inside_its_zone_and_consumes_keys() {
    let mut bench = Bench::new();
    let zone = Rect::from_min_size(pos2(600.0, 0.0), vec2(200.0, 200.0));
    bench.frame_with(Vec::new(), &env(), Draw::default(), |router| {
        router.capture_begin(P, CaptureSlot::Append);
        router.capture_set_mouse_zone(Some(zone));
    });
    let outside = vec![button_event(ON_VIEW, PointerButton::Secondary, true, Modifiers::NONE), button_event(ON_VIEW, PointerButton::Secondary, false, Modifiers::NONE)];
    bench.frame(outside, &env(), Draw::default());
    assert_eq!(bench.router.capture_status().map(|status| status.chords.len()), Some(0), "a click outside the zone (Done / Cancel) is not recorded");
    let inside = zone.center();
    let events = vec![
        Event::ModifiersChanged(ctrl_mods()),
        button_event(inside, PointerButton::Extra1, true, ctrl_mods()),
        key_event(egui::Key::Delete, true, false, ctrl_mods()),
    ];
    let out = bench.frame(events, &env(), Draw::default());
    assert_eq!(key_events(&out.events), 0, "keys are consumed while recording");
    let release = vec![
        key_event(egui::Key::Delete, false, false, ctrl_mods()),
        button_event(inside, PointerButton::Extra1, false, ctrl_mods()),
        Event::ModifiersChanged(Modifiers::NONE),
    ];
    bench.frame(release, &env(), Draw::default());
    let expected: BTreeSet<Button> = [Button::Modifier(ModifierKey::Command), Button::Mouse(MouseButton::Extra(1)), key(egui::Key::Delete)].into_iter().collect();
    assert_eq!(bench.router.capture_status().map(|status| status.chords.clone()), Some(vec![expected.clone()]));
    let result = bench.router.capture_finish();
    assert_eq!(result, Ok(vec![Binding::command(Chord::new([key(egui::Key::P)])), Binding::command(Chord { modes: BTreeSet::new(), buttons: expected })]));
}

#[test]
fn capture_cancel_drops_everything_and_input_resumes() {
    let mut bench = Bench::new();
    bench.frame_with(Vec::new(), &env(), Draw::default(), |router| router.capture_begin(P, CaptureSlot::Append));
    bench.frame(vec![key_event(egui::Key::A, true, false, Modifiers::NONE), key_event(egui::Key::A, false, false, Modifiers::NONE)], &env(), Draw::default());
    bench.router.capture_cancel();
    assert!(bench.router.capture_status().is_none());
    assert_eq!(bench.router.capture_finish(), Err(CaptureError::NotCapturing));
    let out = bench.frame(vec![key_event(egui::Key::P, true, false, Modifiers::NONE)], &env(), Draw::default());
    assert!(out.frame.fired(P).is_some(), "commands fire again after the capture");
}

#[test]
fn captured_alternatives_keep_template_modes_and_check_the_kind() {
    let registry = registry_with_view(declarations(), vec![target(HANDLE.0, 50)]);
    let equals: BTreeSet<Button> = [key(egui::Key::Equals)].into_iter().collect();
    let alternatives = captured_alternatives(&registry, view::ZOOM_IN, CaptureSlot::Replace(0), std::slice::from_ref(&equals));
    let expected = Binding::command(Chord::new([key(egui::Key::Equals)]).with_modes([view::ZOOM_MODIFIER]));
    assert_eq!(alternatives.map(|alternatives| alternatives.first().cloned()), Ok(Some(expected)), "mode references are structural (P3)");
    let two = [equals.clone(), equals.clone()];
    assert_eq!(captured_alternatives(&registry, RECT_MODE, CaptureSlot::Append, &two), Err(CaptureError::SequenceNotAllowed(RECT_MODE.0)));
    assert_eq!(captured_alternatives(&registry, view::ZOOM_DRAG, CaptureSlot::Append, std::slice::from_ref(&equals)), Err(CaptureError::PointerButtonCount { action: view::ZOOM_DRAG.0, count: 0 }));
    let ctrl_rmb: BTreeSet<Button> = [Button::Modifier(ModifierKey::Command), Button::Mouse(MouseButton::Right)].into_iter().collect();
    let gesture = captured_alternatives(&registry, view::ZOOM_DRAG, CaptureSlot::Append, &[ctrl_rmb]).map(|alternatives| alternatives.last().cloned());
    let expected = Binding::gesture(Chord::new([Button::Modifier(ModifierKey::Command)]).with_modes([view::ZOOM_MODIFIER]), Button::Mouse(MouseButton::Right), GestureKind::Drag);
    assert_eq!(gesture, Ok(Some(expected)));
    assert_eq!(captured_alternatives(&registry, view::SCROLL, CaptureSlot::Append, std::slice::from_ref(&equals)), Err(CaptureError::FixedAction(view::SCROLL.0)));
    assert_eq!(captured_alternatives(&registry, P, CaptureSlot::Replace(5), &[equals]), Err(CaptureError::SlotOutOfRange { action: P.0, slot: 5 }));
    assert_eq!(captured_alternatives(&registry, P, CaptureSlot::Append, &[]), Err(CaptureError::Empty(P.0)));
}

/// One diagonal wheel frame with `keys` held (pressed in a frame before), pointer at `at`.
fn diagonal_frame(keys: &[egui::Key], at: Pos2, draw: Draw) -> Out {
    let mut bench = Bench::new();
    bench.frame(Vec::new(), &env(), draw);
    let mut events = vec![Event::PointerMoved(at)];
    events.extend(keys.iter().map(|held| key_event(*held, true, false, Modifiers::NONE)));
    bench.frame(events, &env(), draw);
    bench.frame(vec![diagonal(MouseWheelUnit::Line, vec2(1.0, 1.0), Modifiers::NONE)], &env(), draw)
}

#[test]
fn a_diagonal_wheel_is_owned_per_component() {
    // Neither component bound: egui keeps the whole event and scrolls on both axes.
    let out = diagonal_frame(&[], ON_VIEW, published());
    assert_eq!(wheel_deltas(&out.events), vec![vec2(1.0, 1.0)]);
    assert!(out.smooth_scroll.x != 0.0 && out.smooth_scroll.y != 0.0, "{:?}", out.smooth_scroll);

    // Y owned (Z = zoom modifier): the event keeps only X; only Y smoothing is suppressed.
    let out = diagonal_frame(&[egui::Key::Z], ON_VIEW, published());
    assert_eq!(out.frame.axis(view::ZOOM).map(|sample| sample.steps), Some(1));
    assert_eq!(wheel_deltas(&out.events), vec![vec2(1.0, 0.0)]);
    assert!(out.smooth_scroll.x != 0.0 && out.smooth_scroll.y == 0.0, "{:?}", out.smooth_scroll);

    // X owned (H): the event keeps only Y.
    let out = diagonal_frame(&[egui::Key::H], ON_VIEW, published());
    assert_eq!(out.frame.axis(H_SCROLL).map(|sample| sample.steps), Some(1));
    assert_eq!(wheel_deltas(&out.events), vec![vec2(0.0, 1.0)]);
    assert!(out.smooth_scroll.x == 0.0 && out.smooth_scroll.y != 0.0, "{:?}", out.smooth_scroll);

    // Both owned: the event is removed and nothing scrolls.
    let out = diagonal_frame(&[egui::Key::Z, egui::Key::H], ON_VIEW, published());
    assert!(out.frame.axis(view::ZOOM).is_some() && out.frame.axis(H_SCROLL).is_some());
    assert_eq!(wheel_deltas(&out.events), [] as [egui::Vec2; 0]);
    assert_eq!(out.smooth_scroll, Vec2::ZERO);
}

#[test]
fn a_wheel_over_an_occluding_panel_stays_with_the_panel() {
    let occluded = Draw { publish: true, floating: true, ..Draw::default() };
    let out = diagonal_frame(&[egui::Key::Z], FLOATING_AT + vec2(20.0, 20.0), occluded);
    assert!(out.frame.axis(view::ZOOM).is_none(), "view.zoom needs the pointer on the view");
    assert_eq!(wheel_deltas(&out.events), vec![vec2(1.0, 1.0)], "the panel under the pointer keeps the whole wheel");
    assert!(out.smooth_scroll.x != 0.0 && out.smooth_scroll.y != 0.0, "{:?}", out.smooth_scroll);
}

#[test]
fn a_claim_ends_by_the_policy_of_the_winning_alternatives_modes() {
    let mut bench = Bench::new();
    bench.frame(vec![Event::PointerMoved(ON_VIEW)], &env(), published());
    // C (commit mode) and V (cancel mode) held, K not: only `[commit_mode]+LMB` matches.
    bench.frame(vec![key_event(egui::Key::C, true, false, Modifiers::NONE), key_event(egui::Key::V, true, false, Modifiers::NONE)], &env(), published());
    let out = bench.frame(vec![button_event(ON_VIEW, PointerButton::Primary, true, Modifiers::NONE)], &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| claim.action), Some(POLICY_RECT));
    let out = bench.frame(vec![key_event(egui::Key::V, false, false, Modifiers::NONE)], &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| claim.phase), Some(GesturePhase::Active), "the losing alternative's mode does not end the claim");
    let out = bench.frame(vec![key_event(egui::Key::C, false, false, Modifiers::NONE)], &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| claim.phase), Some(GesturePhase::Finished(ReleasePolicy::Commit)));
}

#[test]
fn overlapping_views_resolve_by_capture_layer_then_publication_order() {
    let mut bench = Bench::new();
    let both = Draw { publish: true, second: Second::Plain, ..Draw::default() };
    let overlap = pos2(400.0, 100.0);
    bench.frame(Vec::new(), &env(), both);
    let out = bench.frame(vec![Event::PointerMoved(overlap)], &env(), both);
    assert_eq!(out.frame.view_under_pointer(), Some(second_id()), "the later-published view wins an overlap");
    let out = bench.frame(vec![Event::PointerMoved(pos2(100.0, 100.0))], &env(), both);
    assert_eq!(out.frame.view_under_pointer(), Some(view_id()), "a region only the first view covers");
    let out = bench.frame(vec![Event::PointerMoved(pos2(650.0, 100.0))], &env(), both);
    assert_eq!(out.frame.view_under_pointer(), Some(second_id()), "a region only the second view covers");

    // Primary fallback: a zoom key with the pointer on no view focuses the primary view's centre.
    let out = bench.frame(
        vec![Event::PointerMoved(pos2(750.0, 550.0)), key_event(egui::Key::Z, true, false, Modifiers::NONE), key_event(egui::Key::Equals, true, false, Modifiers::NONE)],
        &env(),
        both,
    );
    assert_eq!(out.frame.view_under_pointer(), None);
    assert_eq!(out.frame.fired(view::ZOOM_IN).and_then(|fired| fired.focus).map(|focus| (focus.view, focus.pos)), Some((view_id(), VIEW_RECT.center())));
}

#[test]
fn an_overlap_under_a_capture_layer_belongs_to_the_view_that_owns_it() {
    let mut bench = Bench::new();
    // The floating area sits over the first view only: move it into the overlap region.
    let draw = Draw { publish: true, floating: true, second: Second::CapturesFloating, ..Draw::default() };
    bench.frame(Vec::new(), &env(), draw);
    bench.frame(Vec::new(), &env(), draw);
    let over_area = FLOATING_AT + vec2(20.0, 20.0);
    let out = bench.frame(vec![Event::PointerMoved(over_area)], &env(), draw);
    assert_eq!(out.frame.view_under_pointer(), None, "outside the second view the area occludes the first one");
    let mut bench = Bench::new();
    let draw = Draw { publish: true, floating: true, second: Second::CapturesFloating, floating_at: Some(pos2(350.0, 50.0)), ..Draw::default() };
    bench.frame(Vec::new(), &env(), draw);
    bench.frame(Vec::new(), &env(), draw);
    let out = bench.frame(vec![Event::PointerMoved(pos2(370.0, 70.0))], &env(), draw);
    assert_eq!(out.frame.view_under_pointer(), Some(second_id()), "the view whose capture layer is on top wins, not the first published");
}

#[test]
fn command_held_lasts_while_the_firing_key_is_down() {
    let mut bench = Bench::new();
    let out = bench.frame(vec![key_event(egui::Key::P, true, false, Modifiers::NONE)], &env(), Draw::default());
    assert!(out.frame.fired(P).is_some() && out.frame.command_held(P));
    let out = bench.frame(vec![key_event(egui::Key::P, true, true, Modifiers::NONE)], &env(), Draw::default());
    assert!(out.frame.command_held(P), "an auto-repeat keeps it held");
    let out = bench.frame(Vec::new(), &env(), Draw::default());
    assert!(out.frame.fired(P).is_none() && out.frame.command_held(P), "held across frames without events (OS repeat delay)");
    let out = bench.frame(vec![key_event(egui::Key::P, false, false, Modifiers::NONE)], &env(), Draw::default());
    assert!(!out.frame.command_held(P), "the release ends it");
    let out = bench.frame(vec![key_event(egui::Key::Q, true, false, Modifiers::NONE)], &env(), Draw::default());
    assert!(!out.frame.command_held(P), "another key never holds it");
    let out = bench.frame(vec![key_event(egui::Key::P, true, false, Modifiers::NONE), Event::WindowFocused(false)], &env(), Draw::default());
    assert!(!out.frame.command_held(P), "focus loss releases everything");
}

/// A frame feeding one axis from the wheel and a pinch reports one sample per source, each in its
/// own unit.
#[test]
fn axis_samples_carry_their_source() {
    let mut bench = Bench::new();
    bench.frame(vec![Event::PointerMoved(ON_VIEW), key_event(egui::Key::Z, true, false, Modifiers::NONE)], &env(), published());
    let out = bench.frame(vec![wheel(MouseWheelUnit::Line, 1.0, Modifiers::NONE), Event::Zoom(2.0)], &env(), published());
    let samples: Vec<_> = out.frame.axis_samples_for_view(view::ZOOM, view_id()).map(|sample| (sample.source, sample.steps)).collect();
    assert_eq!(samples, vec![(crate::AxisSource::WheelY, 1), (crate::AxisSource::Pinch, 0)]);
    let pinch = out.frame.axis_samples(view::ZOOM).find(|sample| sample.source == crate::AxisSource::Pinch).map(|sample| sample.delta);
    assert!(pinch.is_some_and(|delta| (delta - 2.0_f32.ln()).abs() < 1e-5), "{pinch:?}");
    assert_eq!(out.frame.axis(view::ZOOM).map(|sample| sample.source), Some(crate::AxisSource::WheelY), "`axis` is the first source");
}

/// The zoom commands need a view to act on: on a frame without any published view surface (a tab
/// that draws no viewer) Ctrl+= neither fires nor is consumed; with one it fires at its centre.
#[test]
fn zoom_commands_need_a_view_focus_and_include_the_plus_key() {
    let mut bench = Bench::new();
    let out = bench.frame(vec![key_event(egui::Key::Equals, true, false, ctrl_mods())], &env(), Draw::default());
    assert!(out.frame.fired(view::ZOOM_IN).is_none(), "no view, no zoom command");
    assert_eq!(key_events(&out.events), 1, "the key stays with the tab");
    bench.frame(vec![key_event(egui::Key::Equals, false, false, ctrl_mods())], &env(), published());
    let out = bench.frame(vec![Event::PointerMoved(pos2(700.0, 550.0)), key_event(egui::Key::Plus, true, false, ctrl_mods())], &env(), published());
    let fired = out.frame.fired(view::ZOOM_IN).and_then(|fired| fired.focus);
    assert_eq!(fired.map(|focus| (focus.view, focus.pos)), Some((view_id(), VIEW_RECT.center())), "Ctrl+keypad + zooms the primary view");
}

/// A canvas-wide `Click` binding is claimed at its press (acted on at `Started`), arms the view,
/// and ends on the release.
#[test]
fn a_canvas_wide_click_is_claimed_at_the_press() {
    let mut bench = Bench::new();
    bench.frame(vec![Event::PointerMoved(ON_VIEW)], &env(), published());
    let out = bench.frame(Vec::new(), &env(), published());
    assert!(out.frame.canvas_wide_armed(view_id()), "the RMB press action arms the view");
    let out = bench.frame(vec![button_event(ON_VIEW, PointerButton::Secondary, true, Modifiers::NONE)], &env(), published());
    let claim = out.frame.claimed(view_id()).map(|claim| (claim.action, claim.phase, claim.kind, claim.origin));
    assert_eq!(claim, Some((PRESS, GesturePhase::Started, GestureKind::Click, ON_VIEW)));
    let out = bench.frame(vec![button_event(ON_VIEW, PointerButton::Secondary, false, Modifiers::NONE)], &env(), published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| claim.phase), Some(GesturePhase::Finished(ReleasePolicy::Commit)));
}

/// A press that matched no canvas-wide gesture never becomes a claim, even when the gesture's
/// condition starts to hold while the button is still down (a viewer that lifts a block mid-press
/// must not start a drag from the middle).
#[test]
fn a_gesture_inactive_at_its_press_is_never_claimed_later() {
    let mut bench = Bench::new();
    bench.frame(vec![Event::PointerMoved(ON_VIEW)], &env(), published());
    let out = bench.frame(vec![button_event(ON_VIEW, PointerButton::Middle, true, Modifiers::NONE)], &env(), published());
    assert!(out.frame.claimed(view_id()).is_none(), "blocked at the press");
    let gated = FrameEnv { contexts: vec![GATE], ..env() };
    let out = bench.frame(vec![Event::PointerMoved(ON_VIEW + vec2(30.0, 0.0))], &gated, published());
    assert!(out.frame.claimed(view_id()).is_none(), "unblocked before the release: still no claim");
    bench.frame(vec![button_event(ON_VIEW, PointerButton::Middle, false, Modifiers::NONE)], &gated, published());
    let out = bench.frame(vec![button_event(ON_VIEW, PointerButton::Middle, true, Modifiers::NONE)], &gated, published());
    assert_eq!(out.frame.claimed(view_id()).map(|claim| (claim.action, claim.phase)), Some((GATED, GesturePhase::Started)), "a press while it holds is claimed");
}
