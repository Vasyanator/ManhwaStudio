/*
File: crates/ms-input/src/label.rs

Purpose:
Localized display text of bindings, the one source every hint, menu and settings row uses
instead of spelling a binding in a locale string.

Key functions:
- `InputRegistry::label`: every alternative of an action joined by " / ".
- `InputRegistry::alternative_label`, `chord_label`, `param_group_label`.
- `InputRegistry::conflict_text`, `shadow_text`: the settings pane's conflict lines.
- `button_label`: one button.

Format (pinned by tests):
- chord parts are joined by "+": mode references first, then the chord's own buttons
  (modifiers first, in `ModifierKey` order); e.g. "Ctrl+Shift+=".
- a mode reference prints the mode's CURRENT chord: inline when the mode has one alternative,
  "(A / B)" when it has several, "(<no binding>)" when it has none; e.g. "(Ctrl / Z)+=".
- a key sequence joins its chords by ", ".
- an axis prints its chord, then the source name ("Ctrl+Wheel"); a gesture prints its chord, then
  the localized gesture template around the button name ("Shift+LMB drag"); a touch source is
  its own gesture, so it prints without the template.
- alternatives are joined by " / "; no alternative prints `app.hotkey.no_binding`.
- a parameter group prints "first-last" when every member is one single-glyph key and the
  glyphs are consecutive ("1-6"), else the member labels joined by " / ".
- the command modifier prints "Ctrl" on Linux / Windows and "⌘" on macOS.
- word keys (Esc, Space, arrows, ...) come from `input.key.*`; letters, digits, punctuation and
  F-keys print egui's glyph / name (not words).
*/

use crate::action::ActionId;
use crate::binding::{AxisSource, Binding, ButtonChord, Chord, GestureKind};
use crate::button::{Button, ModifierKey, MouseButton, TouchSource};
use crate::conflicts::{Conflict, ConflictKind, Shadow};
use crate::registry::InputRegistry;

/// Separator between alternatives.
pub const ALTERNATIVE_SEPARATOR: &str = " / ";
/// Separator between the parts of one chord.
pub const CHORD_SEPARATOR: &str = "+";
/// Separator between the chords of a key sequence.
pub const SEQUENCE_SEPARATOR: &str = ", ";

/// The localized name of one button.
#[must_use]
pub fn button_label(button: &Button) -> String {
    match button {
        Button::Modifier(modifier) => modifier_label(*modifier).to_owned(),
        Button::Key(key) => key_label(*key).to_owned(),
        Button::Mouse(mouse) => match mouse {
            MouseButton::Left => t!("input.mouse.left").to_owned(),
            MouseButton::Right => t!("input.mouse.right").to_owned(),
            MouseButton::Middle => t!("input.mouse.middle").to_owned(),
            MouseButton::Extra(number) => tf!("input.mouse.extra", number = number),
        },
        Button::Touch(source) => touch_label(*source).to_owned(),
        Button::Unknown { device, code } => tf!("input.unknown_button", device = device, code = code),
    }
}

fn modifier_label(modifier: ModifierKey) -> &'static str {
    match modifier {
        ModifierKey::Command => {
            if cfg!(target_os = "macos") {
                t!("input.modifier.command_mac")
            } else {
                t!("input.modifier.ctrl")
            }
        }
        ModifierKey::Shift => t!("input.modifier.shift"),
        ModifierKey::Alt => t!("input.modifier.alt"),
        ModifierKey::Super => t!("input.modifier.super"),
    }
}

fn touch_label(source: TouchSource) -> &'static str {
    match source {
        TouchSource::LongPress => t!("input.touch.long_press"),
        TouchSource::LongPressDrag => t!("input.touch.long_press_drag"),
        TouchSource::TwoFingerPinch => t!("input.touch.two_finger_pinch"),
        TouchSource::TwoFingerPan => t!("input.touch.two_finger_pan"),
        TouchSource::OneFingerPan => t!("input.touch.one_finger_pan"),
    }
}

/// Word keys are localized; every other key (letters, digits, punctuation, F-keys) prints
/// egui's glyph or name, which is not a word.
fn key_label(key: egui::Key) -> &'static str {
    use egui::Key;
    match key {
        Key::Escape => t!("input.key.escape"),
        Key::Tab => t!("input.key.tab"),
        Key::Backspace => t!("input.key.backspace"),
        Key::Enter => t!("input.key.enter"),
        Key::Space => t!("input.key.space"),
        Key::Insert => t!("input.key.insert"),
        Key::Delete => t!("input.key.delete"),
        Key::Home => t!("input.key.home"),
        Key::End => t!("input.key.end"),
        Key::PageUp => t!("input.key.page_up"),
        Key::PageDown => t!("input.key.page_down"),
        Key::ArrowLeft => t!("input.key.arrow_left"),
        Key::ArrowRight => t!("input.key.arrow_right"),
        Key::ArrowUp => t!("input.key.arrow_up"),
        Key::ArrowDown => t!("input.key.arrow_down"),
        Key::Copy => t!("input.key.copy"),
        Key::Cut => t!("input.key.cut"),
        Key::Paste => t!("input.key.paste"),
        Key::BrowserBack => t!("input.key.browser_back"),
        Key::ShiftLeft => t!("input.key.shift_left"),
        Key::ShiftRight => t!("input.key.shift_right"),
        Key::ControlLeft => t!("input.key.control_left"),
        Key::ControlRight => t!("input.key.control_right"),
        Key::AltLeft => t!("input.key.alt_left"),
        Key::AltRight => t!("input.key.alt_right"),
        Key::SuperLeft => t!("input.key.super_left"),
        Key::SuperRight => t!("input.key.super_right"),
        Key::IntlBackslash => t!("input.key.intl_backslash"),
        // egui::Key is a foreign enum: its remaining variants are glyph / name keys.
        other => other.symbol_or_name(),
    }
}

fn axis_source_label(source: AxisSource) -> &'static str {
    match source {
        AxisSource::WheelY => t!("input.axis.wheel"),
        AxisSource::WheelX => t!("input.axis.wheel_horizontal"),
        AxisSource::Pinch => t!("input.axis.pinch"),
        AxisSource::DragX => t!("input.axis.drag_horizontal"),
        AxisSource::DragY => t!("input.axis.drag_vertical"),
    }
}

fn gesture_label(button: &Button, kind: GestureKind) -> String {
    let name = button_label(button);
    if matches!(button, Button::Touch(_)) {
        return name;
    }
    match kind {
        GestureKind::Click => tf!("input.gesture.click", button = name),
        GestureKind::DoubleClick => tf!("input.gesture.double_click", button = name),
        GestureKind::Drag => tf!("input.gesture.drag", button = name),
        GestureKind::LongPress => tf!("input.gesture.long_press", button = name),
    }
}

fn button_chord_parts(chord: &ButtonChord) -> String {
    chord.buttons.iter().map(button_label).collect::<Vec<_>>().join(CHORD_SEPARATOR)
}

impl InputRegistry {
    /// The display text of `id`: every current alternative joined by " / ", or the
    /// "no binding" text when it has none (also for an unknown id).
    #[must_use]
    pub fn label(&self, id: ActionId) -> String {
        let bindings = self.bindings(id);
        if bindings.is_empty() {
            return t!("app.hotkey.no_binding").to_owned();
        }
        bindings.iter().map(|binding| self.alternative_label(binding)).collect::<Vec<_>>().join(ALTERNATIVE_SEPARATOR)
    }

    /// The display text of one alternative.
    #[must_use]
    pub fn alternative_label(&self, binding: &Binding) -> String {
        match binding {
            Binding::Command(command) => command.sequence.iter().map(|chord| self.chord_label(chord)).collect::<Vec<_>>().join(SEQUENCE_SEPARATOR),
            Binding::Mode(chord) => button_chord_parts(chord),
            Binding::Axis(axis) => self.prefixed(&axis.chord, axis_source_label(axis.source).to_owned()),
            Binding::Gesture(gesture) => self.prefixed(&gesture.chord, gesture_label(&gesture.button, gesture.kind)),
        }
    }

    /// `chord`'s parts followed by `tail`, joined by "+".
    fn prefixed(&self, chord: &Chord, tail: String) -> String {
        if chord.is_empty() {
            tail
        } else {
            format!("{}{CHORD_SEPARATOR}{tail}", self.chord_label(chord))
        }
    }

    /// The display text of one chord: mode references (as their current chords), then buttons.
    #[must_use]
    pub fn chord_label(&self, chord: &Chord) -> String {
        let modes = chord.modes.iter().map(|mode| self.mode_reference_label(*mode));
        let buttons = chord.buttons.iter().map(button_label);
        modes.chain(buttons).collect::<Vec<_>>().join(CHORD_SEPARATOR)
    }

    fn mode_reference_label(&self, mode: ActionId) -> String {
        let alternatives: Vec<String> = self
            .bindings(mode)
            .iter()
            .filter_map(|binding| match binding {
                Binding::Mode(chord) => Some(button_chord_parts(chord)),
                Binding::Command(_) | Binding::Axis(_) | Binding::Gesture(_) => None,
            })
            .collect();
        match alternatives.as_slice() {
            [] => format!("({})", t!("app.hotkey.no_binding")),
            [single] => single.clone(),
            several => format!("({})", several.join(ALTERNATIVE_SEPARATOR)),
        }
    }

    /// The display text of a parameter group (e.g. `recent_char.1..6` -> "1-6").
    #[must_use]
    pub fn param_group_label(&self, ids: &[ActionId]) -> String {
        if ids.is_empty() {
            return t!("app.hotkey.no_binding").to_owned();
        }
        let glyphs: Option<Vec<char>> = ids.iter().map(|id| self.single_glyph(*id)).collect();
        if let Some(glyphs) = glyphs
            && glyphs.len() >= 2
            && glyphs.windows(2).all(|pair| pair.first().zip(pair.get(1)).is_some_and(|(a, b)| u32::from(*a).checked_add(1) == Some(u32::from(*b))))
            && let (Some(first), Some(last)) = (glyphs.first(), glyphs.last())
        {
            return format!("{first}-{last}");
        }
        ids.iter().map(|id| self.label(*id)).collect::<Vec<_>>().join(ALTERNATIVE_SEPARATOR)
    }

    /// The glyph of an action bound to exactly one single-key alternative whose label is one
    /// character.
    fn single_glyph(&self, id: ActionId) -> Option<char> {
        let [binding] = self.bindings(id) else { return None };
        let key = match binding {
            Binding::Mode(chord) => single_key(chord.buttons.iter()),
            Binding::Command(command) => match command.sequence.as_slice() {
                [chord] if chord.modes.is_empty() => single_key(chord.buttons.iter()),
                _ => None,
            },
            Binding::Axis(_) | Binding::Gesture(_) => None,
        }?;
        let label = key_label(key);
        let mut chars = label.chars();
        let glyph = chars.next()?;
        chars.next().is_none().then_some(glyph)
    }

    /// The settings-pane line for `conflict`, worded from the side of `viewer`.
    #[must_use]
    pub fn conflict_text(&self, conflict: &Conflict, viewer: ActionId) -> String {
        let other_id = if conflict.a == viewer { conflict.b } else { Some(conflict.a) };
        let other = other_id.map_or_else(String::new, |id| self.title(id));
        match conflict.kind {
            ConflictKind::SameChord => tf!("input.conflict.same_chord", other = other),
            ConflictKind::PrefixOf => tf!("input.conflict.prefix_of", other = other),
            ConflictKind::ReservedClipboard => t!("input.conflict.reserved_clipboard").to_owned(),
            ConflictKind::SameGesture => tf!("input.conflict.same_gesture", other = other),
            ConflictKind::SameAxis => tf!("input.conflict.same_axis", other = other),
        }
    }

    /// The settings-pane information line for `shadow`, worded from the side of `viewer`.
    #[must_use]
    pub fn shadow_text(&self, shadow: &Shadow, viewer: ActionId) -> String {
        if shadow.outer == viewer {
            tf!("input.shadow.outer_info", inner = self.title(shadow.inner))
        } else {
            tf!("input.shadow.inner_info", outer = self.title(shadow.outer))
        }
    }

    /// The localized title of `id` (the id itself when unknown).
    #[must_use]
    pub fn title(&self, id: ActionId) -> String {
        self.action(id.0).map_or_else(|| id.0.to_owned(), |view| ms_i18n::resolve_key(view.meta.title_key).to_owned())
    }
}

fn single_key<'a>(mut buttons: impl Iterator<Item = &'a Button>) -> Option<egui::Key> {
    let first = buttons.next()?;
    if buttons.next().is_some() {
        return None;
    }
    match first {
        Button::Key(key) => Some(*key),
        Button::Modifier(_) | Button::Mouse(_) | Button::Touch(_) | Button::Unknown { .. } => None,
    }
}
