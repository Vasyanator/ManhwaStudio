/*
File: crates/ms-widgets/src/touch_nav/router.rs

Purpose:
The input-boundary router of canvas touch gestures. Runs on the ROOT viewport's
`RawInput` from the shell's `eframe::App::raw_input_hook`, BEFORE egui builds its
pointer state, and decides per touch whether a canvas claims it.

Main responsibilities:
- claim a touch on `Touch{Start}` when it starts on a surface published last frame,
  outside its holes, with nothing but the background (or one of that owner's capture
  layers) on top;
- strip egui-winit's emulated mouse events of a claimed touch (they sit right after
  its `Touch` event), plus platform-promoted copies near it (proximity rule);
- run the per-touch gesture state machine (pending long-press, pan, armed, selection,
  spent) and compute pan / pinch per owner;
- hand the result to `mod.rs` for take-once delivery.

Key structures:
- `TouchRouter`: claims keyed by `(TouchDeviceId, TouchId)`, mouse/pen buttons held,
  promoted presses stripped, recently ended claim positions.
- `RouterConfig`, `RouteResult`, `OwnerResult`: the pure core's input and output.

Key functions:
- `TouchRouter::route` (egui adapter: viewport check, pending on/off request, freeze,
  thresholds, dismiss, store deliveries);
- `TouchRouter::set_enabled` (live on/off request, applied at a safe frame start);
- `TouchRouter::route_events` (pure core over an event list, unit-tested).

Notes:
Pairing invariant: a button release is removed only when its press never reached egui
(the press was stripped, or the touch never emitted one), so the router can never leave
a button stuck down. A claimed touch never passes a press, so all of its emulated
releases go; a promoted press and its release go together.

egui-winit emulation per touch (`crates/egui/crates/egui-winit/src/lib.rs`, `on_touch`):
the `Touch` event first, then — only for the touch that owns the emulated pointer —
Start: `PointerMoved` + `PointerButton{Primary, pressed}`; Move: `PointerMoved`;
End: `PointerButton{released}` + `PointerGone`; Cancel: `PointerGone`. Move/press
carry the touch position; the release carries the last MOVE position, so it is matched
by variant and button only — and therefore only after the End of the touch whose Start
was followed by the emulated press (`TouchRouter::emulating`); after any other touch an
adjacent release is a real pen/mouse release.
*/

use std::collections::BTreeMap;

use egui::{
    Context, Event, Id, LayerId, Order, PointerButton, Popup, Pos2, RawInput, TouchDeviceId,
    TouchId, TouchPhase, Vec2, ViewportId,
};
use ms_log::trace::cat;

use super::{
    Delivery, LONG_PRESS_ARM_S, PROMOTION_GRACE_S, PROMOTION_RADIUS_PT, TouchNavigation,
    TouchOutput, TouchSelection, TouchSelectionEnd, TouchSurface, freeze_publications,
    set_router_live, store_deliveries,
};

/// Identity of one touch: egui's device hash plus the platform's touch id.
type TouchKey = (TouchDeviceId, TouchId);

/// How far an emulated event's position may differ from its `Touch` event's position.
/// egui-winit computes both from the same `location / pixels_per_point` expression, so the
/// real difference is zero; the tolerance only absorbs float noise.
const ADJACENT_POS_EPS_PT: f32 = 0.01;

/// Below this mean spread (points) the previous touch layout is too degenerate to divide
/// by, and the pinch reports no zoom for that frame.
const MIN_PINCH_SPREAD_PT: f32 = 1.0;

/// Thresholds of one routing call.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct RouterConfig {
    /// egui's click distance (`InputOptions::max_click_dist`, points): the long-press slop.
    pub(super) slop_pt: f32,
    /// Hold time before a long-press arms, seconds (`LONG_PRESS_ARM_S` in production).
    pub(super) long_press_s: f64,
}

/// One owner's result of a routing call.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct OwnerResult {
    pub(super) owner: Id,
    pub(super) output: TouchOutput,
    /// Absolute time (same clock as `now`) at which a pending long-press of this owner arms.
    pub(super) deadline: Option<f64>,
}

/// Result of [`TouchRouter::route_events`].
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct RouteResult {
    /// One entry per owner that has an output or a pending deadline this frame.
    pub(super) owners: Vec<OwnerResult>,
    /// A touch was claimed this frame: close any open popup (a finger on the canvas does what
    /// a mouse click outside a menu does, without the click).
    pub(super) dismiss_popups: bool,
}

impl RouteResult {
    /// Earliest pending long-press deadline over all owners.
    #[cfg(test)]
    pub(super) fn next_deadline(&self) -> Option<f64> {
        self.owners.iter().filter_map(|owner| owner.deadline).reduce(f64::min)
    }

    /// The output of `owner`, or the default when the owner has none this frame.
    #[cfg(test)]
    pub(super) fn output(&self, owner: Id) -> TouchOutput {
        self.owners.iter().find(|result| result.owner == owner).map(|result| result.output).unwrap_or_default()
    }
}

/// Gesture state of one claimed touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Sole claimed touch of its owner, within the slop, waiting for the long-press time.
    /// Does not navigate yet; on leaving this state it catches up from its start position.
    Pending,
    /// Navigates (pan, and pinch together with the owner's other `Pan` touches).
    Pan,
    /// Long-press armed and still within the slop: lift = menu, move = selection.
    Held,
    /// Long-press selection drag in progress.
    Selecting,
    /// Interrupted after arming: never navigates or selects again; stripping continues.
    Spent,
}

impl Phase {
    /// Whether a later drag of this touch has yet to choose between selection and pan, so the
    /// owner's current `long_press_selection` still decides it.
    fn drag_mode_open(self) -> bool {
        match self {
            Phase::Pending | Phase::Held => true,
            Phase::Pan | Phase::Selecting | Phase::Spent => false,
        }
    }
}

/// One claimed touch.
#[derive(Clone, Copy, Debug)]
struct Claim {
    owner: Id,
    /// The owner's `TouchSurface::long_press_selection`, refreshed from every publication of the
    /// owner while the drag mode is open (`Phase::drag_mode_open`), so the value seen when the
    /// drag starts is last frame's; a frame without a publication keeps the previous value.
    selection_mode: bool,
    start_pos: Pos2,
    start_time: f64,
    /// Latest known position.
    pos: Pos2,
    /// Position navigation treats as "where this touch was at frame start"; `None` for a
    /// touch claimed this frame (it starts contributing next frame, so adding a finger
    /// never jumps). A `Pending` touch keeps its start position here.
    nav_prev: Option<Pos2>,
    phase: Phase,
}

/// Why a claim's long-press is interrupted.
#[derive(Clone, Copy, Debug)]
enum Interrupt {
    /// Another claimed finger of the same owner started (pinch wins / selection cancelled).
    SecondFinger,
    /// A mouse or pen button was pressed (palm rejection).
    PassthroughPress,
}

/// What a touch start is tested against: last frame's publications and egui's layer lookup.
struct ClaimScene<'a> {
    surfaces: &'a [TouchSurface],
    capture: &'a [(Id, LayerId)],
    layer_at: &'a dyn Fn(Pos2) -> Option<LayerId>,
}

/// Per-owner facts collected while the events of one call are processed.
#[derive(Clone, Copy, Debug, Default)]
struct OwnerFrame {
    long_press_at: Option<Pos2>,
    selection: Option<TouchSelection>,
}

/// Event-loop accumulator of one routing call.
#[derive(Debug, Default)]
struct FrameAcc {
    owners: Vec<(Id, OwnerFrame)>,
    dismiss_popups: bool,
}

impl FrameAcc {
    fn owner(&mut self, owner: Id) -> &mut OwnerFrame {
        let index = match self.owners.iter().position(|(id, _)| *id == owner) {
            Some(index) => index,
            None => {
                self.owners.push((owner, OwnerFrame::default()));
                self.owners.len() - 1
            }
        };
        &mut self.owners[index].1
    }

    /// Records the selection of `claim` at `pos`, keeping `began` of an entry made earlier in
    /// the same frame.
    fn selection(&mut self, claim: &Claim, pos: Pos2, began: bool, end: Option<TouchSelectionEnd>) {
        let frame = self.owner(claim.owner);
        let began = began || frame.selection.is_some_and(|selection| selection.began);
        frame.selection = Some(TouchSelection { origin: claim.start_pos, pos, began, end });
    }
}

/// The canvas touch router; one per main window, fed by its `raw_input_hook`.
#[derive(Debug)]
pub struct TouchRouter {
    enabled: bool,
    /// The state [`TouchRouter::set_enabled`] asked for; [`TouchRouter::apply_requested`]
    /// moves `enabled` onto it at a safe frame start.
    requested: bool,
    claims: BTreeMap<TouchKey, Claim>,
    /// Mouse / pen buttons whose press reached egui and whose release has not arrived yet.
    passthrough_down: Vec<PointerButton>,
    /// Buttons whose promoted press was stripped; their next release is stripped too.
    promoted_down: Vec<PointerButton>,
    /// End positions and times of claims, for the promotion grace period.
    recent_ends: Vec<(Pos2, f64)>,
    /// The touch (claimed or not) that owns egui-winit's emulated pointer: set at its Start
    /// when the adjacent emulated press is matched, cleared at its End/Cancel. Kept across a
    /// focus loss, like egui-winit's own `pointer_touch_id`.
    emulating: Option<TouchKey>,
}

impl TouchRouter {
    /// Creates a router. A disabled router never changes the input, and while it runs the
    /// `publish_*` calls are no-ops (it also drops anything published before it was disabled).
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            requested: enabled,
            claims: BTreeMap::new(),
            passthrough_down: Vec::new(),
            promoted_down: Vec::new(),
            recent_ends: Vec::new(),
            emulating: None,
        }
    }

    /// Requests the router on or off (the user's touchscreen-gestures setting), applied at the
    /// start of a later [`Self::route`] call without recreating the router.
    ///
    /// Turning on applies at the next routing call, from a fresh state (no claim, no tracked
    /// button: a touch or button already down then simply passes through, its release
    /// included). Turning off waits until no claim and no stripped promoted press is left, so
    /// the gesture in progress finishes under the old rules and every release whose press was
    /// stripped is still stripped (the pairing invariant: a disabled router strips nothing, so
    /// switching mid-gesture could hand egui an unpaired release). Idempotent; call it every
    /// frame with the live setting.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.requested = enabled;
    }

    /// Whether the router routes right now (an off request may still be waiting).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Applies a pending [`Self::set_enabled`] request when it is safe (see there); returns the
    /// new state when it changed. Either transition resets every gesture field.
    pub(super) fn apply_requested(&mut self) -> Option<bool> {
        if self.requested == self.enabled || (!self.requested && (!self.claims.is_empty() || !self.promoted_down.is_empty())) {
            return None;
        }
        *self = Self::new(self.requested);
        Some(self.enabled)
    }

    /// Routes one frame of raw input. Call it from `eframe::App::raw_input_hook`.
    ///
    /// Acts only on the ROOT viewport (the hook also runs for child viewports, whose input
    /// carries no canvas). Applies a pending on/off request ([`Self::set_enabled`]), freezes last frame's publications, marks publishing live (enabled
    /// router) or off (disabled), strips claimed touches' emulated mouse events from `raw.events`, closes open popups when a touch is claimed
    /// and stores this frame's per-owner outputs for [`super::take_touch_output`].
    ///
    /// Hook-time `Context` reads (`layer_id_at`, popup memory) use egui memory of
    /// `Memory::viewport_id`, which is ROOT between root passes as long as the app shows only
    /// immediate child viewports (it does: `embedded_window.rs`, `panel_dock/window.rs`).
    pub fn route(&mut self, ctx: &Context, raw: &mut RawInput) {
        if raw.viewport_id != ViewportId::ROOT {
            return;
        }
        if let Some(enabled) = self.apply_requested() {
            ms_log::runtime_log::log_info(format!("[touch_nav] canvas touch gestures {}", if enabled { "enabled" } else { "disabled" }));
            if !enabled {
                // A disabled router never stores deliveries again: drop the last ones so no
                // consumer can take a stale output later.
                store_deliveries(ctx, Vec::new());
            }
        }
        let (surfaces, capture) = freeze_publications(ctx);
        set_router_live(ctx, self.enabled);
        if !self.enabled {
            return;
        }
        let now = raw.time.unwrap_or_else(|| ctx.input(|input| input.time));
        let cfg = RouterConfig {
            slop_pt: ctx.options(|options| options.input_options.max_click_dist),
            long_press_s: LONG_PRESS_ARM_S,
        };
        let layer_at = |pos: Pos2| ctx.layer_id_at(pos);
        let result = self.route_events(&mut raw.events, now, cfg, &surfaces, &capture, &layer_at);
        if result.dismiss_popups && Popup::is_any_open(ctx) {
            Popup::close_all(ctx);
            ms_log::trace_log!(cat::INPUT, "touch_nav dismiss popups: canvas touch claimed");
        }
        let deliveries = result
            .owners
            .into_iter()
            .map(|owner| Delivery {
                owner: owner.owner,
                output: owner.output,
                repaint_after_s: owner.deadline.map(|deadline| (deadline - now).max(0.0)),
            })
            .collect();
        store_deliveries(ctx, deliveries);
    }

    /// Pure core: rewrites `events` (one frame, in order) and advances the gesture state.
    ///
    /// `now` is the frame time in seconds; `surfaces` / `capture` are last frame's
    /// publications; `layer_at` answers egui's `layer_id_at` for a point.
    pub(super) fn route_events(
        &mut self,
        events: &mut Vec<Event>,
        now: f64,
        cfg: RouterConfig,
        surfaces: &[TouchSurface],
        capture: &[(Id, LayerId)],
        layer_at: &dyn Fn(Pos2) -> Option<LayerId>,
    ) -> RouteResult {
        let scene = ClaimScene { surfaces, capture, layer_at };
        // A mode can end while the finger rests (Translation's modes are held keys): the owner
        // then publishes `long_press_selection == false` and the drag must pan, since the owner
        // would refuse to begin a selection. Read up to the drag's start, not only at claim.
        for claim in self.claims.values_mut().filter(|claim| claim.phase.drag_mode_open()) {
            if let Some(surface) = surfaces.iter().find(|surface| surface.owner == claim.owner) {
                claim.selection_mode = surface.long_press_selection;
            }
        }
        let mut acc = FrameAcc::default();
        let input = std::mem::take(events);
        events.reserve(input.len());
        let mut input = input.into_iter().peekable();
        while let Some(event) = input.next() {
            match event {
                Event::Touch { device_id, id, phase, pos, .. } => {
                    events.push(event);
                    let key = (device_id, id);
                    match phase {
                        TouchPhase::Start => {
                            let claimed = self.on_touch_start(key, pos, now, &scene, &mut acc);
                            if let Some(moved) = input.next_if(|next| matches!(next, Event::PointerMoved(at) if at.distance(pos) <= ADJACENT_POS_EPS_PT)) {
                                keep_unless(events, moved, claimed);
                            }
                            if let Some(press) = input.next_if(|next| matches!(next, Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, .. } if at.distance(pos) <= ADJACENT_POS_EPS_PT)) {
                                // egui-winit presses only for the touch that takes its emulated
                                // pointer, so this touch owns it until its End/Cancel.
                                self.emulating = Some(key);
                                keep_unless(events, press, claimed);
                            }
                        }
                        TouchPhase::Move => {
                            let claimed = self.on_touch_move(key, pos, cfg, &mut acc);
                            if let Some(moved) = input.next_if(|next| matches!(next, Event::PointerMoved(at) if at.distance(pos) <= ADJACENT_POS_EPS_PT)) {
                                keep_unless(events, moved, claimed);
                            }
                        }
                        TouchPhase::End | TouchPhase::Cancel => {
                            let cancelled = phase == TouchPhase::Cancel;
                            // Only the emulated pointer's owner is followed by emulated events. The
                            // release is matched by variant alone, so after any other touch an
                            // adjacent release / `PointerGone` is a real pen or mouse event: it is
                            // left to the loop below, which keeps the button bookkeeping (a stolen
                            // pen release would leave Primary stuck and the palm state forever).
                            if self.emulating == Some(key) {
                                self.emulating = None;
                                let claimed = self.claims.contains_key(&key);
                                if !cancelled
                                    && let Some(release) = input.next_if(|next| matches!(next, Event::PointerButton { button: PointerButton::Primary, pressed: false, .. }))
                                {
                                    keep_unless(events, release, claimed);
                                }
                                if let Some(gone) = input.next_if(|next| matches!(next, Event::PointerGone)) {
                                    keep_unless(events, gone, claimed);
                                }
                            }
                            self.on_touch_end(key, pos, now, cancelled, cfg, &mut acc);
                        }
                    }
                }
                Event::PointerButton { pos, button, pressed, .. } => {
                    if self.on_pointer_button(pos, button, pressed, now, &mut acc) {
                        events.push(event);
                    }
                }
                Event::PointerMoved(pos) => {
                    if !self.near_claim(pos, now) {
                        events.push(event);
                    }
                }
                Event::WindowFocused(false) => {
                    events.push(event);
                    self.on_focus_lost(&mut acc);
                }
                other => events.push(other),
            }
        }
        self.finish_frame(now, cfg, acc)
    }

    /// Claims the touch when it starts on a published surface; returns whether it did.
    fn on_touch_start(&mut self, key: TouchKey, pos: Pos2, now: f64, scene: &ClaimScene<'_>, acc: &mut FrameAcc) -> bool {
        // A repeated Start for a live key means its End was lost: forget the stale claim.
        self.claims.remove(&key);
        let Some(surface) = scene
            .surfaces
            .iter()
            .find(|surface| surface.rect.contains(pos) && !surface.holes.iter().any(|hole| hole.contains(pos)))
        else {
            return false;
        };
        // Bare canvas reads as the background layer (pinned by the test
        // `layer_id_at_over_bare_canvas_is_the_background_layer`; `None` is accepted too, for a
        // point no layer covers). Anything else on top is ordinary UI — including the top modal
        // layer `layer_id_at` reports while a modal is open (`egui-0.36.2/src/memory/mod.rs:838-845`)
        // — except the owner's own capture overlays.
        let on_top_ok = match (scene.layer_at)(pos) {
            None => true,
            Some(layer) => layer.order == Order::Background || scene.capture.contains(&(surface.owner, layer)),
        };
        if !on_top_ok {
            return false;
        }
        let owner = surface.owner;
        let mut alone = true;
        for claim in self.claims.values_mut().filter(|claim| claim.owner == owner) {
            alone = false;
            interrupt(claim, Interrupt::SecondFinger, acc);
        }
        let pending = alone && self.passthrough_down.is_empty();
        self.claims.insert(
            key,
            Claim {
                owner,
                selection_mode: surface.long_press_selection,
                start_pos: pos,
                start_time: now,
                pos,
                nav_prev: pending.then_some(pos),
                phase: if pending { Phase::Pending } else { Phase::Pan },
            },
        );
        acc.dismiss_popups = true;
        ms_log::trace_log!(cat::INPUT, "touch_nav claim owner={owner:?} pos={pos:?} pending={pending}");
        true
    }

    /// Advances a claimed touch to `pos`; returns whether the touch is claimed.
    fn on_touch_move(&mut self, key: TouchKey, pos: Pos2, cfg: RouterConfig, acc: &mut FrameAcc) -> bool {
        let Some(claim) = self.claims.get_mut(&key) else {
            return false;
        };
        claim.pos = pos;
        let beyond_slop = claim.start_pos.distance(pos) > cfg.slop_pt;
        match claim.phase {
            Phase::Pending => {
                if beyond_slop {
                    // `nav_prev` still holds the start position: the page catches up the slop.
                    claim.phase = Phase::Pan;
                }
            }
            Phase::Held => {
                if beyond_slop {
                    if claim.selection_mode {
                        claim.phase = Phase::Selecting;
                        acc.selection(claim, pos, true, None);
                        ms_log::trace_log!(cat::INPUT, "touch_nav selection start owner={:?} origin={:?}", claim.owner, claim.start_pos);
                    } else {
                        claim.phase = Phase::Pan;
                        claim.nav_prev = Some(claim.start_pos);
                    }
                }
            }
            Phase::Selecting => acc.selection(claim, pos, false, None),
            Phase::Pan | Phase::Spent => {}
        }
        true
    }

    /// Ends (lift) or cancels a touch; a lifted armed long-press requests the menu.
    fn on_touch_end(&mut self, key: TouchKey, pos: Pos2, now: f64, cancelled: bool, cfg: RouterConfig, acc: &mut FrameAcc) {
        let Some(claim) = self.claims.remove(&key) else {
            return;
        };
        match claim.phase {
            Phase::Held => {
                if !cancelled && claim.start_pos.distance(pos) <= cfg.slop_pt {
                    acc.owner(claim.owner).long_press_at = Some(claim.start_pos);
                    ms_log::trace_log!(cat::INPUT, "touch_nav long-press menu owner={:?} at={:?}", claim.owner, claim.start_pos);
                }
            }
            Phase::Selecting => {
                let end = if cancelled { TouchSelectionEnd::Cancelled } else { TouchSelectionEnd::Released };
                acc.selection(&claim, pos, false, Some(end));
                ms_log::trace_log!(cat::INPUT, "touch_nav selection end owner={:?} end={end:?}", claim.owner);
            }
            Phase::Pending | Phase::Pan | Phase::Spent => {}
        }
        self.recent_ends.push((pos, now));
        ms_log::trace_log!(cat::INPUT, "touch_nav release owner={:?} cancelled={cancelled}", claim.owner);
    }

    /// Handles a mouse-button event that is NOT a touch's emulation; returns whether egui
    /// should see it.
    fn on_pointer_button(&mut self, pos: Pos2, button: PointerButton, pressed: bool, now: f64, acc: &mut FrameAcc) -> bool {
        if pressed {
            if self.near_claim(pos, now) {
                self.promoted_down.push(button);
                return false;
            }
            if !self.passthrough_down.contains(&button) {
                self.passthrough_down.push(button);
            }
            for claim in self.claims.values_mut() {
                interrupt(claim, Interrupt::PassthroughPress, acc);
            }
            return true;
        }
        if let Some(index) = self.promoted_down.iter().position(|down| *down == button) {
            self.promoted_down.swap_remove(index);
            return false;
        }
        self.passthrough_down.retain(|down| *down != button);
        true
    }

    /// Whether `pos` is within the promotion radius of a live claim or a claim that ended
    /// within the grace period.
    fn near_claim(&self, pos: Pos2, now: f64) -> bool {
        self.claims.values().any(|claim| claim.pos.distance(pos) <= PROMOTION_RADIUS_PT)
            || self
                .recent_ends
                .iter()
                .any(|(end, time)| now - time <= PROMOTION_GRACE_S && end.distance(pos) <= PROMOTION_RADIUS_PT)
    }

    /// The window lost focus: releases may never arrive, so every claim and every tracked
    /// button is dropped (a running selection is cancelled). Nothing can stick: claimed
    /// presses never reached egui.
    fn on_focus_lost(&mut self, acc: &mut FrameAcc) {
        for claim in self.claims.values() {
            if claim.phase == Phase::Selecting {
                acc.selection(claim, claim.pos, false, Some(TouchSelectionEnd::Cancelled));
            }
        }
        if !self.claims.is_empty() {
            ms_log::trace_log!(cat::INPUT, "touch_nav focus lost: dropped {} claim(s)", self.claims.len());
        }
        self.claims.clear();
        self.passthrough_down.clear();
        self.promoted_down.clear();
    }

    /// Arms due long-presses, builds per-owner outputs and rolls navigation positions over.
    fn finish_frame(&mut self, now: f64, cfg: RouterConfig, acc: FrameAcc) -> RouteResult {
        self.recent_ends.retain(|(_, time)| now - time <= PROMOTION_GRACE_S);
        let palm = !self.passthrough_down.is_empty();
        for claim in self.claims.values_mut().filter(|claim| claim.phase == Phase::Pending) {
            if palm {
                claim.phase = Phase::Pan;
            } else if now - claim.start_time >= cfg.long_press_s {
                claim.phase = Phase::Held;
                ms_log::trace_log!(cat::INPUT, "touch_nav long-press armed owner={:?} at={:?}", claim.owner, claim.start_pos);
            }
        }

        let mut owners: Vec<Id> = acc.owners.iter().map(|(owner, _)| *owner).collect();
        for claim in self.claims.values() {
            if !owners.contains(&claim.owner) {
                owners.push(claim.owner);
            }
        }
        let mut result = RouteResult { owners: Vec::new(), dismiss_popups: acc.dismiss_popups };
        for owner in owners {
            let frame = acc.owners.iter().find(|(id, _)| *id == owner).map(|(_, frame)| *frame).unwrap_or_default();
            let mine = || self.claims.values().filter(move |claim| claim.owner == owner);
            let selection = frame.selection.or_else(|| {
                mine().find(|claim| claim.phase == Phase::Selecting).map(|claim| TouchSelection {
                    origin: claim.start_pos,
                    pos: claim.pos,
                    began: false,
                    end: None,
                })
            });
            let output = TouchOutput {
                navigation: if palm { None } else { navigation_of(mine()) },
                long_press_at: frame.long_press_at,
                long_press_armed_at: mine().find(|claim| claim.phase == Phase::Held).map(|claim| claim.start_pos),
                selection,
            };
            let deadline = mine()
                .filter(|claim| claim.phase == Phase::Pending)
                .map(|claim| claim.start_time + cfg.long_press_s)
                .reduce(f64::min);
            if output != TouchOutput::default() || deadline.is_some() {
                result.owners.push(OwnerResult { owner, output, deadline });
            }
        }

        // Navigating touches roll over to "frame start" for the next call; this also runs while
        // a palm suppresses navigation, so lifting the palm never jumps.
        for claim in self.claims.values_mut().filter(|claim| claim.phase == Phase::Pan) {
            claim.nav_prev = Some(claim.pos);
        }
        result
    }
}

/// Pushes `event` back into the output unless it belongs to a claimed touch.
fn keep_unless(events: &mut Vec<Event>, event: Event, claimed: bool) {
    if !claimed {
        events.push(event);
    }
}

/// Applies an interruption to one claim (see [`Interrupt`]).
fn interrupt(claim: &mut Claim, cause: Interrupt, acc: &mut FrameAcc) {
    match claim.phase {
        Phase::Pending => claim.phase = Phase::Pan,
        Phase::Held => {
            claim.phase = Phase::Spent;
            ms_log::trace_log!(cat::INPUT, "touch_nav long-press cancelled owner={:?} cause={cause:?}", claim.owner);
        }
        Phase::Selecting => {
            claim.phase = Phase::Spent;
            acc.selection(claim, claim.pos, false, Some(TouchSelectionEnd::Cancelled));
            ms_log::trace_log!(cat::INPUT, "touch_nav selection cancelled owner={:?} cause={cause:?}", claim.owner);
        }
        Phase::Pan | Phase::Spent => {}
    }
}

/// Pan / pinch over the navigating touches that existed at frame start and still exist.
/// `None` when nothing navigates or nothing moved.
fn navigation_of<'a>(claims: impl Iterator<Item = &'a Claim>) -> Option<TouchNavigation> {
    let pairs: Vec<(Pos2, Pos2)> = claims
        .filter(|claim| claim.phase == Phase::Pan)
        .filter_map(|claim| claim.nav_prev.map(|prev| (prev, claim.pos)))
        .collect();
    let count = pairs.len();
    if count == 0 {
        return None;
    }
    // Counted as f32 by accumulation: `usize as f32` would be a lossy cast.
    let weight = pairs.iter().fold(0.0_f32, |sum, _| sum + 1.0);
    let mean = |pick: fn(&(Pos2, Pos2)) -> Pos2| -> Pos2 {
        (pairs.iter().fold(Vec2::ZERO, |sum, pair| sum + pick(pair).to_vec2()) / weight).to_pos2()
    };
    let prev_centroid = mean(|pair| pair.0);
    let centroid = mean(|pair| pair.1);
    let zoom_factor = if count >= 2 {
        let prev_spread = pairs.iter().map(|pair| pair.0.distance(prev_centroid)).sum::<f32>() / weight;
        let spread = pairs.iter().map(|pair| pair.1.distance(centroid)).sum::<f32>() / weight;
        if prev_spread > MIN_PINCH_SPREAD_PT && spread.is_finite() { spread / prev_spread } else { 1.0 }
    } else {
        1.0
    };
    let navigation = TouchNavigation {
        prev_centroid,
        centroid,
        zoom_factor,
        touches: u8::try_from(count).unwrap_or(u8::MAX),
    };
    let unchanged = navigation.pan_delta() == Vec2::ZERO && (zoom_factor - 1.0).abs() <= f32::EPSILON;
    (!unchanged).then_some(navigation)
}
