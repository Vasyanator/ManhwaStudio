/*
File: crates/ms-input/src/binding.rs

Purpose:
One binding ALTERNATIVE of an action, per kind: a command's chord sequence, a mode's button
chord, an axis source under a chord, a pointer gesture under a chord. Also the chord resolution
that expands mode references into concrete button sets.

Key structures:
- `ButtonChord`: buttons only (a Mode alternative; never holds mode references).
- `Chord`: mode references + buttons (commands, axes, gestures).
- `CommandBinding`, `AxisBinding`, `GestureBinding`, `Binding`.
- `BindingError`: why an alternative is rejected (declaration time and `set_bindings`).

Key functions:
- `resolve_chord`: the concrete button sets a chord stands for under the modes' CURRENT bindings
  (bounded by `MAX_COMBINATIONS_PER_BINDING`).
- `combination_count`: how many sets a binding resolves to, computed without materializing them.

Limits (user overrides are untrusted input; validation rejects anything beyond them):
- `MAX_ALTERNATIVES` alternatives per action;
- `MAX_MODE_REFS_PER_CHORD` mode references per chord;
- `MAX_COMBINATIONS_PER_BINDING` resolved button sets per alternative (summed over its chords).

Notes:
- A mode reference resolves to the mode's buttons, not to its activation state: `[zoom_modifier]
  + =` with `zoom_modifier = Ctrl | Z` means exactly `Ctrl+=` or `Z+=`.
*/

use std::collections::BTreeSet;

use crate::action::{ActionId, ActionKind};
use crate::button::Button;

/// Maximum number of alternatives of one action. Not a usability limit (the user asked for
/// unlimited alternatives): only a guard that rejects a corrupted or hostile override list.
pub const MAX_ALTERNATIVES: usize = 256;
/// Maximum number of mode references in one chord.
pub const MAX_MODE_REFS_PER_CHORD: usize = 4;
/// Maximum number of resolved button sets of one alternative (summed over its chords). Also the
/// hard bound `resolve_chord` never exceeds, whatever it is given.
pub const MAX_COMBINATIONS_PER_BINDING: usize = 64;

/// The buttons of one Mode alternative, all held at once.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ButtonChord {
    pub buttons: BTreeSet<Button>,
}

impl ButtonChord {
    /// A chord of exactly these buttons.
    #[must_use]
    pub fn new(buttons: impl IntoIterator<Item = Button>) -> Self {
        Self { buttons: buttons.into_iter().collect() }
    }
}

/// Mode references plus buttons, all held at once. A mode reference stands for any one of the
/// referenced mode's alternatives.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Chord {
    pub modes: BTreeSet<ActionId>,
    pub buttons: BTreeSet<Button>,
}

impl Chord {
    /// A chord of exactly these buttons, without mode references.
    #[must_use]
    pub fn new(buttons: impl IntoIterator<Item = Button>) -> Self {
        Self { modes: BTreeSet::new(), buttons: buttons.into_iter().collect() }
    }

    /// The same chord with `modes` referenced as well.
    #[must_use]
    pub fn with_modes(mut self, modes: impl IntoIterator<Item = ActionId>) -> Self {
        self.modes.extend(modes);
        self
    }

    /// No modes and no buttons (valid for axes and gestures: "no extra chord").
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.modes.is_empty() && self.buttons.is_empty()
    }
}

/// A command alternative: one or more chords pressed in order (a key sequence).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommandBinding {
    pub sequence: Vec<Chord>,
}

/// The continuous source of an axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AxisSource {
    WheelY,
    WheelX,
    Pinch,
    DragX,
    DragY,
}

/// An axis alternative: `source` delivers values while `chord` is held.
#[derive(Debug, Clone, PartialEq)]
pub struct AxisBinding {
    pub source: AxisSource,
    pub chord: Chord,
    /// Multiplier applied to the source value; finite and > 0.
    pub sensitivity: f32,
    pub invert: bool,
}

/// The pointer gesture of a gesture alternative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GestureKind {
    Click,
    DoubleClick,
    Drag,
    LongPress,
}

/// A gesture alternative: `kind` performed with `button` while `chord` is held.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GestureBinding {
    pub chord: Chord,
    /// A mouse, touch or unknown-device button (never a key or a modifier).
    pub button: Button,
    pub kind: GestureKind,
}

/// One alternative of an action; the variant must match the action's kind.
#[derive(Debug, Clone, PartialEq)]
pub enum Binding {
    Command(CommandBinding),
    Mode(ButtonChord),
    Axis(AxisBinding),
    Gesture(GestureBinding),
}

impl Binding {
    /// A single-chord command alternative.
    #[must_use]
    pub fn command(chord: Chord) -> Self {
        Self::Command(CommandBinding { sequence: vec![chord] })
    }

    /// A command alternative of several chords pressed in order.
    #[must_use]
    pub fn sequence(chords: Vec<Chord>) -> Self {
        Self::Command(CommandBinding { sequence: chords })
    }

    /// A mode alternative of exactly these buttons.
    #[must_use]
    pub fn mode(buttons: impl IntoIterator<Item = Button>) -> Self {
        Self::Mode(ButtonChord::new(buttons))
    }

    /// An axis alternative with sensitivity 1 and no inversion.
    #[must_use]
    pub fn axis(source: AxisSource, chord: Chord) -> Self {
        Self::Axis(AxisBinding { source, chord, sensitivity: 1.0, invert: false })
    }

    /// A gesture alternative.
    #[must_use]
    pub fn gesture(chord: Chord, button: Button, kind: GestureKind) -> Self {
        Self::Gesture(GestureBinding { chord, button, kind })
    }

    /// The action kind this alternative belongs to.
    #[must_use]
    pub fn kind(&self) -> ActionKind {
        match self {
            Self::Command(_) => ActionKind::Command,
            Self::Mode(_) => ActionKind::Mode,
            Self::Axis(_) => ActionKind::Axis,
            Self::Gesture(_) => ActionKind::Gesture,
        }
    }

    /// Every chord with mode references this alternative contains (none for a Mode).
    #[must_use]
    pub fn chords(&self) -> Vec<&Chord> {
        match self {
            Self::Command(command) => command.sequence.iter().collect(),
            Self::Mode(_) => Vec::new(),
            Self::Axis(axis) => vec![&axis.chord],
            Self::Gesture(gesture) => vec![&gesture.chord],
        }
    }
}

/// Why an alternative (or a whole override) is rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindingError {
    #[error("unknown input action `{0}`")]
    UnknownAction(String),
    #[error("input action `{0}` is fixed and cannot be rebound")]
    FixedAction(&'static str),
    #[error("input action `{action}` is a {expected:?}, the alternative is a {found:?}")]
    KindMismatch { action: &'static str, expected: ActionKind, found: ActionKind },
    #[error("command `{action}` has an alternative without chords")]
    EmptySequence { action: &'static str },
    #[error("input action `{action}` has a chord with neither modes nor buttons")]
    EmptyChord { action: &'static str },
    #[error("input action `{action}` references unknown mode `{reference}`")]
    UnknownModeRef { action: &'static str, reference: &'static str },
    #[error("input action `{action}` references `{reference}`, which is not a mode")]
    RefNotAMode { action: &'static str, reference: &'static str },
    #[error("gesture `{action}` uses a key or modifier as its pointer button")]
    InvalidGestureButton { action: &'static str },
    #[error("axis `{action}` has a non-finite or non-positive sensitivity")]
    InvalidSensitivity { action: &'static str },
    #[error("input action `{action}` has {count} alternatives (at most {MAX_ALTERNATIVES})")]
    TooManyAlternatives { action: &'static str, count: usize },
    #[error("input action `{action}` has a chord with more than {MAX_MODE_REFS_PER_CHORD} mode references")]
    TooManyModeRefs { action: &'static str },
    #[error("an alternative of `{action}` resolves to more than {MAX_COMBINATIONS_PER_BINDING} button combinations")]
    TooManyCombinations { action: &'static str },
}

/// The number of button sets `binding` resolves to (summed over its chords; each chord is the
/// product of its referenced modes' alternative counts), saturating, without materializing them.
/// `mode_count` returns a mode's alternative count.
#[must_use]
pub fn combination_count(binding: &Binding, mode_count: &dyn Fn(ActionId) -> usize) -> usize {
    binding
        .chords()
        .iter()
        .map(|chord| chord.modes.iter().fold(1_usize, |product, mode| product.saturating_mul(mode_count(*mode))))
        .fold(0_usize, usize::saturating_add)
}

/// The concrete button sets `chord` stands for: its own buttons united with one alternative of
/// every referenced mode (cartesian product). `mode_alternatives` returns the CURRENT
/// alternatives of a mode; a referenced mode without alternatives makes the chord unreachable
/// (empty result). Duplicates are removed; the order is deterministic. The result never holds
/// more than [`MAX_COMBINATIONS_PER_BINDING`] sets: validation keeps real bindings below it, and
/// the bound guarantees that nothing (e.g. a mode rebound later) can blow the expansion up.
#[must_use]
pub fn resolve_chord(chord: &Chord, mode_alternatives: &dyn Fn(ActionId) -> Vec<BTreeSet<Button>>) -> Vec<BTreeSet<Button>> {
    let mut partial: Vec<BTreeSet<Button>> = vec![chord.buttons.clone()];
    for mode in &chord.modes {
        let alternatives = mode_alternatives(*mode);
        let mut next = Vec::with_capacity(partial.len().saturating_mul(alternatives.len()));
        for base in &partial {
            for alternative in &alternatives {
                if next.len() >= MAX_COMBINATIONS_PER_BINDING {
                    break;
                }
                let union: BTreeSet<Button> = base.union(alternative).cloned().collect();
                if !next.contains(&union) {
                    next.push(union);
                }
            }
        }
        partial = next;
    }
    partial
}
