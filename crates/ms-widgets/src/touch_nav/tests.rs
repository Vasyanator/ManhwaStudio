/*
File: crates/ms-widgets/src/touch_nav/tests.rs

Purpose:
Contract tests of the canvas touch router and the context-menu helper.

Main responsibilities:
- pure tests of `TouchRouter::route_events` on synthetic event lists shaped exactly like
  egui-winit's touch emulation (claim, strip, pairing, pan, pinch, palm, long-press,
  selection, axis lock, fling and its cancellation, tap);
- pure and adapter tests of the live on/off request (`TouchRouter::set_enabled`);
- headless `Context::run_ui` tests of the egui adapter (`TouchRouter::route`), of the
  background-layer fact `Context::layer_id_at` reports over bare canvas, and of
  `context_menu_popup` against egui's own `Popup::context_menu`.

Notes:
Event sequences follow `crates/egui/crates/egui-winit/src/lib.rs` `on_touch`: the `Touch`
event first, then the emulated mouse events of the touch that owns the emulated pointer.
*/

use egui::{
    Context, Event, Id, LayerId, Modifiers, Order, PointerButton, Popup, Pos2, RawInput, Rect,
    Sense, TouchDeviceId, TouchId, TouchPhase, Ui, ViewportId, pos2, vec2,
};

use super::router::{
    AXIS_LOCK_DECIDE_PT, AXIS_LOCK_TAN, FLING_FRICTION_PT_S2, FLING_MAX_DT_S, FLING_STOP_SPEED_PT_S,
    PanAxis, RouteResult, RouterConfig, classify_axis,
};
use super::{
    LONG_PRESS_ARM_S, PROMOTION_GRACE_S, TouchOutput, TouchRouter, TouchSelection,
    TouchSelectionEnd, TouchSurface, context_menu_popup, publish_capture_layer, publish_hole,
    publish_surface, take_touch_output,
};

const CFG: RouterConfig = RouterConfig { slop_pt: 6.0, long_press_s: LONG_PRESS_ARM_S };

fn owner() -> Id {
    Id::new("touch_nav.test.canvas")
}

fn canvas_rect() -> Rect {
    Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 400.0))
}

fn hole_rect() -> Rect {
    Rect::from_min_size(pos2(300.0, 300.0), vec2(100.0, 100.0))
}

fn surface(long_press_selection: bool) -> TouchSurface {
    TouchSurface { owner: owner(), rect: canvas_rect(), holes: vec![hole_rect()], long_press_selection }
}

fn touch(id: u64, phase: TouchPhase, pos: Pos2) -> Event {
    Event::Touch { device_id: TouchDeviceId(7), id: TouchId(id), phase, pos, force: None }
}

fn button(pos: Pos2, button: PointerButton, pressed: bool) -> Event {
    Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE }
}

/// `Touch{Start}` of the touch that owns egui-winit's emulated pointer.
fn owning_start(id: u64, pos: Pos2) -> Vec<Event> {
    vec![touch(id, TouchPhase::Start, pos), Event::PointerMoved(pos), button(pos, PointerButton::Primary, true)]
}

fn owning_move(id: u64, pos: Pos2) -> Vec<Event> {
    vec![touch(id, TouchPhase::Move, pos), Event::PointerMoved(pos)]
}

/// The emulated release carries the last MOVE position, not the end position.
fn owning_end(id: u64, pos: Pos2, last_move: Pos2) -> Vec<Event> {
    vec![touch(id, TouchPhase::End, pos), button(last_move, PointerButton::Primary, false), Event::PointerGone]
}

fn owning_cancel(id: u64, pos: Pos2) -> Vec<Event> {
    vec![touch(id, TouchPhase::Cancel, pos), Event::PointerGone]
}

/// A touch that does not own the emulated pointer gets only its `Touch` event.
fn bare(id: u64, phase: TouchPhase, pos: Pos2) -> Vec<Event> {
    vec![touch(id, phase, pos)]
}

fn concat(parts: Vec<Vec<Event>>) -> Vec<Event> {
    parts.into_iter().flatten().collect()
}

fn only_touch_events(events: &[Event]) -> bool {
    events.iter().all(|event| matches!(event, Event::Touch { .. }))
}

/// A pure-router test bench: publications and a layer lookup of last frame.
struct Bench {
    router: TouchRouter,
    surfaces: Vec<TouchSurface>,
    capture: Vec<(Id, LayerId)>,
    layer_at: fn(Pos2) -> Option<LayerId>,
}

impl Bench {
    fn new() -> Self {
        Self { router: TouchRouter::new(true), surfaces: vec![surface(true)], capture: Vec::new(), layer_at: |_| None }
    }

    fn step(&mut self, events: Vec<Event>, now: f64) -> (Vec<Event>, RouteResult) {
        let mut events = events;
        let layer_at = self.layer_at;
        let result = self.router.route_events(&mut events, now, CFG, &self.surfaces, &self.capture, &layer_at);
        (events, result)
    }

    fn output(&mut self, events: Vec<Event>, now: f64) -> TouchOutput {
        self.step(events, now).1.output(owner())
    }
}

fn assert_close(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 1e-4, "expected {expected}, got {actual}");
}

// ---------------------------------------------------------------------------------------
// Claim and strip
// ---------------------------------------------------------------------------------------

#[test]
fn claimed_touch_strips_its_start_move_and_end_emulation() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let moved = pos2(140.0, 100.0);
    let end = pos2(141.0, 100.0);
    let (out, result) = bench.step(concat(vec![owning_start(1, start), owning_move(1, moved), owning_end(1, end, moved)]), 0.0);
    assert!(only_touch_events(&out), "only the Touch events may remain: {out:?}");
    assert_eq!(out.len(), 3);
    assert!(result.dismiss_popups, "a claim start must request popup dismissal");
}

#[test]
fn claimed_touch_strips_its_cancel_emulation() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (out, _) = bench.step(owning_cancel(1, start), 0.1);
    assert_eq!(out, vec![touch(1, TouchPhase::Cancel, start)]);
}

#[test]
fn unclaimed_touches_keep_their_emulation() {
    let hole_point = hole_rect().center();
    let outside = pos2(500.0, 100.0);
    for (name, point, layer_at) in [
        ("hole", hole_point, (|_| None) as fn(Pos2) -> Option<LayerId>),
        ("outside rect", outside, |_| None),
        ("foreground layer", pos2(50.0, 50.0), |_| Some(LayerId::new(Order::Foreground, Id::new("panel")))),
        ("middle layer without capture", pos2(50.0, 50.0), |_| Some(LayerId::new(Order::Middle, Id::new("window")))),
    ] {
        let mut bench = Bench::new();
        bench.layer_at = layer_at;
        let input = concat(vec![owning_start(1, point), owning_move(1, point + vec2(30.0, 0.0)), owning_end(1, point + vec2(30.0, 0.0), point + vec2(30.0, 0.0))]);
        let (out, result) = bench.step(input.clone(), 0.0);
        assert_eq!(out, input, "{name}: events must pass untouched");
        assert!(!result.dismiss_popups, "{name}: no claim, no dismissal");
        assert_eq!(result.output(owner()), TouchOutput::default(), "{name}");
    }
}

#[test]
fn background_layer_on_top_still_claims() {
    let mut bench = Bench::new();
    bench.layer_at = |_| Some(LayerId::background());
    let (out, _) = bench.step(owning_start(1, pos2(50.0, 50.0)), 0.0);
    assert!(only_touch_events(&out));
}

#[test]
fn capture_layer_of_the_owner_claims() {
    let capture = LayerId::new(Order::Middle, Id::new("capture.overlay"));
    let mut bench = Bench::new();
    bench.layer_at = |_| Some(LayerId::new(Order::Middle, Id::new("capture.overlay")));
    bench.capture = vec![(owner(), capture)];
    let (out, _) = bench.step(owning_start(1, pos2(50.0, 50.0)), 0.0);
    assert!(only_touch_events(&out), "a capture layer of the owner must not block the claim");

    // The same layer registered for ANOTHER owner does not count.
    let mut other = Bench::new();
    other.layer_at = bench.layer_at;
    other.capture = vec![(Id::new("someone.else"), capture)];
    let input = owning_start(1, pos2(50.0, 50.0));
    let (out, _) = other.step(input.clone(), 0.0);
    assert_eq!(out, input);
}

#[test]
fn non_emulated_events_next_to_a_claimed_touch_are_kept() {
    // A second touch does not own the pointer: whatever follows its Touch event is someone
    // else's and must survive (here: a mouse move at a far position).
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let mouse = Event::PointerMoved(pos2(600.0, 500.0));
    let (out, _) = bench.step(vec![touch(2, TouchPhase::Start, pos2(200.0, 200.0)), mouse.clone()], 0.01);
    assert_eq!(out, vec![touch(2, TouchPhase::Start, pos2(200.0, 200.0)), mouse]);
}

// ---------------------------------------------------------------------------------------
// Pairing invariant
// ---------------------------------------------------------------------------------------

#[test]
fn mouse_buttons_pass_while_a_finger_is_claimed() {
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let mouse_at = pos2(250.0, 250.0);
    let press = button(mouse_at, PointerButton::Secondary, true);
    let release = button(mouse_at, PointerButton::Secondary, false);
    let (out, _) = bench.step(vec![press.clone()], 0.01);
    assert_eq!(out, vec![press]);
    let (out, _) = bench.step(vec![release.clone()], 0.02);
    assert_eq!(out, vec![release]);
}

#[test]
fn promoted_press_and_its_release_are_stripped_together() {
    let mut bench = Bench::new();
    let finger = pos2(100.0, 100.0);
    let (_, _) = bench.step(bare(1, TouchPhase::Start, finger), 0.0);
    // A platform-promoted copy: non-adjacent, within the promotion radius.
    let promoted = pos2(101.0, 100.0);
    let (out, _) = bench.step(vec![Event::PointerMoved(promoted), button(promoted, PointerButton::Primary, true)], 0.01);
    assert!(out.is_empty(), "promoted move and press must be stripped: {out:?}");
    let (_, _) = bench.step(bare(1, TouchPhase::End, finger), 0.02);
    // The promoted release arrives after the touch ended, far from it: still stripped,
    // because its press was.
    let (out, _) = bench.step(vec![button(pos2(300.0, 10.0), PointerButton::Primary, false)], 0.03);
    assert!(out.is_empty(), "the release of a stripped press must be stripped: {out:?}");
    // The next real mouse release (no stripped press pending) passes.
    let real = button(pos2(300.0, 10.0), PointerButton::Primary, false);
    let (out, _) = bench.step(vec![real.clone()], 0.04);
    assert_eq!(out, vec![real]);
}

#[test]
fn promotion_grace_expires() {
    let mut bench = Bench::new();
    let finger = pos2(100.0, 100.0);
    let (_, _) = bench.step(bare(1, TouchPhase::Start, finger), 0.0);
    let (_, _) = bench.step(bare(1, TouchPhase::End, finger), 0.1);
    let within = button(finger, PointerButton::Primary, true);
    let (out, _) = bench.step(vec![within], 0.1 + PROMOTION_GRACE_S * 0.5);
    assert!(out.is_empty(), "a press at the end point within the grace period is promoted");
    let (_, _) = bench.step(vec![button(finger, PointerButton::Primary, false)], 0.1 + PROMOTION_GRACE_S * 0.6);
    let late = button(finger, PointerButton::Primary, true);
    let (out, _) = bench.step(vec![late.clone()], 0.1 + PROMOTION_GRACE_S * 2.0);
    assert_eq!(out, vec![late], "after the grace period a press at that point is a real press");
}

#[test]
fn unclaimed_finger_emulation_does_not_count_as_a_palm() {
    // A finger resting on a panel owns the emulated pointer and its press reaches egui; a
    // second finger on the canvas must still navigate.
    let mut bench = Bench::new();
    let panel_finger = pos2(500.0, 100.0);
    let (out, _) = bench.step(owning_start(1, panel_finger), 0.0);
    assert_eq!(out.len(), 3, "the panel finger keeps its emulation");
    let (_, _) = bench.step(bare(2, TouchPhase::Start, pos2(100.0, 100.0)), 0.01);
    let output = bench.output(bare(2, TouchPhase::Move, pos2(130.0, 100.0)), 0.02);
    let navigation = output.navigation.expect("the canvas finger pans");
    assert_eq!(navigation.pan_delta(), vec2(30.0, 0.0));
}

/// Asserts that a fresh canvas finger navigates, i.e. no mouse/pen button is still tracked as
/// held (`passthrough_down` drained).
fn assert_no_palm_left(bench: &mut Bench, id: u64, now: f64) {
    let (_, _) = bench.step(owning_start(id, pos2(50.0, 50.0)), now);
    let output = bench.output(owning_move(id, pos2(80.0, 50.0)), now + 0.01);
    assert!(output.navigation.is_some(), "a held-button palm must not survive the real release");
}

#[test]
fn real_release_after_a_claimed_non_emulating_touch_ends_survives() {
    // Palm = two claimed contacts, only the first owns egui-winit's emulated pointer; the pen
    // writes meanwhile. When the hand lifts, the pen's release lands right after the second
    // contact's End: it is the pen's, not an emulated one, and must reach egui.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let (_, _) = bench.step(bare(2, TouchPhase::Start, pos2(200.0, 200.0)), 0.01);
    let pen = pos2(250.0, 40.0);
    let press = button(pen, PointerButton::Primary, true);
    let (out, _) = bench.step(vec![press.clone()], 0.02);
    assert_eq!(out, vec![press]);
    let release = button(pen, PointerButton::Primary, false);
    let (out, _) = bench.step(
        concat(vec![owning_end(1, pos2(100.0, 100.0), pos2(100.0, 100.0)), bare(2, TouchPhase::End, pos2(200.0, 200.0)), vec![release.clone()]]),
        0.03,
    );
    assert_eq!(out, vec![touch(1, TouchPhase::End, pos2(100.0, 100.0)), touch(2, TouchPhase::End, pos2(200.0, 200.0)), release]);
    assert_no_palm_left(&mut bench, 3, 0.04);
}

#[test]
fn real_release_after_an_unclaimed_non_emulating_touch_ends_drains_the_palm() {
    // Same shape off the canvas: the second contact lands on a panel (unclaimed) and does not
    // own the emulated pointer. The pen's release after its End must still be accounted as the
    // pen's, or the router keeps a phantom held button and navigation stays dead.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(500.0, 100.0)), 0.0);
    let (_, _) = bench.step(bare(2, TouchPhase::Start, pos2(600.0, 100.0)), 0.01);
    let pen = pos2(250.0, 40.0);
    let (_, _) = bench.step(vec![button(pen, PointerButton::Primary, true)], 0.02);
    let release = button(pen, PointerButton::Primary, false);
    let (out, _) = bench.step(concat(vec![bare(2, TouchPhase::End, pos2(600.0, 100.0)), vec![release.clone()]]), 0.03);
    assert_eq!(out, vec![touch(2, TouchPhase::End, pos2(600.0, 100.0)), release]);
    let input = owning_end(1, pos2(500.0, 100.0), pos2(500.0, 100.0));
    let (out, _) = bench.step(input.clone(), 0.035);
    assert_eq!(out, input, "the unclaimed emulating finger keeps its release and PointerGone");
    assert_no_palm_left(&mut bench, 3, 0.04);
}

#[test]
fn pointer_gone_after_a_non_emulating_cancel_is_kept() {
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let (_, _) = bench.step(bare(2, TouchPhase::Start, pos2(200.0, 200.0)), 0.01);
    let (out, _) = bench.step(concat(vec![bare(2, TouchPhase::Cancel, pos2(200.0, 200.0)), vec![Event::PointerGone]]), 0.02);
    assert_eq!(out, vec![touch(2, TouchPhase::Cancel, pos2(200.0, 200.0)), Event::PointerGone]);
    // The emulating finger's own Cancel still strips its PointerGone.
    let (out, _) = bench.step(owning_cancel(1, pos2(100.0, 100.0)), 0.03);
    assert_eq!(out, vec![touch(1, TouchPhase::Cancel, pos2(100.0, 100.0))]);
}

// ---------------------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------------------

#[test]
fn one_finger_pans_and_catches_up_the_slop() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let output = bench.output(owning_start(1, start), 0.0);
    assert_eq!(output.navigation, None);
    // Within the slop: the pending long-press does not move the page yet.
    let output = bench.output(owning_move(1, pos2(103.0, 100.0)), 0.05);
    assert_eq!(output.navigation, None);
    // Beyond the slop: the page moves by the WHOLE distance from the start.
    let navigation = bench.output(owning_move(1, pos2(120.0, 110.0)), 0.1).navigation.expect("pan");
    assert_eq!(navigation.prev_centroid, start);
    assert_eq!(navigation.pan_delta(), vec2(20.0, 10.0));
    assert_eq!(navigation.touches, 1);
    assert_close(navigation.zoom_factor, 1.0);
    let navigation = bench.output(owning_move(1, pos2(125.0, 110.0)), 0.15).navigation.expect("pan");
    assert_eq!(navigation.pan_delta(), vec2(5.0, 0.0));
}

#[test]
fn two_fingers_pinch_about_their_centroid() {
    let mut bench = Bench::new();
    let (_, _) = bench.step(concat(vec![owning_start(1, pos2(100.0, 100.0)), bare(2, TouchPhase::Start, pos2(200.0, 100.0))]), 0.0);
    let output = bench.output(concat(vec![owning_move(1, pos2(90.0, 100.0)), bare(2, TouchPhase::Move, pos2(210.0, 100.0))]), 0.05);
    let navigation = output.navigation.expect("pinch");
    assert_eq!(navigation.touches, 2);
    assert_eq!(navigation.prev_centroid, pos2(150.0, 100.0));
    assert_eq!(navigation.centroid, pos2(150.0, 100.0));
    assert_close(navigation.zoom_factor, 1.2);
    assert_eq!(output.long_press_armed_at, None);
}

#[test]
fn adding_or_lifting_a_finger_mid_frame_never_jumps() {
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let (_, _) = bench.step(owning_move(1, pos2(120.0, 100.0)), 0.05);
    // Finger 2 lands far away in the same frame finger 1 moves by 4: only finger 1 counts.
    let output = bench.output(concat(vec![owning_move(1, pos2(124.0, 100.0)), bare(2, TouchPhase::Start, pos2(300.0, 100.0))]), 0.1);
    let navigation = output.navigation.expect("pan of finger 1");
    assert_eq!(navigation.pan_delta(), vec2(4.0, 0.0));
    assert_eq!(navigation.touches, 1);
    // Both move by +2: a plain two-finger pan, no zoom.
    let navigation = bench
        .output(concat(vec![owning_move(1, pos2(126.0, 100.0)), bare(2, TouchPhase::Move, pos2(302.0, 100.0))]), 0.15)
        .navigation
        .expect("two-finger pan");
    assert_eq!(navigation.pan_delta(), vec2(2.0, 0.0));
    assert_close(navigation.zoom_factor, 1.0);
    // Finger 2 lifts far away while finger 1 moves by 1: only finger 1 counts again.
    let navigation = bench
        .output(concat(vec![owning_move(1, pos2(127.0, 100.0)), bare(2, TouchPhase::End, pos2(400.0, 100.0))]), 0.2)
        .navigation
        .expect("pan of finger 1");
    assert_eq!(navigation.pan_delta(), vec2(1.0, 0.0));
    assert_close(navigation.zoom_factor, 1.0);
}

#[test]
fn pen_button_held_blocks_navigation_and_long_press() {
    let mut bench = Bench::new();
    let pen = pos2(380.0, 20.0);
    let (out, _) = bench.step(vec![Event::PointerMoved(pen), button(pen, PointerButton::Primary, true)], 0.0);
    assert_eq!(out.len(), 2, "the pen's own events pass");
    // A palm lands on the canvas while the pen is down.
    let (_, result) = bench.step(bare(1, TouchPhase::Start, pos2(100.0, 100.0)), 0.01);
    assert_eq!(result.next_deadline(), None, "no long-press may start under a pen");
    let output = bench.output(bare(1, TouchPhase::Move, pos2(160.0, 100.0)), 0.02);
    assert_eq!(output.navigation, None);
    let output = bench.output(Vec::new(), 1.0);
    assert_eq!(output, TouchOutput::default(), "the palm never arms a long-press");
    // Pen lifts; the palm's pan is measured from where the palm IS, not from where it landed:
    // its axis decision waits for `AXIS_LOCK_DECIDE_PT` from there, then catches up.
    let (_, _) = bench.step(vec![button(pen, PointerButton::Primary, false)], 1.1);
    let output = bench.output(bare(1, TouchPhase::Move, pos2(165.0, 100.0)), 1.2);
    assert_eq!(output.navigation, None, "5 pt from the lift point: still undecided");
    let navigation = bench.output(bare(1, TouchPhase::Move, pos2(172.0, 101.0)), 1.25).navigation.expect("pan");
    assert_eq!(navigation.prev_centroid, pos2(160.0, 100.0));
    assert_eq!(navigation.pan_delta(), vec2(12.0, 0.0));
}

#[test]
fn pen_press_interrupts_a_pending_long_press() {
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let (_, _) = bench.step(vec![button(pos2(380.0, 20.0), PointerButton::Primary, true)], 0.1);
    let output = bench.output(Vec::new(), 1.0);
    assert_eq!(output.long_press_armed_at, None);
}

// ---------------------------------------------------------------------------------------
// Long-press: arm, menu, selection
// ---------------------------------------------------------------------------------------

#[test]
fn long_press_arms_after_the_hold_time_and_opens_the_menu_on_release() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, result) = bench.step(owning_start(1, start), 0.0);
    assert_eq!(result.next_deadline(), Some(LONG_PRESS_ARM_S));
    let (_, result) = bench.step(Vec::new(), LONG_PRESS_ARM_S * 0.5);
    assert_eq!(result.output(owner()).long_press_armed_at, None);
    assert_eq!(result.next_deadline(), Some(LONG_PRESS_ARM_S), "reported while pending");
    let (_, result) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    assert_eq!(result.output(owner()).long_press_armed_at, Some(start));
    assert_eq!(result.next_deadline(), None, "no deadline once armed");
    assert_eq!(result.output(owner()).long_press_at, None, "the menu waits for the release");
    let output = bench.output(owning_end(1, start, start), LONG_PRESS_ARM_S + 0.3);
    assert_eq!(output.long_press_at, Some(start));
    assert_eq!(output.navigation, None);
    assert_eq!(output.long_press_armed_at, None);
}

#[test]
fn jitter_within_the_slop_still_arms() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(owning_move(1, pos2(104.0, 103.0)), 0.1);
    let output = bench.output(owning_move(1, pos2(97.0, 98.0)), LONG_PRESS_ARM_S + 0.01);
    assert_eq!(output.navigation, None, "jitter never pans");
    assert_eq!(output.long_press_armed_at, Some(start), "armed at the START point");
    let output = bench.output(owning_end(1, pos2(98.0, 99.0), pos2(97.0, 98.0)), LONG_PRESS_ARM_S + 0.1);
    assert_eq!(output.long_press_at, Some(start));
}

#[test]
fn moving_before_the_hold_time_pans_and_never_arms() {
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let output = bench.output(owning_move(1, pos2(100.0, 140.0)), 0.1);
    assert!(output.navigation.is_some());
    let (_, result) = bench.step(Vec::new(), 2.0);
    assert_eq!(result.output(owner()), TouchOutput::default());
    assert_eq!(result.next_deadline(), None);
    let output = bench.output(owning_end(1, pos2(100.0, 140.0), pos2(100.0, 140.0)), 2.1);
    assert_eq!(output.long_press_at, None);
}

#[test]
fn second_finger_before_arming_pinches_instead() {
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let (_, result) = bench.step(bare(2, TouchPhase::Start, pos2(200.0, 100.0)), 0.1);
    assert_eq!(result.next_deadline(), None, "the second finger disarms the long-press");
    let (_, result) = bench.step(Vec::new(), 2.0);
    assert_eq!(result.output(owner()).long_press_armed_at, None);
    let output = bench.output(concat(vec![owning_end(1, pos2(100.0, 100.0), pos2(100.0, 100.0)), bare(2, TouchPhase::End, pos2(200.0, 100.0))]), 2.1);
    assert_eq!(output.long_press_at, None);
}

#[test]
fn second_finger_after_arming_cancels_and_dismisses() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    let (_, result) = bench.step(bare(2, TouchPhase::Start, pos2(200.0, 200.0)), LONG_PRESS_ARM_S + 0.1);
    assert!(result.dismiss_popups, "the second finger's claim closes an open menu");
    assert_eq!(result.output(owner()).long_press_armed_at, None);
    // The spent finger never navigates, the second finger pans alone.
    let output = bench.output(concat(vec![owning_move(1, pos2(160.0, 100.0)), bare(2, TouchPhase::Move, pos2(210.0, 200.0))]), LONG_PRESS_ARM_S + 0.2);
    let navigation = output.navigation.expect("the second finger pans");
    assert_eq!(navigation.touches, 1);
    assert_eq!(navigation.pan_delta(), vec2(10.0, 0.0));
    assert_eq!(output.selection, None);
    let output = bench.output(owning_end(1, pos2(160.0, 100.0), pos2(160.0, 100.0)), LONG_PRESS_ARM_S + 0.3);
    assert_eq!(output.long_press_at, None, "a cancelled long-press never opens the menu");
}

#[test]
fn spent_finger_lifting_does_not_pan_under_the_menu() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    let output = bench.output(owning_end(1, pos2(102.0, 101.0), start), LONG_PRESS_ARM_S + 0.2);
    assert_eq!(output.long_press_at, Some(start));
    assert_eq!(output.navigation, None);
}

#[test]
fn drag_after_arming_is_a_selection_from_the_press_point() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    let output = bench.output(owning_move(1, pos2(150.0, 130.0)), LONG_PRESS_ARM_S + 0.1);
    assert_eq!(output.selection, Some(TouchSelection { origin: start, pos: pos2(150.0, 130.0), began: true, end: None }));
    assert_eq!(output.navigation, None, "a selection never pans");
    let output = bench.output(Vec::new(), LONG_PRESS_ARM_S + 0.15);
    assert_eq!(output.selection, Some(TouchSelection { origin: start, pos: pos2(150.0, 130.0), began: false, end: None }), "reported every frame while it lasts");
    let output = bench.output(owning_move(1, pos2(180.0, 160.0)), LONG_PRESS_ARM_S + 0.2);
    assert_eq!(output.selection.map(|selection| (selection.pos, selection.began)), Some((pos2(180.0, 160.0), false)));
    let output = bench.output(owning_end(1, pos2(181.0, 160.0), pos2(180.0, 160.0)), LONG_PRESS_ARM_S + 0.3);
    assert_eq!(output.selection, Some(TouchSelection { origin: start, pos: pos2(181.0, 160.0), began: false, end: Some(TouchSelectionEnd::Released) }));
    assert_eq!(output.long_press_at, None, "a selection never opens the menu");
}

#[test]
fn second_finger_cancels_a_running_selection() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    let (_, _) = bench.step(owning_move(1, pos2(150.0, 130.0)), LONG_PRESS_ARM_S + 0.1);
    let output = bench.output(bare(2, TouchPhase::Start, pos2(250.0, 250.0)), LONG_PRESS_ARM_S + 0.2);
    assert_eq!(output.selection.and_then(|selection| selection.end), Some(TouchSelectionEnd::Cancelled));
    let output = bench.output(owning_move(1, pos2(170.0, 130.0)), LONG_PRESS_ARM_S + 0.3);
    assert_eq!(output.selection, None);
    assert_eq!(output.navigation, None, "the spent finger never pans");
}

#[test]
fn selection_within_one_frame_reports_begin_and_end() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    let output = bench.output(concat(vec![owning_move(1, pos2(150.0, 100.0)), owning_end(1, pos2(150.0, 100.0), pos2(150.0, 100.0))]), LONG_PRESS_ARM_S + 0.1);
    assert_eq!(output.selection, Some(TouchSelection { origin: start, pos: pos2(150.0, 100.0), began: true, end: Some(TouchSelectionEnd::Released) }));
}

#[test]
fn drag_after_arming_pans_where_the_owner_has_no_selection() {
    let mut bench = Bench::new();
    bench.surfaces = vec![surface(false)];
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    let output = bench.output(owning_move(1, pos2(130.0, 100.0)), LONG_PRESS_ARM_S + 0.1);
    assert_eq!(output.selection, None);
    assert_eq!(output.navigation.map(|navigation| navigation.pan_delta()), Some(vec2(30.0, 0.0)));
}

#[test]
fn selection_mode_dropped_before_arming_makes_the_drag_pan() {
    // Translation's mode is a held key: released while the finger rests, the owner publishes
    // `long_press_selection == false` from then on, and the drag must pan instead of starting
    // a selection the owner can no longer begin.
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    bench.surfaces = vec![surface(false)];
    let (_, _) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    let output = bench.output(owning_move(1, pos2(130.0, 100.0)), LONG_PRESS_ARM_S + 0.1);
    assert_eq!(output.selection, None);
    assert_eq!(output.navigation.map(|navigation| navigation.pan_delta()), Some(vec2(30.0, 0.0)));
}

#[test]
fn selection_mode_dropped_after_arming_before_the_drag_makes_it_pan() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    bench.surfaces = vec![surface(false)];
    let output = bench.output(owning_move(1, pos2(130.0, 100.0)), LONG_PRESS_ARM_S + 0.1);
    assert_eq!(output.selection, None);
    assert_eq!(output.navigation.map(|navigation| navigation.pan_delta()), Some(vec2(30.0, 0.0)));
}

#[test]
fn selection_mode_gained_before_the_drag_selects_and_a_running_selection_keeps_going() {
    let mut bench = Bench::new();
    bench.surfaces = vec![surface(false)];
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    bench.surfaces = vec![surface(true)];
    let (_, _) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    let output = bench.output(owning_move(1, pos2(150.0, 130.0)), LONG_PRESS_ARM_S + 0.1);
    assert_eq!(output.selection.map(|selection| selection.began), Some(true));
    // Once selecting, a later publication without the mode does not turn it into a pan: the
    // owner already began its selection and ends it on lift.
    bench.surfaces = vec![surface(false)];
    let output = bench.output(owning_move(1, pos2(170.0, 130.0)), LONG_PRESS_ARM_S + 0.2);
    assert_eq!(output.selection.map(|selection| selection.pos), Some(pos2(170.0, 130.0)));
    assert_eq!(output.navigation, None);
    // A frame without any publication of the owner keeps the last known mode.
    let mut fresh = Bench::new();
    let (_, _) = fresh.step(owning_start(1, start), 0.0);
    fresh.surfaces = Vec::new();
    let (_, _) = fresh.step(Vec::new(), LONG_PRESS_ARM_S);
    let output = fresh.output(owning_move(1, pos2(150.0, 130.0)), LONG_PRESS_ARM_S + 0.1);
    assert_eq!(output.selection.map(|selection| selection.began), Some(true));
}

#[test]
fn focus_loss_drops_claims_and_cancels_the_selection() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    let (_, _) = bench.step(owning_move(1, pos2(150.0, 130.0)), LONG_PRESS_ARM_S + 0.1);
    let output = bench.output(vec![Event::WindowFocused(false)], LONG_PRESS_ARM_S + 0.2);
    assert_eq!(output.selection.and_then(|selection| selection.end), Some(TouchSelectionEnd::Cancelled));
    let (_, result) = bench.step(Vec::new(), LONG_PRESS_ARM_S + 0.3);
    assert_eq!(result.owners, Vec::new());
}

// ---------------------------------------------------------------------------------------
// Axis lock
// ---------------------------------------------------------------------------------------

#[test]
fn axis_classification_honours_the_tolerance_edges() {
    let edge = 100.0 * AXIS_LOCK_TAN;
    for (delta, expected) in [
        (vec2(0.0, 30.0), PanAxis::Vertical),
        (vec2(0.0, -30.0), PanAxis::Vertical),
        (vec2(30.0, 0.0), PanAxis::Horizontal),
        (vec2(-30.0, 0.0), PanAxis::Horizontal),
        // Exactly on the 22.5° line still locks; just past it is free.
        (vec2(edge, 100.0), PanAxis::Vertical),
        (vec2(-edge, -100.0), PanAxis::Vertical),
        (vec2(edge + 0.5, 100.0), PanAxis::Free),
        (vec2(100.0, edge), PanAxis::Horizontal),
        (vec2(-100.0, edge), PanAxis::Horizontal),
        (vec2(100.0, edge + 0.5), PanAxis::Free),
        (vec2(10.0, 10.0), PanAxis::Free),
        (vec2(-10.0, 7.0), PanAxis::Free),
    ] {
        assert_eq!(classify_axis(delta), expected, "{delta:?}");
    }
    assert_close(AXIS_LOCK_TAN, 22.5_f32.to_radians().tan());
}

#[test]
fn lone_finger_waits_for_the_decision_then_catches_up_along_the_axis() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    // Beyond the slop but short of the decision distance: the page does not move yet.
    let output = bench.output(owning_move(1, pos2(107.0, 102.0)), 0.02);
    assert!(start.distance(pos2(107.0, 102.0)) < AXIS_LOCK_DECIDE_PT);
    assert_eq!(output.navigation, None);
    // Decided horizontal: the whole horizontal distance from the start, no vertical drift.
    let navigation = bench.output(owning_move(1, pos2(112.0, 103.0)), 0.04).navigation.expect("pan");
    assert_eq!(navigation.prev_centroid, start);
    assert_eq!(navigation.pan_delta(), vec2(12.0, 0.0));
    // Sticky: later vertical drift never pans, even a large one.
    let navigation = bench.output(owning_move(1, pos2(120.0, 115.0)), 0.06).navigation.expect("pan");
    assert_eq!(navigation.pan_delta(), vec2(8.0, 0.0));
    let output = bench.output(owning_move(1, pos2(120.0, 190.0)), 0.08);
    assert_eq!(output.navigation, None, "a locked horizontal pan ignores a vertical move");
}

#[test]
fn vertical_and_diagonal_decisions() {
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let navigation = bench.output(owning_move(1, pos2(103.0, 120.0)), 0.02).navigation.expect("pan");
    assert_eq!(navigation.pan_delta(), vec2(0.0, 20.0));

    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let navigation = bench.output(owning_move(1, pos2(110.0, 110.0)), 0.02).navigation.expect("pan");
    assert_eq!(navigation.pan_delta(), vec2(10.0, 10.0), "a diagonal move pans both axes");
    let navigation = bench.output(owning_move(1, pos2(130.0, 112.0)), 0.04).navigation.expect("pan");
    assert_eq!(navigation.pan_delta(), vec2(20.0, 2.0), "free stays free");
}

#[test]
fn second_finger_makes_the_pan_free() {
    // Locked first, then a second finger: both axes pan from then on.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let (_, _) = bench.step(owning_move(1, pos2(120.0, 100.0)), 0.02);
    let (_, _) = bench.step(bare(2, TouchPhase::Start, pos2(200.0, 100.0)), 0.04);
    let navigation = bench
        .output(concat(vec![owning_move(1, pos2(123.0, 105.0)), bare(2, TouchPhase::Move, pos2(203.0, 105.0))]), 0.06)
        .navigation
        .expect("two-finger pan");
    assert_eq!(navigation.pan_delta(), vec2(3.0, 5.0));
    // Finger 2 lifts: finger 1 stays free for the rest of the gesture.
    let (_, _) = bench.step(bare(2, TouchPhase::End, pos2(203.0, 105.0)), 0.08);
    let navigation = bench.output(owning_move(1, pos2(125.0, 115.0)), 0.1).navigation.expect("pan");
    assert_eq!(navigation.pan_delta(), vec2(2.0, 10.0));

    // Undecided when the second finger lands: it catches up from the start, unprojected.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(100.0, 100.0)), 0.0);
    let (_, _) = bench.step(owning_move(1, pos2(107.0, 101.0)), 0.02);
    let navigation = bench.output(bare(2, TouchPhase::Start, pos2(200.0, 100.0)), 0.04).navigation.expect("catch-up");
    assert_eq!(navigation.pan_delta(), vec2(7.0, 1.0));
    assert_eq!(navigation.touches, 1);
}

// ---------------------------------------------------------------------------------------
// Fling
// ---------------------------------------------------------------------------------------

/// Frame interval of the fling tests, seconds.
const FRAME_S: f64 = 0.01;

/// Drags finger 1 from `start` by `step` per frame for `frames` frames (one frame per
/// `FRAME_S`, starting at `t0`) and lifts it one frame later, still moving. Returns the lift
/// frame's result and its time.
fn throw(bench: &mut Bench, start: Pos2, step: egui::Vec2, frames: u32, t0: f64) -> (RouteResult, f64) {
    let (_, _) = bench.step(owning_start(1, start), t0);
    let mut pos = start;
    let mut now = t0;
    for _ in 0..frames {
        pos += step;
        now += FRAME_S;
        let (_, _) = bench.step(owning_move(1, pos), now);
    }
    now += FRAME_S;
    let (_, result) = bench.step(owning_end(1, pos + step, pos), now);
    (result, now)
}

#[test]
fn locked_lift_flings_with_egui_kinetic_law() {
    let mut bench = Bench::new();
    // 6 pt per 10 ms = 600 pt/s horizontally, with a vertical drift the lock discards.
    let (result, lift) = throw(&mut bench, pos2(50.0, 100.0), vec2(6.0, 0.5), 20, 0.0);
    assert_eq!(result.next_deadline(), Some(lift), "the lift frame asks for the next frame at once");
    let mut speed = 600.0_f32;
    let mut total = 0.0_f32;
    let mut now = lift;
    let mut steps = 0_u32;
    loop {
        now += FRAME_S;
        let (_, result) = bench.step(Vec::new(), now);
        let Some(navigation) = result.output(owner()).navigation else {
            assert_eq!(result.next_deadline(), None, "a finished glide requests nothing");
            break;
        };
        steps += 1;
        assert_eq!(navigation.touches, 0, "a kinetic frame has no touches");
        assert_eq!(navigation.prev_centroid, pos2(176.0, 110.5), "anchored at the lift point");
        assert_close(navigation.zoom_factor, 1.0);
        assert_eq!(navigation.pan_delta().y, 0.0, "the glide follows the lock");
        assert_eq!(result.next_deadline(), Some(now));
        // egui's step (`scroll_area.rs:866-877`): v -= friction * dt, then move by v * dt.
        speed -= FLING_FRICTION_PT_S2 * 0.01;
        if steps <= 2 {
            assert!((navigation.pan_delta().x - speed * 0.01).abs() < 0.01, "step {steps}: {navigation:?}");
        }
        total += navigation.pan_delta().x;
        assert!(steps < 200, "the glide must stop");
    }
    // 600 pt/s minus 10 per step, stopping under 20 pt/s: about 59 steps and 177 pt.
    assert!((58..=60).contains(&steps), "steps {steps}");
    assert!((total - 177.0).abs() < 7.0, "total {total}");
    assert!(speed < FLING_STOP_SPEED_PT_S * 2.0);
}

#[test]
fn fling_steps_are_clamped_to_the_max_dt() {
    let mut bench = Bench::new();
    let (_, lift) = throw(&mut bench, pos2(50.0, 100.0), vec2(0.0, 6.0), 20, 0.0);
    // A stalled frame (1 s) integrates at most `FLING_MAX_DT_S`.
    let navigation = bench.output(Vec::new(), lift + 1.0).navigation.expect("glide");
    let dt = FLING_MAX_DT_S;
    let expected = (600.0 - FLING_FRICTION_PT_S2 * dt) * dt;
    assert!((navigation.pan_delta().y - expected).abs() < 0.1, "{navigation:?}");
    assert_eq!(navigation.pan_delta().x, 0.0);
}

#[test]
fn diagonal_or_two_finger_lifts_never_fling() {
    let mut bench = Bench::new();
    let (result, lift) = throw(&mut bench, pos2(50.0, 50.0), vec2(6.0, 6.0), 20, 0.0);
    assert_eq!(result.next_deadline(), None);
    assert_eq!(bench.output(Vec::new(), lift + FRAME_S), TouchOutput::default());

    let mut bench = Bench::new();
    let (_, _) = bench.step(concat(vec![owning_start(1, pos2(50.0, 100.0)), bare(2, TouchPhase::Start, pos2(50.0, 200.0))]), 0.0);
    let mut now = 0.0;
    for frame in 1..=10_u8 {
        now += FRAME_S;
        let x = 50.0 + 6.0 * f32::from(frame);
        let (_, _) = bench.step(concat(vec![owning_move(1, pos2(x, 100.0)), bare(2, TouchPhase::Move, pos2(x, 200.0))]), now);
    }
    now += FRAME_S;
    let (_, result) = bench.step(concat(vec![owning_end(1, pos2(116.0, 100.0), pos2(110.0, 100.0)), bare(2, TouchPhase::End, pos2(116.0, 200.0))]), now);
    assert_eq!(result.next_deadline(), None);
    assert_eq!(bench.output(Vec::new(), now + FRAME_S), TouchOutput::default());
}

#[test]
fn a_finger_that_rested_before_lifting_does_not_fling() {
    // Frames kept running while it rested (the per-frame samples age the motion out).
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(50.0, 100.0)), 0.0);
    let mut now = 0.0;
    let mut pos = pos2(50.0, 100.0);
    for _ in 0..10 {
        now += FRAME_S;
        pos += vec2(6.0, 0.0);
        let (_, _) = bench.step(owning_move(1, pos), now);
    }
    for _ in 0..20 {
        now += FRAME_S;
        let (_, _) = bench.step(Vec::new(), now);
    }
    now += FRAME_S;
    let (_, result) = bench.step(owning_end(1, pos, pos), now);
    assert_eq!(result.next_deadline(), None);
    assert_eq!(bench.output(Vec::new(), now + FRAME_S).navigation, None);

    // No frame ran while it rested: the lift still finds too few recent samples.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, pos2(50.0, 100.0)), 0.0);
    let (_, _) = bench.step(owning_move(1, pos2(80.0, 100.0)), 0.01);
    let (_, _) = bench.step(owning_move(1, pos2(110.0, 100.0)), 0.02);
    let (_, result) = bench.step(owning_end(1, pos2(110.0, 100.0), pos2(110.0, 100.0)), 0.5);
    assert_eq!(result.next_deadline(), None);
    assert_eq!(bench.output(Vec::new(), 0.51).navigation, None);
}

/// Starts a horizontal glide and returns the lift time.
fn gliding_bench() -> (Bench, f64) {
    let mut bench = Bench::new();
    let (_, lift) = throw(&mut bench, pos2(50.0, 100.0), vec2(6.0, 0.0), 20, 0.0);
    let navigation = bench.output(Vec::new(), lift + FRAME_S).navigation;
    assert!(navigation.is_some(), "the glide runs");
    (bench, lift + FRAME_S)
}

#[test]
fn every_cancel_cause_stops_the_fling() {
    let wheel = Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, -3.0), phase: TouchPhase::Move, modifiers: Modifiers::NONE };
    for (name, events) in [
        ("mouse or pen press", vec![button(pos2(380.0, 20.0), PointerButton::Primary, true)]),
        ("wheel", vec![wheel]),
        ("pinch zoom event", vec![Event::Zoom(1.1)]),
        ("focus loss", vec![Event::WindowFocused(false)]),
    ] {
        let (mut bench, now) = gliding_bench();
        let (_, result) = bench.step(events, now + FRAME_S);
        assert_eq!(result.output(owner()).navigation, None, "{name}");
        assert_eq!(result.next_deadline(), None, "{name}");
        assert_eq!(bench.output(Vec::new(), now + 2.0 * FRAME_S).navigation, None, "{name}: stays stopped");
    }

    // The canvas stopped publishing (tab switched): gone, and it does not resume.
    let (mut bench, now) = gliding_bench();
    let surfaces = std::mem::take(&mut bench.surfaces);
    assert_eq!(bench.output(Vec::new(), now + FRAME_S).navigation, None);
    bench.surfaces = surfaces;
    assert_eq!(bench.output(Vec::new(), now + 2.0 * FRAME_S).navigation, None);

    // Gestures switched off: the switch applies at once (a glide holds no claim) and resets it.
    let (mut bench, now) = gliding_bench();
    bench.router.set_enabled(false);
    assert_eq!(bench.router.apply_requested(), Some(false));
    bench.router.set_enabled(true);
    assert_eq!(bench.router.apply_requested(), Some(true));
    assert_eq!(bench.output(Vec::new(), now + FRAME_S).navigation, None);
}

#[test]
fn a_touch_on_the_canvas_catches_the_fling_without_tapping() {
    let (mut bench, now) = gliding_bench();
    let catch = pos2(200.0, 200.0);
    let (_, result) = bench.step(owning_start(2, catch), now + FRAME_S);
    assert_eq!(result.output(owner()).navigation, None, "the catching finger stops the glide");
    let output = bench.output(owning_end(2, catch, catch), now + 2.0 * FRAME_S);
    assert_eq!(output.tap_at, None, "stopping a glide is not a tap");
    assert_eq!(output.navigation, None);
    // The next lone tap is a tap again.
    let (_, _) = bench.step(owning_start(3, catch), now + 0.5);
    assert_eq!(bench.output(owning_end(3, catch, catch), now + 0.55).tap_at, Some(catch));

    // A touch outside the canvas (a panel) leaves the glide running.
    let (mut bench, now) = gliding_bench();
    let outside = pos2(600.0, 100.0);
    let output = bench.output(owning_start(2, outside), now + FRAME_S);
    assert!(output.navigation.is_some(), "an unclaimed touch does not stop the glide");
}

// ---------------------------------------------------------------------------------------
// Tap
// ---------------------------------------------------------------------------------------

#[test]
fn short_still_lone_touch_is_a_tap_at_its_press_point() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(owning_move(1, pos2(103.0, 102.0)), 0.05);
    let output = bench.output(owning_end(1, pos2(104.0, 102.0), pos2(103.0, 102.0)), 0.1);
    assert_eq!(output.tap_at, Some(start));
    assert_eq!(output.navigation, None);
    assert_eq!(output.long_press_at, None);
    // Reported on the lift frame only.
    assert_eq!(bench.output(Vec::new(), 0.15), TouchOutput::default());
    // A whole tap within one frame counts too.
    let output = bench.output(concat(vec![owning_start(2, start), owning_end(2, start, start)]), 0.2);
    assert_eq!(output.tap_at, Some(start));
}

#[test]
fn pans_long_presses_and_interrupted_touches_are_not_taps() {
    let start = pos2(100.0, 100.0);
    // Moved beyond the slop and back.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(owning_move(1, pos2(120.0, 100.0)), 0.05);
    assert_eq!(bench.output(owning_end(1, start, start), 0.1).tap_at, None, "pan");
    // Lifted beyond the slop in the lift event itself.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    assert_eq!(bench.output(owning_end(1, pos2(110.0, 100.0), start), 0.05).tap_at, None, "lift beyond the slop");
    // Armed: the menu, not a tap.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(Vec::new(), LONG_PRESS_ARM_S);
    let output = bench.output(owning_end(1, start, start), LONG_PRESS_ARM_S + 0.1);
    assert_eq!((output.tap_at, output.long_press_at), (None, Some(start)), "long-press");
    // Held past the arm time with no frame in between: neither.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    assert_eq!(bench.output(owning_end(1, start, start), LONG_PRESS_ARM_S + 0.1).tap_at, None, "late lift");
    // Cancelled by the platform.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    assert_eq!(bench.output(owning_cancel(1, start), 0.05).tap_at, None, "cancel");
    // A second finger.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(bare(2, TouchPhase::Start, pos2(200.0, 100.0)), 0.02);
    let output = bench.output(concat(vec![owning_end(1, start, start), bare(2, TouchPhase::End, pos2(200.0, 100.0))]), 0.05);
    assert_eq!(output.tap_at, None, "two fingers");
    // A pen held while the finger taps (palm).
    let mut bench = Bench::new();
    let (_, _) = bench.step(vec![button(pos2(380.0, 20.0), PointerButton::Primary, true)], 0.0);
    let (_, _) = bench.step(bare(1, TouchPhase::Start, start), 0.01);
    assert_eq!(bench.output(bare(1, TouchPhase::End, start), 0.05).tap_at, None, "palm");
    // A pen pressed during the touch.
    let mut bench = Bench::new();
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    let (_, _) = bench.step(vec![button(pos2(380.0, 20.0), PointerButton::Primary, true)], 0.02);
    assert_eq!(bench.output(owning_end(1, start, start), 0.05).tap_at, None, "pen press during the touch");
}

// ---------------------------------------------------------------------------------------
// Live on/off (`set_enabled`)
// ---------------------------------------------------------------------------------------

#[test]
fn off_request_waits_until_the_claimed_gesture_ends() {
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    bench.router.set_enabled(false);
    assert_eq!(bench.router.apply_requested(), None, "a live claim keeps the router on");
    assert!(bench.router.is_enabled());
    // The claimed touch's emulated release is still stripped under the old rules.
    let (out, _) = bench.step(owning_end(1, start, start), 0.1);
    assert!(only_touch_events(&out), "the release of a stripped press must stay stripped: {out:?}");
    assert_eq!(bench.router.apply_requested(), Some(false));
    assert!(!bench.router.is_enabled());
    assert_eq!(bench.router.apply_requested(), None, "an applied request is not re-applied");
}

#[test]
fn off_request_waits_for_a_stripped_promoted_release() {
    let mut bench = Bench::new();
    let finger = pos2(100.0, 100.0);
    let (_, _) = bench.step(bare(1, TouchPhase::Start, finger), 0.0);
    let promoted = pos2(101.0, 100.0);
    let (_, _) = bench.step(vec![button(promoted, PointerButton::Primary, true)], 0.01);
    let (_, _) = bench.step(bare(1, TouchPhase::End, finger), 0.02);
    bench.router.set_enabled(false);
    assert_eq!(bench.router.apply_requested(), None, "a stripped press still waits for its release");
    let (out, _) = bench.step(vec![button(pos2(300.0, 10.0), PointerButton::Primary, false)], 0.03);
    assert!(out.is_empty());
    assert_eq!(bench.router.apply_requested(), Some(false));
}

#[test]
fn on_request_applies_at_once_and_a_cancelled_off_request_keeps_the_gesture() {
    let mut router = TouchRouter::new(false);
    router.set_enabled(false);
    assert_eq!(router.apply_requested(), None);
    router.set_enabled(true);
    assert_eq!(router.apply_requested(), Some(true));
    assert!(router.is_enabled());

    // Off then on again before the gesture ended: nothing changes, the claim survives.
    let mut bench = Bench::new();
    let start = pos2(100.0, 100.0);
    let (_, _) = bench.step(owning_start(1, start), 0.0);
    bench.router.set_enabled(false);
    assert_eq!(bench.router.apply_requested(), None);
    bench.router.set_enabled(true);
    assert_eq!(bench.router.apply_requested(), None);
    let (out, _) = bench.step(owning_end(1, start, start), 0.1);
    assert!(only_touch_events(&out));
}

// ---------------------------------------------------------------------------------------
// Headless egui: adapter, layer fact, menu helper
// ---------------------------------------------------------------------------------------

const SCREEN: Rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(800.0, 600.0));
const FLOATING_AT: Pos2 = Pos2::new(500.0, 50.0);

/// Which context-menu builder the frame uses.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Menu {
    /// egui's own `Popup::context_menu` (the characterization reference).
    Egui,
    /// `context_menu_popup`, fed with the router's output of the frame.
    Ours,
}

/// What one headless frame observed.
#[derive(Clone, Copy, Debug, Default)]
struct Frame {
    canvas_clicked: bool,
    floating_rect: Option<Rect>,
    popup_id: Option<Id>,
    item_rect: Option<Rect>,
    primary_down: bool,
    taken: TouchOutput,
}

/// Runs one frame: optional routing, then a canvas (root background layer, publishes its
/// surface, takes the router output, draws its context menu) and a Foreground area.
fn run_frame(ctx: &Context, router: Option<&mut TouchRouter>, events: Vec<Event>, time: f64, menu: Menu) -> Frame {
    let mut raw = RawInput { screen_rect: Some(SCREEN), time: Some(time), events, ..RawInput::default() };
    if let Some(router) = router {
        router.route(ctx, &mut raw);
    }
    let mut frame = Frame::default();
    ctx.run_ui(raw, |ui: &mut Ui| {
        let response = ui.allocate_rect(canvas_rect(), Sense::click());
        publish_surface(ui.ctx(), surface(true));
        frame.taken = take_touch_output(ui.ctx(), owner());
        frame.canvas_clicked = response.clicked();
        frame.popup_id = Some(Popup::default_response_id(&response));
        let popup = match menu {
            Menu::Egui => Popup::context_menu(&response),
            Menu::Ours => context_menu_popup(&response, frame.taken.long_press_at),
        };
        if let Some(inner) = popup.show(|ui| ui.button("item").rect) {
            frame.item_rect = Some(inner.inner);
        }
        let floating = egui::Area::new(Id::new("touch_nav.test.floating"))
            .order(Order::Foreground)
            .fixed_pos(FLOATING_AT)
            .show(ui.ctx(), |ui| ui.label("panel"));
        frame.floating_rect = Some(floating.response.rect);
        frame.primary_down = ui.input(|input| input.pointer.primary_down());
    })
    .drop_without_applying_deltas();
    frame
}

/// Runs two empty frames and returns the floating area's settled rect. The first frame lays
/// the area out before the screen size is known, so its rect is not where it ends up.
fn warm_up(ctx: &Context, router: Option<&mut TouchRouter>) -> Rect {
    let mut router = router;
    let _first = run_frame(ctx, router.as_deref_mut(), Vec::new(), 0.0, Menu::Ours);
    let settled = run_frame(ctx, router, Vec::new(), 0.02, Menu::Ours);
    let floating = settled.floating_rect.expect("floating area drawn");
    assert!(!canvas_rect().intersects(floating), "the floating area must lie outside the canvas: {floating:?}");
    floating
}

fn popup_rect(ctx: &Context, frame: &Frame) -> Option<Rect> {
    let id = frame.popup_id?;
    ctx.memory(|memory| memory.area_rect(id))
}

#[test]
fn layer_id_at_over_bare_canvas_is_the_background_layer() {
    // Pins the fact the claim rule relies on (the plan left "None or Background" open): the
    // root ui's background layer IS reported by `layer_id_at` over bare canvas, and a
    // Foreground area is reported as itself.
    let ctx = Context::default();
    let floating = warm_up(&ctx, None);
    assert_eq!(ctx.layer_id_at(pos2(100.0, 100.0)), Some(LayerId::background()));
    assert_eq!(ctx.layer_id_at(floating.center()).map(|layer| layer.order), Some(Order::Foreground));
}

#[test]
fn claimed_touch_never_presses_while_a_floating_area_keeps_emulation() {
    let ctx = Context::default();
    let mut router = TouchRouter::new(true);
    let floating = warm_up(&ctx, Some(&mut router));

    let on_canvas = pos2(100.0, 100.0);
    let frame = run_frame(&ctx, Some(&mut router), owning_start(1, on_canvas), 0.1, Menu::Ours);
    assert!(!frame.primary_down, "a claimed finger must not press the primary button");
    let frame = run_frame(&ctx, Some(&mut router), owning_end(1, on_canvas, on_canvas), 0.2, Menu::Ours);
    assert!(!frame.canvas_clicked, "a claimed tap must not click the canvas");
    assert!(!frame.primary_down);

    let on_panel = floating.center();
    let frame = run_frame(&ctx, Some(&mut router), owning_start(2, on_panel), 0.3, Menu::Ours);
    assert!(frame.primary_down, "a finger over a Foreground area keeps egui-winit's emulation");
    let frame = run_frame(&ctx, Some(&mut router), owning_end(2, on_panel, on_panel), 0.4, Menu::Ours);
    assert!(!frame.primary_down);
}

#[test]
fn disabled_router_and_child_viewports_leave_input_untouched() {
    let ctx = Context::default();
    let frame = run_frame(&ctx, None, Vec::new(), 0.0, Menu::Ours);
    assert!(frame.popup_id.is_some());
    let input = owning_start(1, pos2(100.0, 100.0));

    let mut disabled = TouchRouter::new(false);
    let mut raw = RawInput { time: Some(0.1), events: input.clone(), ..RawInput::default() };
    disabled.route(&ctx, &mut raw);
    assert_eq!(raw.events, input);

    let mut enabled = TouchRouter::new(true);
    prime(&ctx, &mut enabled);
    publish_surface(&ctx, surface(true));
    let mut raw = RawInput { viewport_id: ViewportId::from_hash_of("child"), time: Some(0.1), events: input.clone(), ..RawInput::default() };
    enabled.route(&ctx, &mut raw);
    assert_eq!(raw.events, input, "a child viewport's input is never routed");
    // The child call did not freeze the root's publications: the next ROOT call claims.
    let mut raw = RawInput { time: Some(0.2), events: input, ..RawInput::default() };
    enabled.route(&ctx, &mut raw);
    assert!(only_touch_events(&raw.events));
}

#[test]
fn live_toggle_through_route_switches_routing_and_publishing() {
    let ctx = Context::default();
    let at = pos2(100.0, 100.0);
    let mut router = TouchRouter::new(true);
    prime(&ctx, &mut router);
    publish_surface(&ctx, surface(true));
    let mut raw = RawInput { time: Some(0.1), events: owning_start(1, at), ..RawInput::default() };
    router.route(&ctx, &mut raw);
    assert!(only_touch_events(&raw.events), "claimed while on");

    // Off while the finger is down: the gesture finishes stripped, then the router turns off.
    router.set_enabled(false);
    publish_surface(&ctx, surface(true));
    let mut raw = RawInput { time: Some(0.2), events: owning_end(1, at, at), ..RawInput::default() };
    router.route(&ctx, &mut raw);
    assert!(only_touch_events(&raw.events), "the claimed release stays stripped");
    assert!(router.is_enabled());
    publish_surface(&ctx, surface(true));
    let input = owning_start(2, at);
    let mut raw = RawInput { time: Some(0.3), events: input.clone(), ..RawInput::default() };
    router.route(&ctx, &mut raw);
    assert!(!router.is_enabled());
    assert_eq!(raw.events, input, "an off router leaves a canvas touch to egui");
    publish_all(&ctx);
    assert_eq!(stored_publications(&ctx), (0, 0, 0), "publishing is off with the router");
    let mut raw = RawInput { time: Some(0.4), events: owning_end(2, at, at), ..RawInput::default() };
    router.route(&ctx, &mut raw);
    assert_eq!(raw.events.len(), 3, "the unclaimed release reaches egui");

    // On again: the next routing call applies it; the frame after claims.
    router.set_enabled(true);
    prime(&ctx, &mut router);
    assert!(router.is_enabled());
    publish_surface(&ctx, surface(true));
    let mut raw = RawInput { time: Some(0.5), events: owning_start(3, at), ..RawInput::default() };
    router.route(&ctx, &mut raw);
    assert!(only_touch_events(&raw.events), "claimed again once back on");
}

/// One empty ROOT routing call: what the shell's hook does before the first pass, so the
/// publications of the pass that follows are kept for the router.
fn prime(ctx: &Context, router: &mut TouchRouter) {
    let mut raw = RawInput::default();
    router.route(ctx, &mut raw);
}

/// Number of entries of each publication list currently stored in `ctx`.
fn stored_publications(ctx: &Context) -> (usize, usize, usize) {
    ctx.data(|data| {
        (
            data.get_temp::<Vec<TouchSurface>>(super::surfaces_id()).map_or(0, |list| list.len()),
            data.get_temp::<Vec<(Id, LayerId)>>(super::capture_layers_id()).map_or(0, |list| list.len()),
            data.get_temp::<Vec<(Id, Rect)>>(super::holes_id()).map_or(0, |list| list.len()),
        )
    })
}

fn publish_all(ctx: &Context) {
    publish_surface(ctx, surface(true));
    publish_capture_layer(ctx, owner(), LayerId::new(Order::Middle, Id::new("touch_nav.test.capture")));
    publish_hole(ctx, owner(), hole_rect());
}

#[test]
fn publications_are_dropped_unless_a_live_router_drains_them() {
    // No router at all (the wasm entry): every frame's publications would otherwise pile up in
    // `ctx` data forever — `publish_hole` appends without dedup.
    let ctx = Context::default();
    for _ in 0..3 {
        publish_all(&ctx);
    }
    assert_eq!(stored_publications(&ctx), (0, 0, 0), "nothing is stored without a router");

    // A disabled router (gestures unavailable) does not make them live either.
    let mut disabled = TouchRouter::new(false);
    prime(&ctx, &mut disabled);
    publish_all(&ctx);
    assert_eq!(stored_publications(&ctx), (0, 0, 0));

    // An enabled router makes them live, and its next call drains them.
    let mut enabled = TouchRouter::new(true);
    prime(&ctx, &mut enabled);
    publish_all(&ctx);
    publish_hole(&ctx, owner(), hole_rect());
    assert_eq!(stored_publications(&ctx), (1, 1, 2));
    prime(&ctx, &mut enabled);
    assert_eq!(stored_publications(&ctx), (0, 0, 0), "the routing call freezes and drains them");

    // Disabling turns publishing off again (the shell builds one router per window, but the
    // flag must follow the router that runs, not the first one ever seen).
    prime(&ctx, &mut disabled);
    publish_all(&ctx);
    assert_eq!(stored_publications(&ctx), (0, 0, 0));
}

#[test]
fn capture_layer_publication_reaches_the_router() {
    let ctx = Context::default();
    let mut router = TouchRouter::new(true);
    let floating = warm_up(&ctx, Some(&mut router));
    // Make the canvas cover the floating area's point: without a capture registration the
    // Foreground area blocks the claim ...
    let wide = TouchSurface { rect: SCREEN, ..surface(true) };
    publish_surface(&ctx, wide.clone());
    let blocked = owning_start(1, floating.center());
    let mut raw = RawInput { time: Some(0.05), events: blocked.clone(), ..RawInput::default() };
    router.route(&ctx, &mut raw);
    assert_eq!(raw.events, blocked);
    let mut raw = RawInput { time: Some(0.06), events: owning_end(1, floating.center(), floating.center()), ..RawInput::default() };
    router.route(&ctx, &mut raw);
    // ... and with it the touch is claimed.
    publish_surface(&ctx, wide);
    publish_capture_layer(&ctx, owner(), LayerId::new(Order::Foreground, Id::new("touch_nav.test.floating")));
    let mut raw = RawInput { time: Some(0.1), events: owning_start(2, floating.center()), ..RawInput::default() };
    router.route(&ctx, &mut raw);
    assert!(only_touch_events(&raw.events), "a touch under the owner's capture layer is claimed");
}

#[test]
fn published_hole_of_the_owner_is_not_claimed() {
    let ctx = Context::default();
    let mut router = TouchRouter::new(true);
    let late_hole = Rect::from_min_size(pos2(50.0, 50.0), vec2(40.0, 40.0));
    prime(&ctx, &mut router);
    publish_surface(&ctx, surface(true));
    publish_hole(&ctx, owner(), late_hole);
    // A hole of another owner (no surface of its own) is dropped and exempts nothing.
    publish_hole(&ctx, Id::new("touch_nav.test.other"), Rect::from_min_size(pos2(150.0, 150.0), vec2(40.0, 40.0)));
    let in_hole = owning_start(1, late_hole.center());
    let mut raw = RawInput { time: Some(0.1), events: in_hole.clone(), ..RawInput::default() };
    router.route(&ctx, &mut raw);
    assert_eq!(raw.events, in_hole, "a touch in a late-published hole keeps egui's emulation");
    let mut raw = RawInput { time: Some(0.12), events: owning_end(1, late_hole.center(), late_hole.center()), ..RawInput::default() };
    router.route(&ctx, &mut raw);

    // The hole lives one frame like every publication: published again, it still exempts;
    // the other owner's hole never did.
    publish_surface(&ctx, surface(true));
    publish_hole(&ctx, owner(), late_hole);
    let mut raw = RawInput { time: Some(0.2), events: owning_start(2, pos2(170.0, 170.0)), ..RawInput::default() };
    router.route(&ctx, &mut raw);
    assert!(only_touch_events(&raw.events), "a foreign owner's hole must not exempt this surface");
}

#[test]
fn bare_layer_painter_does_not_block_a_claim() {
    // A tool outline painted through `ctx.layer_painter` on a higher order (the patch tool's
    // `Order::Middle` outline) registers no `Area`, so `layer_id_at` never reports it and a
    // finger over it is claimed like one over bare canvas — no capture registration needed.
    let ctx = Context::default();
    let mut router = TouchRouter::new(true);
    let _floating = warm_up(&ctx, Some(&mut router));
    let over_outline = pos2(120.0, 120.0);
    let raw = RawInput { screen_rect: Some(SCREEN), time: Some(0.05), ..RawInput::default() };
    ctx.run_ui(raw, |ui: &mut Ui| {
        let _canvas = ui.allocate_rect(canvas_rect(), Sense::click());
        publish_surface(ui.ctx(), surface(true));
        let painter = ui.ctx().layer_painter(LayerId::new(Order::Middle, Id::new("touch_nav.test.outline")));
        painter.rect_filled(Rect::from_center_size(over_outline, vec2(80.0, 80.0)), 0.0, egui::Color32::WHITE);
    })
    .drop_without_applying_deltas();
    assert_eq!(ctx.layer_id_at(over_outline), Some(LayerId::background()));
    let mut raw = RawInput { time: Some(0.1), events: owning_start(1, over_outline), ..RawInput::default() };
    router.route(&ctx, &mut raw);
    assert!(only_touch_events(&raw.events), "a finger over a bare layer painter is claimed");
}

/// Opens the canvas menu with a mouse secondary click at `at` and returns the menu rect
/// once it settled (after the sizing pass).
/// `time` is the first frame's time; the helper uses `time .. time + 0.2`.
fn open_by_secondary_click(ctx: &Context, at: Pos2, menu: Menu, time: f64) -> (Frame, Rect) {
    let _first = run_frame(ctx, None, Vec::new(), time, menu);
    let _press = run_frame(ctx, None, vec![Event::PointerMoved(at), button(at, PointerButton::Secondary, true)], time + 0.05, menu);
    let opened = run_frame(ctx, None, vec![button(at, PointerButton::Secondary, false)], time + 0.1, menu);
    assert!(opened.popup_id.is_some_and(|id| Popup::is_id_open(ctx, id)), "{menu:?}: secondary click opens");
    let settled = run_frame(ctx, None, Vec::new(), time + 0.2, menu);
    let rect = popup_rect(ctx, &settled).expect("menu area exists");
    (settled, rect)
}

fn click(ctx: &Context, at: Pos2, time: f64, menu: Menu) -> Frame {
    let _press = run_frame(ctx, None, vec![Event::PointerMoved(at), button(at, PointerButton::Primary, true)], time, menu);
    run_frame(ctx, None, vec![button(at, PointerButton::Primary, false)], time + 0.05, menu)
}

#[test]
fn context_menu_popup_matches_egui_context_menu_for_the_mouse() {
    let at = pos2(120.0, 140.0);
    let egui_ctx = Context::default();
    let ours_ctx = Context::default();
    let (egui_frame, egui_rect) = open_by_secondary_click(&egui_ctx, at, Menu::Egui, 0.0);
    let (ours_frame, ours_rect) = open_by_secondary_click(&ours_ctx, at, Menu::Ours, 0.0);
    assert_eq!(ours_rect, egui_rect, "same anchor, same placement");
    assert!(egui_rect.min.distance(at) < 20.0, "anchored at the click point: {egui_rect:?}");

    // A click on a menu item closes both.
    let egui_item = egui_frame.item_rect.expect("egui menu item");
    let ours_item = ours_frame.item_rect.expect("our menu item");
    let egui_after = click(&egui_ctx, egui_item.center(), 0.3, Menu::Egui);
    let ours_after = click(&ours_ctx, ours_item.center(), 0.3, Menu::Ours);
    assert!(!egui_after.popup_id.is_some_and(|id| Popup::is_id_open(&egui_ctx, id)));
    assert!(!ours_after.popup_id.is_some_and(|id| Popup::is_id_open(&ours_ctx, id)));

    // Re-open, then a primary click on the canvas itself closes both.
    let elsewhere = pos2(350.0, 30.0);
    for (ctx, menu) in [(&egui_ctx, Menu::Egui), (&ours_ctx, Menu::Ours)] {
        let (_, _) = open_by_secondary_click(ctx, at, menu, 1.0);
        let after = click(ctx, elsewhere, 1.5, menu);
        assert!(after.canvas_clicked, "{menu:?}: the click lands on the canvas");
        assert!(!after.popup_id.is_some_and(|id| Popup::is_id_open(ctx, id)), "{menu:?}: canvas click closes");
    }
}

#[test]
fn long_press_opens_the_menu_at_the_press_point_without_a_pointer() {
    let at = pos2(120.0, 140.0);
    let reference_ctx = Context::default();
    let (_, reference_rect) = open_by_secondary_click(&reference_ctx, at, Menu::Egui, 0.0);

    let ctx = Context::default();
    let mut router = TouchRouter::new(true);
    let _first = run_frame(&ctx, Some(&mut router), Vec::new(), 0.0, Menu::Ours);
    let _down = run_frame(&ctx, Some(&mut router), owning_start(1, at), 0.1, Menu::Ours);
    let armed = run_frame(&ctx, Some(&mut router), Vec::new(), 0.1 + LONG_PRESS_ARM_S, Menu::Ours);
    assert_eq!(armed.taken.long_press_armed_at, Some(at));
    let lifted = run_frame(&ctx, Some(&mut router), owning_end(1, at, at), 0.2 + LONG_PRESS_ARM_S, Menu::Ours);
    assert_eq!(lifted.taken.long_press_at, Some(at));
    assert_eq!(ctx.pointer_hover_pos(), None, "the claimed finger never produced a pointer");
    assert!(lifted.popup_id.is_some_and(|id| Popup::is_id_open(&ctx, id)), "the long-press opens the menu");
    let settled = run_frame(&ctx, Some(&mut router), Vec::new(), 0.3 + LONG_PRESS_ARM_S, Menu::Ours);
    assert!(settled.popup_id.is_some_and(|id| Popup::is_id_open(&ctx, id)), "it stays open after the lift");
    assert_eq!(popup_rect(&ctx, &settled), Some(reference_rect), "same place a secondary click there opens it");

    // A new finger on the canvas closes it at claim time, before the frame runs.
    let mut raw = RawInput { screen_rect: Some(SCREEN), time: Some(0.4 + LONG_PRESS_ARM_S), events: owning_start(2, pos2(350.0, 30.0)), ..RawInput::default() };
    publish_surface(&ctx, surface(true));
    router.route(&ctx, &mut raw);
    assert!(!Popup::is_any_open(&ctx), "a claim start dismisses the open menu");
}
