/*
File: crates/ms-input/src/capture.rs

Purpose:
Binding capture for the settings pane (P6): records what the user presses as chords and turns
them into one new binding alternative of the action being rebound.

Key structures:
- `CaptureSlot`: which alternative the capture replaces or appends.
- `CaptureStatus`: what the pane shows while recording (recorded chords, buttons held now).
- `CaptureSession`: the recording state machine the router feeds (crate-private).
- `CaptureError`: why a capture cannot become a binding.

Key functions:
- `captured_alternatives`: pure; the action's new alternative list with the captured
  alternative applied to the slot.

Recording contract:
- Recording starts with the first router frame AFTER `capture_begin` (the click that started
  it was processed before); buttons already held at the start are ignored until released.
- A chord is recorded when EVERY held button has been released: it holds every button pressed
  since the previous chord (keys, modifier roles, mouse buttons incl. side buttons). A
  modifier-only chord is valid.
- Mouse presses count only inside the pane's capture zone (`InputRouter::capture_set_mouse_zone`),
  so clicking the pane's own Done / Cancel buttons is never recorded; without a zone no mouse
  button is recorded.
- Several chords form a key sequence; the pane ends it explicitly (`capture_finish`). Cancel
  is a call too: Esc is an ordinary bindable key and is recorded like any other.
- Touch sources are not captured: the pane picks them from a catalog and builds the binding.
- Mode references are structural (P3): the new alternative takes the mode references of its
  template (the replaced alternative, else the action's first current, else first default
  alternative); capture replaces only buttons. Axis source / sensitivity / inversion and the
  gesture kind come from the template as well.
*/

use std::collections::BTreeSet;

use egui::{Pos2, Rect};

use crate::action::{ActionId, ActionKind};
use crate::binding::{AxisBinding, Binding, ButtonChord, Chord, CommandBinding, GestureBinding};
use crate::button::Button;
use crate::registry::InputRegistry;

/// Which alternative a capture produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSlot {
    /// A new alternative after the current ones.
    Append,
    /// Replaces the current alternative at this index.
    Replace(usize),
}

/// The state of a running capture, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureStatus {
    pub action: ActionId,
    pub slot: CaptureSlot,
    /// Finished chords, in press order.
    pub chords: Vec<BTreeSet<Button>>,
    /// Every button pressed since the last finished chord (the chord being formed).
    pub pending: BTreeSet<Button>,
}

/// Why a capture could not become a binding.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    #[error("no binding capture is running")]
    NotCapturing,
    #[error("nothing was captured for `{0}`")]
    Empty(&'static str),
    #[error("unknown input action `{0}`")]
    UnknownAction(&'static str),
    #[error("input action `{0}` is fixed and cannot be rebound")]
    FixedAction(&'static str),
    #[error("input action `{action}` has no alternative {slot}")]
    SlotOutOfRange { action: &'static str, slot: usize },
    #[error("input action `{0}` takes a single chord, not a key sequence")]
    SequenceNotAllowed(&'static str),
    #[error("input action `{0}` has no alternative to take the axis source or gesture kind from")]
    NoTemplate(&'static str),
    #[error("gesture `{action}` needs exactly one mouse or device button in the chord, got {count}")]
    PointerButtonCount { action: &'static str, count: usize },
}

/// The recording state machine (fed by the router, which owns the event stream).
#[derive(Debug, Clone)]
pub(crate) struct CaptureSession {
    status: CaptureStatus,
    /// Held right now (only buttons that count for the capture).
    held: BTreeSet<Button>,
    /// Held at the start, or pressed outside the mouse zone: their releases record nothing.
    ignored: BTreeSet<Button>,
    mouse_zone: Option<Rect>,
}

impl CaptureSession {
    /// Starts recording for `action`; `already_held` are ignored until released.
    pub(crate) fn new(action: ActionId, slot: CaptureSlot, already_held: BTreeSet<Button>) -> Self {
        Self { status: CaptureStatus { action, slot, chords: Vec::new(), pending: BTreeSet::new() }, held: BTreeSet::new(), ignored: already_held, mouse_zone: None }
    }

    pub(crate) fn status(&self) -> &CaptureStatus {
        &self.status
    }

    pub(crate) fn set_mouse_zone(&mut self, zone: Option<Rect>) {
        self.mouse_zone = zone;
    }

    /// A press; `pos` is the pointer position of a mouse press.
    pub(crate) fn press(&mut self, button: &Button, pos: Option<Pos2>) {
        if self.ignored.contains(button) || self.held.contains(button) {
            return;
        }
        if matches!(button, Button::Mouse(_)) && !self.mouse_zone.zip(pos).is_some_and(|(zone, pos)| zone.contains(pos)) {
            self.ignored.insert(button.clone());
            return;
        }
        self.held.insert(button.clone());
        self.status.pending.insert(button.clone());
    }

    /// A release; records the pending chord when nothing counted stays held.
    pub(crate) fn release(&mut self, button: &Button) {
        if self.ignored.remove(button) {
            return;
        }
        if self.held.remove(button) && self.held.is_empty() && !self.status.pending.is_empty() {
            self.status.chords.push(std::mem::take(&mut self.status.pending));
        }
    }

    /// Focus loss: the chord being formed is dropped (its releases will never arrive).
    pub(crate) fn reset_held(&mut self) {
        self.held.clear();
        self.ignored.clear();
        self.status.pending.clear();
    }

    /// The recorded chords (a chord still being held is not part of the result).
    pub(crate) fn into_chords(self) -> (ActionId, CaptureSlot, Vec<BTreeSet<Button>>) {
        (self.status.action, self.status.slot, self.status.chords)
    }
}

/// The alternative list `action` gets when the captured `chords` are written into `slot` (see
/// the module notes for the template rules). The caller applies it with
/// `InputRegistry::set_bindings`, which validates it.
///
/// # Errors
/// [`CaptureError`]: unknown or fixed action, nothing captured, a slot past the current list, a
/// sequence for a non-command, no template for an axis / gesture, or a gesture chord without
/// exactly one pointer button.
pub fn captured_alternatives(registry: &InputRegistry, action: ActionId, slot: CaptureSlot, chords: &[BTreeSet<Button>]) -> Result<Vec<Binding>, CaptureError> {
    let view = registry.action(action.0).ok_or(CaptureError::UnknownAction(action.0))?;
    if view.fixed {
        return Err(CaptureError::FixedAction(action.0));
    }
    let Some(last) = chords.last() else { return Err(CaptureError::Empty(action.0)) };
    let mut alternatives = view.bindings.to_vec();
    let template = match slot {
        CaptureSlot::Replace(index) => Some(alternatives.get(index).ok_or(CaptureError::SlotOutOfRange { action: action.0, slot: index })?),
        CaptureSlot::Append => alternatives.first().or_else(|| view.meta.defaults.first()),
    };
    let single = || if chords.len() == 1 { Ok(last.clone()) } else { Err(CaptureError::SequenceNotAllowed(action.0)) };
    let binding = match view.kind {
        ActionKind::Command => {
            // The template's mode references go on the completing (last) chord, where they hold
            // in every default this crate declares (`[zoom_modifier] + =`).
            let modes = match template {
                Some(Binding::Command(command)) => command.sequence.last().map(|chord| chord.modes.clone()).unwrap_or_default(),
                Some(Binding::Mode(_) | Binding::Axis(_) | Binding::Gesture(_)) | None => BTreeSet::new(),
            };
            let count = chords.len();
            let sequence = chords
                .iter()
                .enumerate()
                .map(|(index, buttons)| Chord { modes: if index + 1 == count { modes.clone() } else { BTreeSet::new() }, buttons: buttons.clone() })
                .collect();
            Binding::Command(CommandBinding { sequence })
        }
        ActionKind::Mode => Binding::Mode(ButtonChord { buttons: single()? }),
        ActionKind::Axis => {
            let Some(Binding::Axis(axis)) = template else { return Err(CaptureError::NoTemplate(action.0)) };
            Binding::Axis(AxisBinding { source: axis.source, chord: Chord { modes: axis.chord.modes.clone(), buttons: single()? }, sensitivity: axis.sensitivity, invert: axis.invert })
        }
        ActionKind::Gesture => {
            let Some(Binding::Gesture(gesture)) = template else { return Err(CaptureError::NoTemplate(action.0)) };
            let buttons = single()?;
            let pointer: Vec<&Button> = buttons.iter().filter(|button| button.is_pointer_button()).collect();
            let [button] = pointer.as_slice() else { return Err(CaptureError::PointerButtonCount { action: action.0, count: pointer.len() }) };
            let button = (*button).clone();
            let rest = buttons.iter().filter(|other| **other != button).cloned().collect();
            Binding::Gesture(GestureBinding { chord: Chord { modes: gesture.chord.modes.clone(), buttons: rest }, button, kind: gesture.kind })
        }
    };
    match slot {
        CaptureSlot::Replace(index) => {
            let entry = alternatives.get_mut(index).ok_or(CaptureError::SlotOutOfRange { action: action.0, slot: index })?;
            *entry = binding;
        }
        CaptureSlot::Append => alternatives.push(binding),
    }
    Ok(alternatives)
}
