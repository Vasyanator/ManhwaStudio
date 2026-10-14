/*
File: crates/ms-input/src/conflicts.rs

Purpose:
Conflict and shadow detection over every pair of alternatives of every pair of actions.

Key structures:
- `ConflictReport`: the full list of conflicts and shadows.
- `Conflict` / `ConflictKind`: an ambiguity on ONE scope level, or a chord the platform never
  delivers (reserved clipboard chords).
- `Shadow`: an overlap across scope levels; the inner action wins while active (information).

Key functions:
- `compute`: the report for a registry's current bindings.

Rules:
- Two actions are compared only if they can be active together (`may_be_co_active`).
- Every chord is compared by its EFFECTIVE button sets, the same sets the matcher can explain:
  each resolved set, also united with any choice of alternatives of the action's accepted modes
  (`T` accepting `recent.2 = Num2` covers both {T} and {Num2, T}). The expansion per chord is
  bounded by `CONFLICT_EXPANSION_LIMIT`; an action whose expansion hit the bound is listed in
  `ConflictReport::truncated` (its analysis is incomplete, never a freeze).
- Commands: equal resolved chords at every position of equally long sequences = `SameChord`; a
  strictly shorter sequence matching the prefix of a longer one = `PrefixOf`. A one-chord command
  equal to a mode alternative is `SameChord` too (the command press would suppress the mode).
- Modes: equal alternatives = `SameChord`, unless one `beats` the other. Different chords never
  conflict (subset chords are ordered by specificity / `beats`).
- Axes: same source and equal resolved chord = `SameAxis`. Gestures: same target, button, kind
  and equal resolved chord = `SameGesture` (different targets are ordered by priority).
- `ReservedClipboard`: a command / mode chord equal to Cmd+C / Cmd+X / Cmd+V (and on Windows
  Ctrl+Insert / Shift+Delete / Shift+Insert): egui-winit turns those into clipboard events, so the
  binding can never fire. Fixed actions are exempt.
- Alternatives of the same action never conflict with each other.
*/

use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::action::{Accept, ActionId, ActionKind, ActionSpec, TargetId};
use crate::binding::{AxisSource, Binding, Chord, GestureKind};
use crate::button::{Button, ModifierKey};
use crate::context::may_be_co_active;
use crate::registry::{ActionView, InputRegistry};

/// The kind of a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictKind {
    SameChord,
    PrefixOf,
    ReservedClipboard,
    SameGesture,
    SameAxis,
}

/// A real ambiguity: alternative `a_alt` of `a` against alternative `b_alt` of `b` on the same
/// scope level, or (`b == None`) an alternative the platform never delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Conflict {
    pub kind: ConflictKind,
    pub a: ActionId,
    pub a_alt: usize,
    pub b: Option<ActionId>,
    pub b_alt: Option<usize>,
}

/// Information: alternative `outer_alt` of `outer` is taken over by alternative `inner_alt` of
/// the more inner-scoped `inner` while `inner` is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Shadow {
    pub inner: ActionId,
    pub inner_alt: usize,
    pub outer: ActionId,
    pub outer_alt: usize,
}

/// Maximum number of effective button sets one chord expands to during conflict analysis.
pub const CONFLICT_EXPANSION_LIMIT: usize = 1024;

/// Every conflict and shadow over a registry's current bindings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictReport {
    pub conflicts: Vec<Conflict>,
    pub shadows: Vec<Shadow>,
    /// Actions whose accepted-mode expansion hit `CONFLICT_EXPANSION_LIMIT`: their conflicts may
    /// be incomplete.
    pub truncated: Vec<ActionId>,
}

/// The effective button sets of one chord.
type Sets = BTreeSet<BTreeSet<Button>>;

/// One alternative reduced to what overlap detection compares.
enum Form {
    Sequence(Vec<Sets>),
    Mode(BTreeSet<Button>),
    Axis(AxisSource, Sets),
    Gesture(Option<TargetId>, Button, GestureKind, Sets),
}

/// The effective sets of `chord` for an action accepting `accepts`: every resolved set, also
/// united with one alternative of any subset of the accepted modes. Sets `truncated` when the
/// expansion stopped at [`CONFLICT_EXPANSION_LIMIT`].
fn effective(registry: &InputRegistry, chord: &Chord, accepts: &[Accept], truncated: &mut bool) -> Sets {
    let mut sets: Sets = registry.resolve(chord).into_iter().collect();
    for accept in accepts {
        let alternatives = registry.mode_button_sets(accept.mode);
        let base: Vec<BTreeSet<Button>> = sets.iter().cloned().collect();
        for set in &base {
            for alternative in &alternatives {
                if sets.len() >= CONFLICT_EXPANSION_LIMIT {
                    *truncated = true;
                    return sets;
                }
                sets.insert(set.union(alternative).cloned().collect());
            }
        }
    }
    sets
}

fn form_of(registry: &InputRegistry, view: &ActionView<'_>, binding: &Binding, truncated: &mut bool) -> Form {
    let accepts = view.spec.map_or(&[][..], ActionSpec::accepts);
    match binding {
        Binding::Command(command) => Form::Sequence(command.sequence.iter().map(|chord| effective(registry, chord, accepts, truncated)).collect()),
        Binding::Mode(chord) => Form::Mode(chord.buttons.clone()),
        Binding::Axis(axis) => Form::Axis(axis.source, effective(registry, &axis.chord, accepts, truncated)),
        Binding::Gesture(gesture) => {
            let target = match view.spec {
                Some(ActionSpec::Gesture(spec)) => Some(spec.target),
                Some(ActionSpec::Command(_) | ActionSpec::Mode(_) | ActionSpec::Axis(_)) | None => None,
            };
            Form::Gesture(target, gesture.button.clone(), gesture.kind, effective(registry, &gesture.chord, accepts, truncated))
        }
    }
}

fn intersects(a: &Sets, b: &Sets) -> bool {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    small.iter().any(|set| large.contains(set))
}

fn beats(view: &ActionView<'_>, other: ActionId) -> bool {
    match view.spec {
        Some(ActionSpec::Mode(mode)) => mode.beats.contains(&other),
        Some(ActionSpec::Command(_) | ActionSpec::Axis(_) | ActionSpec::Gesture(_)) | None => false,
    }
}

/// The overlap kind of two alternatives of different actions, `None` when they cannot both
/// match one input.
fn overlap(a_view: &ActionView<'_>, a: &Form, b_view: &ActionView<'_>, b: &Form) -> Option<ConflictKind> {
    match (a, b) {
        (Form::Sequence(left), Form::Sequence(right)) => {
            let common = left.len().min(right.len());
            let prefix_equal = left.iter().zip(right.iter()).take(common).all(|(l, r)| intersects(l, r));
            if !prefix_equal || common == 0 {
                None
            } else if left.len() == right.len() {
                Some(ConflictKind::SameChord)
            } else {
                Some(ConflictKind::PrefixOf)
            }
        }
        (Form::Sequence(sequence), Form::Mode(mode)) | (Form::Mode(mode), Form::Sequence(sequence)) => {
            (sequence.len() == 1 && sequence.first().is_some_and(|chord| chord.contains(mode))).then_some(ConflictKind::SameChord)
        }
        (Form::Mode(left), Form::Mode(right)) => {
            let ordered = beats(a_view, b_view.meta.id) || beats(b_view, a_view.meta.id);
            (left == right && !ordered).then_some(ConflictKind::SameChord)
        }
        (Form::Axis(left_source, left), Form::Axis(right_source, right)) => {
            (left_source == right_source && intersects(left, right)).then_some(ConflictKind::SameAxis)
        }
        (Form::Gesture(left_target, left_button, left_kind, left), Form::Gesture(right_target, right_button, right_kind, right)) => {
            (left_target == right_target && left_button == right_button && left_kind == right_kind && intersects(left, right))
                .then_some(ConflictKind::SameGesture)
        }
        // Different kinds of input never reach the same matcher path.
        (Form::Sequence(_) | Form::Mode(_), Form::Axis(..) | Form::Gesture(..))
        | (Form::Axis(..) | Form::Gesture(..), Form::Sequence(_) | Form::Mode(_))
        | (Form::Axis(..), Form::Gesture(..))
        | (Form::Gesture(..), Form::Axis(..)) => None,
    }
}

/// The chords egui-winit turns into clipboard events (never delivered as key presses).
fn reserved_chords() -> Sets {
    let chord = |modifier: ModifierKey, key: egui::Key| -> BTreeSet<Button> { [Button::Modifier(modifier), Button::Key(key)].into_iter().collect() };
    let mut reserved: Sets = [chord(ModifierKey::Command, egui::Key::C), chord(ModifierKey::Command, egui::Key::X), chord(ModifierKey::Command, egui::Key::V)].into_iter().collect();
    if cfg!(windows) {
        reserved.insert(chord(ModifierKey::Command, egui::Key::Insert));
        reserved.insert(chord(ModifierKey::Shift, egui::Key::Delete));
        reserved.insert(chord(ModifierKey::Shift, egui::Key::Insert));
    }
    reserved
}

fn hits_reserved(form: &Form, reserved: &Sets) -> bool {
    match form {
        Form::Sequence(sequence) => sequence.iter().any(|chord| intersects(chord, reserved)),
        Form::Mode(mode) => reserved.contains(mode),
        Form::Axis(..) | Form::Gesture(..) => false,
    }
}

/// The conflict / shadow report of `registry`'s current bindings.
#[must_use]
pub fn compute(registry: &InputRegistry) -> ConflictReport {
    let reserved = reserved_chords();
    let mut report = ConflictReport::default();
    let actions: Vec<(ActionView<'_>, Vec<Form>)> = registry
        .actions()
        .map(|view| {
            let mut truncated = false;
            let forms = view.bindings.iter().map(|binding| form_of(registry, &view, binding, &mut truncated)).collect();
            if truncated {
                report.truncated.push(view.meta.id);
            }
            (view, forms)
        })
        .collect();
    for (view, forms) in &actions {
        if view.fixed || !matches!(view.kind, ActionKind::Command | ActionKind::Mode) {
            continue;
        }
        for (alt, form) in forms.iter().enumerate() {
            if hits_reserved(form, &reserved) {
                report.conflicts.push(Conflict { kind: ConflictKind::ReservedClipboard, a: view.meta.id, a_alt: alt, b: None, b_alt: None });
            }
        }
    }
    for (position, (a_view, a_forms)) in actions.iter().enumerate() {
        for (b_view, b_forms) in actions.iter().skip(position.saturating_add(1)) {
            if !may_be_co_active(a_view.meta, b_view.meta, registry.exclusive_contexts()) {
                continue;
            }
            let (a_level, b_level) = (a_view.meta.scope.level(), b_view.meta.scope.level());
            for (a_alt, a_form) in a_forms.iter().enumerate() {
                for (b_alt, b_form) in b_forms.iter().enumerate() {
                    let Some(kind) = overlap(a_view, a_form, b_view, b_form) else { continue };
                    match a_level.cmp(&b_level) {
                        Ordering::Equal => {
                            report.conflicts.push(Conflict { kind, a: a_view.meta.id, a_alt, b: Some(b_view.meta.id), b_alt: Some(b_alt) });
                        }
                        Ordering::Greater => {
                            report.shadows.push(Shadow { inner: a_view.meta.id, inner_alt: a_alt, outer: b_view.meta.id, outer_alt: b_alt });
                        }
                        Ordering::Less => {
                            report.shadows.push(Shadow { inner: b_view.meta.id, inner_alt: b_alt, outer: a_view.meta.id, outer_alt: a_alt });
                        }
                    }
                }
            }
        }
    }
    report
}
