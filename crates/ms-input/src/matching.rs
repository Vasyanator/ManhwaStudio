/*
File: crates/ms-input/src/matching.rs

Purpose:
The pure matcher: turns an abstract stream of button presses / releases into fired commands,
reports which modes are active, and picks the gesture / axis an input belongs to. No egui
`Context`: the runtime feeds `MatchEvent`s and an `ActiveSet` per frame.

Key structures:
- `MatchState`: held buttons, mode-suppressed buttons, pending key sequences, repeat owners.
- `MatchEvent`, `MatchOutcome`, `FiredCommand`, `GestureMatch`, `AxisMatch`.

Key functions:
- `MatchState::step`: one press / release / reset.
- `MatchState::active_modes`, `mode_active`, `match_gesture`, `gesture_params`, `match_axis`.

Matching contract:
- A chord (resolved through mode references) matches when all its buttons are held and the held
  MODIFIER set equals the chord's modifier set exactly, except that modifiers of currently
  active ACCEPTED modes are tolerated. Extra held non-modifier buttons are allowed.
- A command fires on the press that completes the last chord of an alternative (any of its
  buttons, a modifier included, may be the completing one). That press is then excluded from
  mode activation until it is released (Z vs Ctrl+Z).
- One press fires at most one command: the longest sequence, then the most explained held
  buttons (the chord's resolved buttons plus those of active accepted modes), then the innermost
  scope, then declaration order. Mode references do not add specificity: an inner-scope binding
  equal to an outer one's resolution wins, exactly as the shadow report says.
- Sequences keep pending prefixes; any non-modifier press that continues nothing resets them
  (no timeout). A press that only advances a sequence fires nothing (`SequencePending`).
- Repeat presses re-fire only the `repeat` command their button fired, while it still matches.
- While a text edit has focus no chord holding a keyboard key matches (commands, modes, axes,
  gestures); while a popup is open no command fires.
- Modes are active while one alternative is a SUBSET of the held, non-suppressed buttons, then
  every mode beaten (`ModeSpec::beats`) by another active mode is dropped.
- Gestures rank by explained buttons, target priority, then innermost scope; axes by explained
  buttons, then innermost scope.
*/

use std::collections::{BTreeMap, BTreeSet};

use crate::action::{Accept, ActionId, ActionSpec, Sample, ScopeLevel, TargetId};
use crate::binding::{AxisSource, Binding, Chord, GestureKind};
use crate::button::Button;
use crate::context::ActiveSet;
use crate::registry::InputRegistry;

/// One abstract input event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchEvent {
    /// `button` went down; `repeat` marks a key auto-repeat of a still-held button.
    Press { button: Button, repeat: bool },
    Release { button: Button },
    /// Focus loss: everything is released, pending sequences are dropped.
    Reset,
}

/// A command fired by a press, with the accepted parameter modes active at that press (in
/// `accepts` declaration order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiredCommand {
    pub action: ActionId,
    pub params: Vec<ActionId>,
}

/// The result of one press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchOutcome {
    Fired(FiredCommand),
    /// The press advanced at least one key sequence and fired nothing; the runtime consumes it.
    SequencePending,
}

/// A gesture an input belongs to, with the accepted modes active at its press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GestureMatch {
    pub action: ActionId,
    pub target: TargetId,
    pub at_press: Vec<ActionId>,
    /// Index of the winning alternative in the action's current bindings.
    pub alternative: usize,
    /// The mode references of the winning alternative's chord (the runtime ends a claimed
    /// gesture by their release policies when one is released mid-gesture).
    pub modes: BTreeSet<ActionId>,
}

/// The axis an input source belongs to, with the matched alternative's tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisMatch {
    pub action: ActionId,
    pub sensitivity: f32,
    pub invert: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    action: ActionId,
    alt: usize,
    next: usize,
}

/// How well a chord matched: the number of held buttons it explains (more is more specific).
type Specificity = usize;

/// The matcher's memory between events.
#[derive(Debug, Clone, Default)]
pub struct MatchState {
    held: BTreeSet<Button>,
    /// Buttons whose press completed a command chord or advanced a sequence; they do not
    /// activate modes until released.
    suppressed: BTreeSet<Button>,
    pending: Vec<Pending>,
    /// The command each still-held button fired (repeat owner).
    fired_by: BTreeMap<Button, ActionId>,
}

impl MatchState {
    /// The currently held buttons.
    #[must_use]
    pub fn held(&self) -> &BTreeSet<Button> {
        &self.held
    }

    /// Whether a key sequence is waiting for its next chord.
    #[must_use]
    pub fn sequence_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Processes one event and returns what it fired.
    pub fn step(&mut self, registry: &InputRegistry, event: &MatchEvent, env: &ActiveSet) -> Vec<MatchOutcome> {
        match event {
            MatchEvent::Press { button, repeat: true } if self.held.contains(button) => self.repeat(registry, button, env),
            MatchEvent::Press { button, repeat: _ } => self.press(registry, button.clone(), env),
            MatchEvent::Release { button } => {
                self.held.remove(button);
                self.suppressed.remove(button);
                self.fired_by.remove(button);
                Vec::new()
            }
            MatchEvent::Reset => {
                *self = Self::default();
                Vec::new()
            }
        }
    }

    fn press(&mut self, registry: &InputRegistry, button: Button, env: &ActiveSet) -> Vec<MatchOutcome> {
        self.held.insert(button.clone());
        let active_modes = self.active_modes(registry, env);
        // (sequence length, specificity, level, declaration rank) of a completed alternative.
        let mut best: Option<((usize, Specificity, ScopeLevel), FiredCommand)> = None;
        let mut advances: Vec<Pending> = Vec::new();
        let mut consider = |key: (usize, Specificity, ScopeLevel), fired: FiredCommand| {
            if best.as_ref().is_none_or(|(best_key, _)| key > *best_key) {
                best = Some((key, fired));
            }
        };
        for view in registry.actions() {
            let Some(ActionSpec::Command(spec)) = view.spec else { continue };
            if env.popup_open || !env.meta_active(view.meta) {
                continue;
            }
            for (alt, binding) in view.bindings.iter().enumerate() {
                let Binding::Command(command) = binding else { continue };
                let length = command.sequence.len();
                let continued = self.pending.iter().filter(|pending| pending.action == view.meta.id && pending.alt == alt).map(|pending| pending.next);
                let positions: BTreeSet<usize> = continued.chain(std::iter::once(0)).collect();
                for position in positions {
                    let Some(chord) = command.sequence.get(position) else { continue };
                    let Some(specificity) = self.chord_match(registry, chord, Some(&button), &spec.accepts, &active_modes, env) else { continue };
                    let next = position.saturating_add(1);
                    if next == length {
                        let params = accepted_now(&spec.accepts, &active_modes);
                        consider((length, specificity, view.meta.scope.level()), FiredCommand { action: view.meta.id, params });
                    } else {
                        advances.push(Pending { action: view.meta.id, alt, next });
                    }
                }
            }
        }
        if let Some((_, fired)) = best {
            self.pending.clear();
            self.suppressed.insert(button.clone());
            self.fired_by.insert(button, fired.action);
            return vec![MatchOutcome::Fired(fired)];
        }
        if !advances.is_empty() {
            self.pending = advances;
            self.suppressed.insert(button);
            return vec![MatchOutcome::SequencePending];
        }
        if !button.is_modifier() {
            self.pending.clear();
        }
        Vec::new()
    }

    fn repeat(&self, registry: &InputRegistry, button: &Button, env: &ActiveSet) -> Vec<MatchOutcome> {
        let Some(&action) = self.fired_by.get(button) else { return Vec::new() };
        let Some(view) = registry.action(action.0) else { return Vec::new() };
        let Some(ActionSpec::Command(spec)) = view.spec else { return Vec::new() };
        if !spec.repeat || env.popup_open || !env.meta_active(view.meta) {
            return Vec::new();
        }
        let active_modes = self.active_modes(registry, env);
        let still_matches = view.bindings.iter().any(|binding| match binding {
            Binding::Command(command) => command
                .sequence
                .last()
                .is_some_and(|chord| self.chord_match(registry, chord, Some(button), &spec.accepts, &active_modes, env).is_some()),
            Binding::Mode(_) | Binding::Axis(_) | Binding::Gesture(_) => false,
        });
        if still_matches {
            vec![MatchOutcome::Fired(FiredCommand { action, params: accepted_now(&spec.accepts, &active_modes) })]
        } else {
            Vec::new()
        }
    }

    /// Every active mode (subset match over held, non-suppressed buttons, then `beats`).
    #[must_use]
    pub fn active_modes(&self, registry: &InputRegistry, env: &ActiveSet) -> BTreeSet<ActionId> {
        let effective: BTreeSet<&Button> = self.held.iter().filter(|button| !self.suppressed.contains(*button)).collect();
        let mut raw: Vec<(ActionId, &[ActionId])> = Vec::new();
        for view in registry.actions() {
            let Some(ActionSpec::Mode(spec)) = view.spec else { continue };
            if !env.meta_active(view.meta) {
                continue;
            }
            let held = view.bindings.iter().any(|binding| match binding {
                Binding::Mode(chord) => {
                    chord.buttons.iter().all(|button| effective.contains(button)) && !(env.text_edit_focused && chord.buttons.iter().any(Button::is_key))
                }
                Binding::Command(_) | Binding::Axis(_) | Binding::Gesture(_) => false,
            });
            if held {
                raw.push((view.meta.id, spec.beats.as_slice()));
            }
        }
        raw.iter()
            .filter(|(id, _)| !raw.iter().any(|(_, beats)| beats.contains(id)))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Whether mode `id` is active.
    #[must_use]
    pub fn mode_active(&self, registry: &InputRegistry, id: ActionId, env: &ActiveSet) -> bool {
        self.active_modes(registry, env).contains(&id)
    }

    /// The gesture a `kind` gesture with `button` on one of `targets` (every target the pointer
    /// hits) belongs to, under the currently held chord. `None` when no active gesture matches.
    #[must_use]
    pub fn match_gesture(&self, registry: &InputRegistry, button: &Button, kind: GestureKind, targets: &[TargetId], env: &ActiveSet) -> Option<GestureMatch> {
        let active_modes = self.active_modes(registry, env);
        let mut best: Option<((Specificity, u16, ScopeLevel), GestureMatch)> = None;
        for view in registry.actions() {
            let Some(ActionSpec::Gesture(spec)) = view.spec else { continue };
            if !targets.contains(&spec.target) || !env.meta_active(view.meta) {
                continue;
            }
            let priority = registry.target(spec.target).map_or(0, |target| target.priority);
            for (alternative, binding) in view.bindings.iter().enumerate() {
                let Binding::Gesture(gesture) = binding else { continue };
                if gesture.button != *button || gesture.kind != kind {
                    continue;
                }
                let Some(specificity) = self.chord_match(registry, &gesture.chord, None, &spec.accepts, &active_modes, env) else { continue };
                let key = (specificity, priority, view.meta.scope.level());
                if best.as_ref().is_none_or(|(best_key, _)| key > *best_key) {
                    let at_press = accepted_now(&spec.accepts, &active_modes);
                    best = Some((key, GestureMatch { action: view.meta.id, target: spec.target, at_press, alternative, modes: gesture.chord.modes.clone() }));
                }
            }
        }
        best.map(|(_, matched)| matched)
    }

    /// The parameters of a running gesture now: `AtPress` accepts as sampled at its press,
    /// `LatestDuringGesture` accepts as active now (in `accepts` declaration order).
    #[must_use]
    pub fn gesture_params(&self, registry: &InputRegistry, gesture: &GestureMatch, env: &ActiveSet) -> Vec<ActionId> {
        let Some(ActionSpec::Gesture(spec)) = registry.spec(gesture.action) else { return Vec::new() };
        let active_modes = self.active_modes(registry, env);
        spec.accepts
            .iter()
            .filter(|accept| match accept.sample {
                Sample::AtPress => gesture.at_press.contains(&accept.mode),
                Sample::LatestDuringGesture => active_modes.contains(&accept.mode),
            })
            .map(|accept| accept.mode)
            .collect()
    }

    /// The axis `source` drives under the currently held chord, `None` when no active axis
    /// matches (the runtime then leaves the input to egui).
    #[must_use]
    pub fn match_axis(&self, registry: &InputRegistry, source: AxisSource, env: &ActiveSet) -> Option<AxisMatch> {
        let active_modes = self.active_modes(registry, env);
        let mut best: Option<((Specificity, ScopeLevel), AxisMatch)> = None;
        for view in registry.actions() {
            let Some(ActionSpec::Axis(_)) = view.spec else { continue };
            if !env.meta_active(view.meta) {
                continue;
            }
            for binding in view.bindings {
                let Binding::Axis(axis) = binding else { continue };
                if axis.source != source {
                    continue;
                }
                let Some(specificity) = self.chord_match(registry, &axis.chord, None, &[], &active_modes, env) else { continue };
                let key = (specificity, view.meta.scope.level());
                if best.as_ref().is_none_or(|(best_key, _)| key > *best_key) {
                    best = Some((key, AxisMatch { action: view.meta.id, sensitivity: axis.sensitivity, invert: axis.invert }));
                }
            }
        }
        best.map(|(_, matched)| matched)
    }

    /// How specifically `chord` matches the held buttons (best resolution), or `None`.
    /// `completing`, when given, must be one of the resolved chord's buttons.
    fn chord_match(
        &self,
        registry: &InputRegistry,
        chord: &Chord,
        completing: Option<&Button>,
        accepts: &[Accept],
        active_modes: &BTreeSet<ActionId>,
        env: &ActiveSet,
    ) -> Option<Specificity> {
        // Held buttons that belong to an active accepted mode: tolerated as extra modifiers and
        // counted as explained.
        let accepted: BTreeSet<Button> = accepts
            .iter()
            .filter(|accept| active_modes.contains(&accept.mode))
            .flat_map(|accept| registry.mode_button_sets(accept.mode))
            .filter(|set| set.is_subset(&self.held))
            .flatten()
            .collect();
        let held_modifiers: BTreeSet<&Button> = self.held.iter().filter(|button| button.is_modifier()).collect();
        registry
            .resolve(chord)
            .into_iter()
            .filter(|resolved| completing.is_none_or(|button| resolved.contains(button)))
            .filter(|resolved| resolved.is_subset(&self.held))
            .filter(|resolved| !(env.text_edit_focused && resolved.iter().any(Button::is_key)))
            .filter(|resolved| {
                held_modifiers
                    .iter()
                    .all(|modifier| resolved.contains(*modifier) || accepted.contains(*modifier))
            })
            .map(|resolved| resolved.union(&accepted).count())
            .max()
    }
}

/// The accepted modes among `active_modes`, in `accepts` declaration order.
fn accepted_now(accepts: &[Accept], active_modes: &BTreeSet<ActionId>) -> Vec<ActionId> {
    accepts.iter().filter(|accept| active_modes.contains(&accept.mode)).map(|accept| accept.mode).collect()
}
