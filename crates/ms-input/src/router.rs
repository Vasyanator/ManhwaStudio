/*
File: crates/ms-input/src/router.rs

Purpose:
The egui runtime of the input model: `InputRouter::begin_frame` turns one frame of
`egui::InputState` events into an `InputFrame` (fired commands, active modes, axis samples,
claimed canvas-wide gestures) through the pure matcher (`matching.rs`), and consumes exactly the
key and wheel events it matched. Also hosts binding capture (`capture.rs`) and the bridge from
the touch router's selection drags to touch binding alternatives.

Key structures:
- `InputRouter`: registry (shared with every `InputFrame` through an `Arc`), matcher state, the
  fed modifier roles, consumed keys, per-button press snapshots, claims, wheel accumulators,
  the capture session and the last frame.

Key functions:
- `InputRouter::begin_frame`: the once-per-frame dispatch, BEFORE any tab draws.
- `InputRouter::capture_begin` / `capture_set_mouse_zone` / `capture_status` /
  `capture_finish` / `capture_cancel`.

Frame contract (see also the crate `MODULE_README.md`, "Runtime"):
1. Freeze the view surfaces published during the previous frame (one-frame lag).
2. Read the frame's events and the activity (`text_edit_focused`, any popup open, pointer over a
   view: inside a surface, outside its holes, and the top layer there is the background or the
   surface's capture layer).
3. Feed every event, in order, to the matcher: `Event::Key` (physical key, logical fallback;
   modifier keys are not keys — their roles come from the events' `modifiers` and
   `ModifiersChanged`), pointer buttons, wheel (axes), `Zoom` (pinch axis). A matched key press
   (fired, or advancing a sequence) is consumed with every repeat and its release. Wheel
   ownership is per component: a wheel event is removed when axes own every non-zero component,
   and replaced by a copy with the owned components zeroed when they own only some. Pointer
   events are never removed (P2).
4. Window focus loss resets everything held, cancels claims and drops the forming capture chord.
5. Claims: a pointer-button press inside a surface that matches a `Drag` binding on a
   `canvas_wide` target becomes a claimed gesture of that view (Started, Active, Finished).
6. `FrameEnv::modal_open` or a running capture: the matcher only tracks held buttons (it is fed
   with a popup-open activity, under which the matcher contract fires no command and advances no
   sequence), nothing fires, no mode is active, nothing is claimed or resolved.

Notes:
- `ctx.input*` closures are kept tiny: events are copied out, decided on outside the lock (the
  layer lookups need `ctx.memory`), and removed by index in one `input_mut` afterwards.
- egui's wheel smoothing is private: per SMOOTHED axis (raw components mapped through egui's
  scroll-folding modifiers), a component of `smooth_scroll_delta` is zeroed while only owned wheel
  input feeds it — this frame and the following ones while egui still ramps it out
  (`is_scrolling`) — until wheel input egui keeps reaches that axis.
- View lookup: of the surfaces containing the pointer and passing the layer rule, one whose
  `capture_layer` is the top layer wins; otherwise the LATER-published one (a viewer drawn
  later, e.g. a window over a canvas, publishes later).
*/

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use egui::{Context, Event, Modifiers, MouseWheelUnit, Order, Popup, Pos2, Vec2};
use ms_log::trace::cat;

use crate::action::{ActionId, ActionSpec, ReleasePolicy, TargetId};
use crate::binding::{AxisSource, Binding, GestureKind};
use crate::button::{Button, ModifierKey, MouseButton, TouchSource};
use crate::capture::{CaptureError, CaptureSession, CaptureSlot, CaptureStatus, captured_alternatives};
use crate::context::ActiveSet;
use crate::frame::{
    AxisSample, ClaimedGesture, Fired, FrameEnv, GesturePhase, InputFrame, PressSnapshot, ViewFocus, ViewId, ViewSurface, mouse_button, take_view_surfaces,
};
use crate::matching::{GestureMatch, MatchEvent, MatchOutcome, MatchState};
use crate::registry::InputRegistry;
use crate::touch::{TouchSelection, TouchSelectionEnd, peek_selection};

/// Points of `MouseWheelUnit::Point` scrolling that make one wheel notch (step).
pub const WHEEL_NOTCH_PT: f32 = 40.0;

/// One running claimed gesture.
#[derive(Debug, Clone)]
struct Claim {
    matched: GestureMatch,
    view: ViewId,
    driver: ClaimDriver,
    origin: Pos2,
    current: Pos2,
    /// `matched.modes` are the mode references of the WINNING alternative: releasing one
    /// mid-gesture ends the gesture with that mode's `on_release_mid_gesture`.
    phase: GesturePhase,
}

/// What drives a claim: a mouse button held, or the touch router's selection drag of an owner.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ClaimDriver {
    Mouse(MouseButton),
    Touch(egui::Id),
}

impl ClaimDriver {
    fn button(self) -> Button {
        match self {
            Self::Mouse(button) => Button::Mouse(button),
            Self::Touch(_) => Button::Touch(TouchSource::LongPressDrag),
        }
    }
}

/// Per-frame accumulation of one axis.
#[derive(Debug, Clone, Copy)]
struct AxisAcc {
    delta: f32,
    steps: i32,
}

/// What the event pass collected.
#[derive(Debug, Default)]
struct EventPass {
    consumed: Vec<usize>,
    fired: Vec<(ActionId, Vec<ActionId>)>,
    axes: BTreeMap<ActionId, AxisAcc>,
    /// Wheel events whose owned components are zeroed in place (the rest stays egui's).
    replaced: Vec<(usize, Event)>,
    /// Per SMOOTHED axis (x, y): wheel input this frame that an axis owned / that egui keeps.
    wheel_owned: [bool; 2],
    wheel_unowned: [bool; 2],
    /// egui's scroll-folding modifiers (`InputOptions::{horizontal,vertical}_scroll_modifier`).
    scroll_fold: (Modifiers, Modifiers),
    focus_lost: bool,
}

/// The input runtime of one studio session. Owned by the studio root; `begin_frame` runs once per
/// frame before any tab draws.
#[derive(Debug)]
pub struct InputRouter {
    registry: Arc<InputRegistry>,
    state: MatchState,
    /// Modifier roles currently fed to the matcher.
    modifiers: BTreeSet<ModifierKey>,
    /// Held Super keys (the Super role on platforms whose `Modifiers` cannot report it).
    super_keys: BTreeSet<egui::Key>,
    /// Key buttons whose press was consumed: their repeats and release are consumed too.
    consumed_keys: BTreeSet<Button>,
    presses: BTreeMap<MouseButton, PressSnapshot>,
    /// Buttons released last frame; their snapshots are dropped at the next frame start.
    released: Vec<MouseButton>,
    claims: Vec<Claim>,
    /// `Point`-unit wheel remainders per axis action (sign included), in points.
    wheel_remainder: BTreeMap<ActionId, f32>,
    /// Per smoothed axis (x, y): a matched wheel's smoothing is still being suppressed.
    wheel_owned: [bool; 2],
    focused: bool,
    surfaces: Vec<ViewSurface>,
    capture: Option<CaptureSession>,
    frame: InputFrame,
}

impl InputRouter {
    /// A router over `registry`, with nothing held.
    #[must_use]
    pub fn new(registry: InputRegistry) -> Self {
        let registry = Arc::new(registry);
        Self {
            frame: InputFrame::empty(Arc::clone(&registry)),
            registry,
            state: MatchState::default(),
            modifiers: BTreeSet::new(),
            super_keys: BTreeSet::new(),
            consumed_keys: BTreeSet::new(),
            presses: BTreeMap::new(),
            released: Vec::new(),
            claims: Vec::new(),
            wheel_remainder: BTreeMap::new(),
            wheel_owned: [false; 2],
            focused: true,
            surfaces: Vec::new(),
            capture: None,
        }
    }

    /// The registry (current bindings, labels, conflicts).
    #[must_use]
    pub fn registry(&self) -> &InputRegistry {
        &self.registry
    }

    /// Mutable registry access for binding edits. The current `InputFrame` keeps the registry it
    /// was computed with; the change applies from the next `begin_frame`.
    pub fn registry_mut(&mut self) -> &mut InputRegistry {
        Arc::make_mut(&mut self.registry)
    }

    /// The last frame computed by [`Self::begin_frame`].
    #[must_use]
    pub fn frame(&self) -> &InputFrame {
        &self.frame
    }

    /// Processes this frame's input (see the file header for the contract) and returns the
    /// result. Call once per frame, before anything reads keys, the wheel or drags.
    pub fn begin_frame(&mut self, ctx: &Context, env: &FrameEnv) -> &InputFrame {
        self.surfaces = take_view_surfaces(ctx);
        self.claims.retain(|claim| !matches!(claim.phase, GesturePhase::Finished(_)));
        // Claims of earlier frames are Active now; this frame's presses add `Started` ones.
        for claim in &mut self.claims {
            claim.phase = GesturePhase::Active;
        }
        for button in std::mem::take(&mut self.released) {
            self.presses.remove(&button);
        }
        let (events, pointer, pointer_delta, focused, scrolling) =
            ctx.input(|input| (input.events.clone(), input.pointer.latest_pos(), input.pointer.delta(), input.focused, input.is_scrolling()));
        let view_under_pointer = pointer.and_then(|pos| self.surface_at(ctx, pos)).map(|surface| surface.id);
        let active = ActiveSet {
            active_tab: env.active_tab,
            contexts: env.contexts.iter().copied().collect(),
            text_edit_focused: ctx.text_edit_focused(),
            popup_open: Popup::is_any_open(ctx) || ctx.any_popup_open(),
            pointer_on_view: view_under_pointer.is_some(),
        };
        let suppressed = env.modal_open || self.capture.is_some();

        let input_options = ctx.options(|options| options.input_options);
        let mut pass = EventPass { scroll_fold: (input_options.horizontal_scroll_modifier, input_options.vertical_scroll_modifier), ..EventPass::default() };
        for (index, event) in events.iter().enumerate() {
            self.on_event(ctx, index, event, &active, suppressed, &mut pass);
        }
        if (self.focused && !focused) || pass.focus_lost {
            self.on_focus_lost();
        }
        self.focused = focused;
        if suppressed {
            self.cancel_claims("input suppressed");
        }

        self.update_claims(ctx, pointer, &active, suppressed);
        if !suppressed {
            self.drag_axes(pointer_delta, &active, &mut pass);
        }

        // Wheel smoothing, per smoothed axis (see the file header): an axis is suppressed while
        // only owned wheel input feeds it; any input egui keeps on that axis releases it.
        for axis in 0..2 {
            if pass.wheel_unowned[axis] || !scrolling {
                self.wheel_owned[axis] = false;
            }
            if pass.wheel_owned[axis] && !pass.wheel_unowned[axis] {
                self.wheel_owned[axis] = true;
            }
        }
        let zero_scroll = self.wheel_owned;
        let changes_events = !pass.consumed.is_empty() || !pass.replaced.is_empty();
        if changes_events || zero_scroll.contains(&true) {
            let mut in_sync = true;
            ctx.input_mut(|input| {
                if input.events.len() == events.len() {
                    for (index, replacement) in pass.replaced.drain(..) {
                        if let Some(slot) = input.events.get_mut(index) {
                            *slot = replacement;
                        }
                    }
                    // Indices are ascending; remove from the back so earlier ones stay valid.
                    for index in pass.consumed.iter().rev() {
                        input.events.remove(*index);
                    }
                } else {
                    in_sync = false;
                }
                if zero_scroll[0] {
                    input.smooth_scroll_delta.x = 0.0;
                }
                if zero_scroll[1] {
                    input.smooth_scroll_delta.y = 0.0;
                }
            });
            // Logged outside the closure (no logging under the input lock): something rewrote the
            // events between the read and the write in this function.
            if changes_events && !in_sync {
                ms_log::runtime_log::log_warn(format!(
                    "[ms_input] matched key/wheel events were not consumed: the frame's event list changed under the router (expected {} events)",
                    events.len()
                ));
            }
        }

        let focus = self.focus(pointer, view_under_pointer);
        let modes = if suppressed { BTreeSet::new() } else { self.state.active_modes(&self.registry, &active) };
        let armed = if suppressed { Vec::new() } else { self.armed_views(view_under_pointer, &active) };
        let touch_selectable = if suppressed { Vec::new() } else { self.touch_selectable_views(&active) };
        let claims = self.claims.iter().map(|claim| self.claimed_gesture(claim, &active)).collect();
        self.frame = InputFrame {
            registry: Arc::clone(&self.registry),
            suppressed,
            state: self.state.clone(),
            fired: pass.fired.into_iter().map(|(action, params)| Fired { action, params, focus }).collect(),
            modes,
            axes: pass.axes.into_iter().map(|(action, acc)| AxisSample { action, delta: acc.delta, steps: acc.steps, focus }).collect(),
            claims,
            armed,
            touch_selectable,
            presses: self.presses.clone(),
            view_under_pointer,
            active,
        };
        &self.frame
    }

    /// Starts recording a new binding alternative for `action` (see `capture.rs`). Replaces a
    /// running capture. Input is suppressed until it finishes or is cancelled.
    pub fn capture_begin(&mut self, action: ActionId, slot: CaptureSlot) {
        ms_log::trace_log!(cat::INPUT, "ms_input capture begin action={} slot={slot:?}", action.0);
        self.capture = Some(CaptureSession::new(action, slot, self.state.held().clone()));
    }

    /// The screen rect in which mouse presses are recorded by the running capture (`None`: no
    /// mouse button is recorded). Call it every frame the capture UI is drawn.
    pub fn capture_set_mouse_zone(&mut self, zone: Option<egui::Rect>) {
        if let Some(capture) = &mut self.capture {
            capture.set_mouse_zone(zone);
        }
    }

    /// The running capture, if any.
    #[must_use]
    pub fn capture_status(&self) -> Option<&CaptureStatus> {
        self.capture.as_ref().map(CaptureSession::status)
    }

    /// Ends the running capture and returns the action's new alternative list (apply it with
    /// `registry_mut().set_bindings(action, ..)`, which validates it).
    ///
    /// # Errors
    /// [`CaptureError::NotCapturing`] without a capture; otherwise the errors of
    /// [`captured_alternatives`]. The capture ends in every case.
    pub fn capture_finish(&mut self) -> Result<Vec<Binding>, CaptureError> {
        let capture = self.capture.take().ok_or(CaptureError::NotCapturing)?;
        let (action, slot, chords) = capture.into_chords();
        let result = captured_alternatives(&self.registry, action, slot, &chords);
        ms_log::trace_log!(cat::INPUT, "ms_input capture finish action={} chords={} ok={}", action.0, chords.len(), result.is_ok());
        result
    }

    /// Drops the running capture without changing anything.
    pub fn capture_cancel(&mut self) {
        if let Some(capture) = self.capture.take() {
            ms_log::trace_log!(cat::INPUT, "ms_input capture cancelled action={}", capture.status().action.0);
        }
    }

    /// The surface whose interactive area holds `pos`, with nothing but the background (or the
    /// surface's own capture layer) on top.
    ///
    /// Overlap precedence: every containing surface is filtered by the layer rule first; one whose
    /// capture layer IS the top layer wins, otherwise the later-published one.
    fn surface_at(&self, ctx: &Context, pos: Pos2) -> Option<&ViewSurface> {
        let top = ctx.layer_id_at(pos);
        let candidates: Vec<&ViewSurface> = self
            .surfaces
            .iter()
            .filter(|surface| surface.contains(pos))
            .filter(|surface| top.is_none_or(|layer| layer.order == Order::Background || Some(layer) == surface.capture_layer))
            .collect();
        candidates.iter().rev().find(|surface| top.is_some() && surface.capture_layer == top).or_else(|| candidates.last()).copied()
    }

    /// The view an input acts on: the one under the pointer, else the primary view's centre.
    fn focus(&self, pointer: Option<Pos2>, under: Option<ViewId>) -> Option<ViewFocus> {
        if let (Some(view), Some(pos)) = (under, pointer) {
            return Some(ViewFocus { view, pos });
        }
        self.surfaces.iter().find(|surface| surface.primary).map(|surface| ViewFocus { view: surface.id, pos: surface.rect.center() })
    }

    fn on_event(&mut self, ctx: &Context, index: usize, event: &Event, active: &ActiveSet, suppressed: bool, pass: &mut EventPass) {
        match event {
            Event::Key { key, physical_key, pressed, repeat, modifiers } => {
                self.sync_modifiers(*modifiers, active, suppressed, pass);
                if is_modifier_key(*key) {
                    // A modifier key is not a key button: its role is fed from `modifiers`.
                    // Super keys are the exception where `Modifiers` cannot report them (on
                    // macOS they are ⌘, already the Command role).
                    if is_super_key(*key) && !cfg!(target_os = "macos") {
                        if *pressed {
                            self.super_keys.insert(*key);
                        } else {
                            self.super_keys.remove(key);
                        }
                        self.sync_modifiers(*modifiers, active, suppressed, pass);
                    }
                    if self.capture.is_some() {
                        pass.consumed.push(index);
                    }
                    return;
                }
                let button = Button::Key(physical_key.unwrap_or(*key));
                if self.capture.is_some() {
                    // Nothing reacts to keys while a binding is being recorded.
                    pass.consumed.push(index);
                }
                if *pressed {
                    let matched = self.feed(&MatchEvent::Press { button: button.clone(), repeat: *repeat }, None, active, suppressed, pass);
                    if matched {
                        self.consumed_keys.insert(button);
                        if self.capture.is_none() {
                            pass.consumed.push(index);
                        }
                    } else if self.consumed_keys.contains(&button) && self.capture.is_none() {
                        pass.consumed.push(index);
                    }
                } else {
                    self.feed(&MatchEvent::Release { button: button.clone() }, None, active, suppressed, pass);
                    if self.consumed_keys.remove(&button) && self.capture.is_none() {
                        pass.consumed.push(index);
                    }
                }
            }
            Event::ModifiersChanged(modifiers) => self.sync_modifiers(*modifiers, active, suppressed, pass),
            Event::PointerButton { pos, button, pressed, modifiers } => {
                self.sync_modifiers(*modifiers, active, suppressed, pass);
                let mouse = mouse_button(*button);
                if *pressed {
                    self.feed(&MatchEvent::Press { button: Button::Mouse(mouse), repeat: false }, Some(*pos), active, suppressed, pass);
                    let mut env = active.clone();
                    env.pointer_on_view = self.surface_at(ctx, *pos).is_some();
                    self.presses.insert(mouse, PressSnapshot { state: self.state.clone(), env });
                    self.released.retain(|released| *released != mouse);
                    if !suppressed {
                        self.try_claim(ctx, mouse, *pos, active);
                    }
                } else {
                    self.feed(&MatchEvent::Release { button: Button::Mouse(mouse) }, Some(*pos), active, suppressed, pass);
                    self.released.push(mouse);
                    self.finish_claims(|driver| driver == ClaimDriver::Mouse(mouse), ReleasePolicy::Commit, Some(*pos));
                }
            }
            Event::MouseWheel { unit, delta, phase, modifiers } => {
                self.sync_modifiers(*modifiers, active, suppressed, pass);
                if suppressed {
                    return;
                }
                // Ownership is decided per component: an axis may own Y while X stays egui's.
                let owned_y = delta.y != 0.0 && self.wheel_axis(AxisSource::WheelY, delta.y, *unit, active, pass);
                let owned_x = delta.x != 0.0 && self.wheel_axis(AxisSource::WheelX, delta.x, *unit, active, pass);
                let kept_x = delta.x != 0.0 && !owned_x;
                let kept_y = delta.y != 0.0 && !owned_y;
                // egui folds both raw components into one smoothed axis under its scroll
                // modifiers (`wheel_state.rs` `on_wheel_event`): map raw -> smoothed axis alike.
                let (horizontal, vertical) = pass.scroll_fold;
                let fold = |raw: usize| match (modifiers.matches_any(horizontal), modifiers.matches_any(vertical)) {
                    (true, false) => 0,
                    (false, true) => 1,
                    (true, true) | (false, false) => raw,
                };
                for (raw, owned, kept) in [(0, owned_x, kept_x), (1, owned_y, kept_y)] {
                    if owned {
                        pass.wheel_owned[fold(raw)] = true;
                    }
                    if kept {
                        pass.wheel_unowned[fold(raw)] = true;
                    }
                }
                if (owned_x || owned_y) && !kept_x && !kept_y {
                    pass.consumed.push(index);
                } else if owned_x || owned_y {
                    let rest = Vec2::new(if owned_x { 0.0 } else { delta.x }, if owned_y { 0.0 } else { delta.y });
                    pass.replaced.push((index, Event::MouseWheel { unit: *unit, delta: rest, phase: *phase, modifiers: *modifiers }));
                }
            }
            Event::Zoom(factor) => {
                if !suppressed && factor.is_finite() && *factor > 0.0 {
                    let log = factor.ln();
                    if let Some(axis) = self.state.match_axis(&self.registry, AxisSource::Pinch, active) {
                        let entry = pass.axes.entry(axis.action).or_insert(AxisAcc { delta: 0.0, steps: 0 });
                        entry.delta += log * axis.sensitivity * sign(axis.invert);
                    }
                }
            }
            Event::WindowFocused(false) => pass.focus_lost = true,
            // `egui::Event` is a foreign enum: text, clipboard, pointer motion, touch, IME and the
            // rest carry nothing the matcher binds (touch arrives through `touch::peek_selection`).
            _ => {}
        }
    }

    /// Feeds one press / release to the matcher (and to a running capture); records fired
    /// commands. Returns whether the press was matched (fired or advanced a sequence).
    fn feed(&mut self, event: &MatchEvent, pos: Option<Pos2>, active: &ActiveSet, suppressed: bool, pass: &mut EventPass) -> bool {
        if let Some(capture) = &mut self.capture {
            match event {
                MatchEvent::Press { button, repeat: false } => capture.press(button, pos),
                MatchEvent::Release { button } => capture.release(button),
                MatchEvent::Press { repeat: true, .. } | MatchEvent::Reset => {}
            }
        }
        let outcomes = if suppressed { self.state.step(&self.registry, event, &tracking_env()) } else { self.state.step(&self.registry, event, active) };
        let mut matched = false;
        for outcome in outcomes {
            matched = true;
            match outcome {
                MatchOutcome::Fired(fired) => {
                    ms_log::trace_log!(cat::INPUT, "ms_input fired {} params={:?}", fired.action.0, fired.params);
                    pass.fired.push((fired.action, fired.params));
                }
                MatchOutcome::SequencePending => {}
            }
        }
        matched
    }

    /// Brings the fed modifier roles in line with `modifiers` (releases first, then presses).
    fn sync_modifiers(&mut self, modifiers: Modifiers, active: &ActiveSet, suppressed: bool, pass: &mut EventPass) {
        let target = modifier_roles(modifiers, !self.super_keys.is_empty());
        let released: Vec<ModifierKey> = self.modifiers.difference(&target).copied().collect();
        let pressed: Vec<ModifierKey> = target.difference(&self.modifiers).copied().collect();
        for role in released {
            self.modifiers.remove(&role);
            self.feed(&MatchEvent::Release { button: Button::Modifier(role) }, None, active, suppressed, pass);
        }
        for role in pressed {
            self.modifiers.insert(role);
            self.feed(&MatchEvent::Press { button: Button::Modifier(role), repeat: false }, None, active, suppressed, pass);
        }
    }

    /// Accumulates one wheel event into the axis `source` drives, if any. Returns whether one did.
    fn wheel_axis(&mut self, source: AxisSource, value: f32, unit: MouseWheelUnit, active: &ActiveSet, pass: &mut EventPass) -> bool {
        let Some(axis) = self.state.match_axis(&self.registry, source, active) else { return false };
        let sign = sign(axis.invert);
        let entry = pass.axes.entry(axis.action).or_insert(AxisAcc { delta: 0.0, steps: 0 });
        match unit {
            MouseWheelUnit::Line | MouseWheelUnit::Page => {
                entry.delta += value * axis.sensitivity * sign;
                entry.steps += if value * sign > 0.0 { 1 } else { -1 };
            }
            MouseWheelUnit::Point => {
                entry.delta += value / WHEEL_NOTCH_PT * axis.sensitivity * sign;
                let remainder = self.wheel_remainder.entry(axis.action).or_insert(0.0);
                *remainder += value * sign;
                while *remainder >= WHEEL_NOTCH_PT {
                    *remainder -= WHEEL_NOTCH_PT;
                    entry.steps += 1;
                }
                while *remainder <= -WHEEL_NOTCH_PT {
                    *remainder += WHEEL_NOTCH_PT;
                    entry.steps -= 1;
                }
            }
        }
        true
    }

    /// Claims a canvas-wide `Drag` gesture for a press of `mouse` at `pos` (P2).
    fn try_claim(&mut self, ctx: &Context, mouse: MouseButton, pos: Pos2, active: &ActiveSet) {
        let Some(view) = self.surface_at(ctx, pos).map(|surface| surface.id) else { return };
        if self.claims.iter().any(|claim| claim.view == view) {
            return;
        }
        let mut env = active.clone();
        env.pointer_on_view = true;
        let targets = self.canvas_wide_targets();
        let button = Button::Mouse(mouse);
        let Some(matched) = self.state.match_gesture(&self.registry, &button, GestureKind::Drag, &targets, &env) else { return };
        ms_log::trace_log!(cat::INPUT, "ms_input claim {} view={:?} button={mouse:?} pos={pos:?}", matched.action.0, view.0);
        self.claims.push(Claim { matched, view, driver: ClaimDriver::Mouse(mouse), origin: pos, current: pos, phase: GesturePhase::Started });
    }

    /// Every declared canvas-wide target.
    fn canvas_wide_targets(&self) -> Vec<TargetId> {
        self.registry.targets().iter().filter(|target| target.canvas_wide).map(|target| target.id).collect()
    }

    /// Ends the claims `which` selects with `policy` (no-op for already finished ones).
    fn finish_claims(&mut self, which: impl Fn(ClaimDriver) -> bool, policy: ReleasePolicy, pos: Option<Pos2>) {
        for claim in self.claims.iter_mut().filter(|claim| which(claim.driver) && !matches!(claim.phase, GesturePhase::Finished(_))) {
            if let Some(pos) = pos {
                claim.current = pos;
            }
            claim.phase = GesturePhase::Finished(policy);
            ms_log::trace_log!(cat::INPUT, "ms_input claim finished {} policy={policy:?}", claim.matched.action.0);
        }
    }

    fn cancel_claims(&mut self, cause: &str) {
        if self.claims.iter().any(|claim| !matches!(claim.phase, GesturePhase::Finished(_))) {
            ms_log::trace_log!(cat::INPUT, "ms_input claims cancelled: {cause}");
        }
        self.finish_claims(|_| true, ReleasePolicy::Cancel, None);
    }

    /// Window focus loss: everything is released (no release events will arrive).
    fn on_focus_lost(&mut self) {
        ms_log::trace_log!(cat::INPUT, "ms_input focus lost: held state reset");
        // A reset only clears the matcher state; it never fires anything.
        self.state.step(&self.registry, &MatchEvent::Reset, &tracking_env());
        self.modifiers.clear();
        self.super_keys.clear();
        self.consumed_keys.clear();
        self.presses.clear();
        self.released.clear();
        self.wheel_remainder.clear();
        self.wheel_owned = [false; 2];
        if let Some(capture) = &mut self.capture {
            capture.reset_held();
        }
        self.cancel_claims("focus lost");
    }

    /// Advances running claims: position, Started -> Active, mode releases per policy, and the
    /// touch router's selection drags.
    fn update_claims(&mut self, ctx: &Context, pointer: Option<Pos2>, active: &ActiveSet, suppressed: bool) {
        let modes = self.state.active_modes(&self.registry, active);
        let policies: Vec<(usize, ReleasePolicy)> = self
            .claims
            .iter()
            .enumerate()
            .filter(|(_, claim)| matches!(claim.phase, GesturePhase::Started | GesturePhase::Active) && matches!(claim.driver, ClaimDriver::Mouse(_)))
            .filter_map(|(index, claim)| {
                let released: Vec<ReleasePolicy> = claim.matched.modes.iter().filter(|mode| !modes.contains(mode)).map(|mode| self.release_policy(*mode)).collect();
                if released.is_empty() {
                    None
                } else if released.contains(&ReleasePolicy::Cancel) {
                    Some((index, ReleasePolicy::Cancel))
                } else {
                    Some((index, ReleasePolicy::Commit))
                }
            })
            .collect();
        for (index, policy) in policies {
            if let Some(claim) = self.claims.get_mut(index) {
                ms_log::trace_log!(cat::INPUT, "ms_input claim {} ended by a mode release policy={policy:?}", claim.matched.action.0);
                claim.phase = GesturePhase::Finished(policy);
            }
        }
        for claim in &mut self.claims {
            if let (ClaimDriver::Mouse(_), Some(pos), GesturePhase::Started | GesturePhase::Active) = (claim.driver, pointer, claim.phase) {
                claim.current = pos;
            }
        }
        if !suppressed {
            self.update_touch_claims(ctx, active);
        }
    }

    /// Turns the touch router's selection drags into claims of their views' touch alternatives.
    fn update_touch_claims(&mut self, ctx: &Context, active: &ActiveSet) {
        let owners: Vec<(ViewId, egui::Id)> = self.surfaces.iter().filter_map(|surface| surface.touch_owner.map(|owner| (surface.id, owner))).collect();
        let mut delivered: Vec<egui::Id> = Vec::new();
        for (view, owner) in owners {
            let Some(selection) = peek_selection(ctx, owner) else { continue };
            delivered.push(owner);
            self.on_touch_selection(view, owner, selection, active);
        }
        // The touch router reports a running selection every frame; a claim whose owner got no
        // selection this frame lost its drag (surface gone, router switched off).
        self.finish_claims(|driver| matches!(driver, ClaimDriver::Touch(owner) if !delivered.contains(&owner)), ReleasePolicy::Cancel, None);
    }

    fn on_touch_selection(&mut self, view: ViewId, owner: egui::Id, selection: TouchSelection, active: &ActiveSet) {
        let existing = self.claims.iter().position(|claim| claim.driver == ClaimDriver::Touch(owner));
        let index = if let Some(index) = existing {
            index
        } else {
            if !selection.began {
                return;
            }
            let mut env = active.clone();
            env.pointer_on_view = true;
            let button = Button::Touch(TouchSource::LongPressDrag);
            let targets = self.canvas_wide_targets();
            let Some(matched) = self.state.match_gesture(&self.registry, &button, GestureKind::Drag, &targets, &env) else { return };
            ms_log::trace_log!(cat::INPUT, "ms_input touch claim {} view={:?}", matched.action.0, view.0);
            self.claims.push(Claim {
                matched,
                view,
                driver: ClaimDriver::Touch(owner),
                origin: selection.origin,
                current: selection.pos,
                phase: GesturePhase::Started,
            });
            self.claims.len() - 1
        };
        let Some(claim) = self.claims.get_mut(index) else { return };
        claim.current = selection.pos;
        if let Some(end) = selection.end {
            claim.phase = GesturePhase::Finished(match end {
                TouchSelectionEnd::Released => ReleasePolicy::Commit,
                TouchSelectionEnd::Cancelled => ReleasePolicy::Cancel,
            });
        }
    }

    /// `DragX` / `DragY` axes: the pointer movement while a mouse claim runs.
    fn drag_axes(&mut self, delta: Vec2, active: &ActiveSet, pass: &mut EventPass) {
        let running = self.claims.iter().any(|claim| matches!(claim.driver, ClaimDriver::Mouse(_)) && matches!(claim.phase, GesturePhase::Started | GesturePhase::Active));
        if !running {
            return;
        }
        for (source, value) in [(AxisSource::DragX, delta.x), (AxisSource::DragY, delta.y)] {
            if value == 0.0 {
                continue;
            }
            if let Some(axis) = self.state.match_axis(&self.registry, source, active) {
                let entry = pass.axes.entry(axis.action).or_insert(AxisAcc { delta: 0.0, steps: 0 });
                entry.delta += value * axis.sensitivity * sign(axis.invert);
            }
        }
    }

    /// The release policy of mode `id` (commit for anything that is not a declared mode).
    fn release_policy(&self, id: ActionId) -> ReleasePolicy {
        match self.registry.spec(id) {
            Some(ActionSpec::Mode(mode)) => mode.on_release_mid_gesture,
            Some(ActionSpec::Command(_) | ActionSpec::Axis(_) | ActionSpec::Gesture(_)) | None => ReleasePolicy::Commit,
        }
    }

    /// Views that must raise their capture `Area` (see `InputFrame::canvas_wide_armed`).
    fn armed_views(&self, under: Option<ViewId>, active: &ActiveSet) -> Vec<ViewId> {
        let mut armed: Vec<ViewId> =
            self.claims.iter().filter(|claim| matches!(claim.driver, ClaimDriver::Mouse(_)) && !matches!(claim.phase, GesturePhase::Finished(_))).map(|claim| claim.view).collect();
        if let Some(view) = under
            && !armed.contains(&view)
            && self.canvas_wide_drag_holds(active, |button| matches!(button, Button::Mouse(_)))
        {
            armed.push(view);
        }
        armed
    }

    /// Views whose finger long-press drag would be claimed now.
    fn touch_selectable_views(&self, active: &ActiveSet) -> Vec<ViewId> {
        let mut env = active.clone();
        env.pointer_on_view = true;
        if !self.canvas_wide_drag_holds(&env, |button| *button == Button::Touch(TouchSource::LongPressDrag)) {
            return Vec::new();
        }
        self.surfaces.iter().filter(|surface| surface.touch_owner.is_some()).map(|surface| surface.id).collect()
    }

    /// Whether some canvas-wide `Drag` gesture whose button satisfies `button_filter` would match
    /// a press of that button now.
    fn canvas_wide_drag_holds(&self, env: &ActiveSet, button_filter: impl Fn(&Button) -> bool) -> bool {
        let targets = self.canvas_wide_targets();
        if targets.is_empty() {
            return false;
        }
        let buttons: BTreeSet<Button> = self
            .registry
            .actions()
            .filter(|view| matches!(view.spec, Some(ActionSpec::Gesture(spec)) if targets.contains(&spec.target)))
            .flat_map(|view| view.bindings.iter())
            .filter_map(|binding| match binding {
                Binding::Gesture(gesture) if gesture.kind == GestureKind::Drag && button_filter(&gesture.button) => Some(gesture.button.clone()),
                Binding::Gesture(_) | Binding::Command(_) | Binding::Mode(_) | Binding::Axis(_) => None,
            })
            .collect();
        buttons.iter().any(|button| self.state.match_gesture(&self.registry, button, GestureKind::Drag, &targets, env).is_some())
    }

    /// The public view of one claim with its parameters sampled now.
    fn claimed_gesture(&self, claim: &Claim, active: &ActiveSet) -> ClaimedGesture {
        ClaimedGesture {
            action: claim.matched.action,
            target: claim.matched.target,
            view: claim.view,
            phase: claim.phase,
            origin: claim.origin,
            current: claim.current,
            params: self.state.gesture_params(&self.registry, &claim.matched, active),
            button: claim.driver.button(),
        }
    }
}

/// `-1.0` for an inverted binding, else `1.0`.
fn sign(invert: bool) -> f32 {
    if invert { -1.0 } else { 1.0 }
}

/// The activity suppressed input is fed with: a popup is "open", so by the matcher contract no
/// command fires and no sequence advances, while held buttons are still tracked exactly.
fn tracking_env() -> ActiveSet {
    ActiveSet { popup_open: true, ..ActiveSet::default() }
}

/// Whether `key` is a modifier KEY (egui-winit reports Shift / Ctrl / Alt / Super presses as
/// `Event::Key` with their physical key besides the `modifiers` change).
fn is_modifier_key(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::ShiftLeft
            | egui::Key::ShiftRight
            | egui::Key::ControlLeft
            | egui::Key::ControlRight
            | egui::Key::AltLeft
            | egui::Key::AltRight
            | egui::Key::SuperLeft
            | egui::Key::SuperRight
    )
}

/// Whether `key` is a Super (Windows / system) key.
fn is_super_key(key: egui::Key) -> bool {
    matches!(key, egui::Key::SuperLeft | egui::Key::SuperRight)
}

/// The modifier roles `modifiers` reports. `Command` is egui's platform command modifier (Ctrl,
/// ⌘ on macOS). `Super` is the remaining system key: the Control key on macOS (where ⌘ is the
/// command), else a held Super key (`super_held`), which egui's `Modifiers` does not carry.
fn modifier_roles(modifiers: Modifiers, super_held: bool) -> BTreeSet<ModifierKey> {
    let super_role = if cfg!(target_os = "macos") { modifiers.ctrl } else { super_held };
    [(modifiers.command, ModifierKey::Command), (modifiers.shift, ModifierKey::Shift), (modifiers.alt, ModifierKey::Alt), (super_role, ModifierKey::Super)]
        .into_iter()
        .filter(|(on, _)| *on)
        .map(|(_, role)| role)
        .collect()
}
