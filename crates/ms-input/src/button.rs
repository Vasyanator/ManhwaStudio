/*
File: crates/ms-input/src/button.rs

Purpose:
The physical inputs a binding is made of: keyboard keys, modifier roles, mouse buttons, touch
gesture sources and an open-ended `Unknown` device button that persists unchanged.

Key structures:
- `Button`: one bindable input; `Ord` puts modifiers first, so a `BTreeSet<Button>` iterates in
  label order.
- `ModifierKey`, `MouseButton`, `TouchSource`: the closed button families.

Notes:
- `Button::Key` holds the PHYSICAL key position (egui's `physical_key`, falling back to the
  logical key where the platform has no physical one, e.g. the web). Non-QWERTY layouts see the
  QWERTY name of the position.
- Pen / gamepad / tablet-pad adapters do not exist yet; their buttons are expressible (and
  survive a round trip through the config) as `Button::Unknown`.
*/

/// A modifier ROLE, independent of the side of the keyboard it was pressed on.
///
/// `Command` is the platform command modifier: Ctrl on Linux / Windows, ⌘ on macOS (egui's
/// `Modifiers::command`). `Super` is the remaining system key. The declaration order is the
/// label order ("Ctrl+Shift+Alt").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ModifierKey {
    Command,
    Shift,
    Alt,
    Super,
}

/// A mouse button. `Extra(n)` numbers the side buttons from 1 (egui's `Extra1` is `Extra(1)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Extra(u8),
}

/// A touch gesture SOURCE recognized by the touch router. It acts as a button: a binding
/// alternative may use it where it would use a mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TouchSource {
    LongPress,
    LongPressDrag,
    TwoFingerPinch,
    TwoFingerPan,
    OneFingerPan,
}

/// One bindable input.
///
/// Variant order is significant: the derived `Ord` sorts modifiers first, then keys, mouse,
/// touch and unknown buttons, which is the order labels print a chord in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Button {
    /// A held modifier role (either side).
    Modifier(ModifierKey),
    /// A physical keyboard key.
    Key(egui::Key),
    Mouse(MouseButton),
    Touch(TouchSource),
    /// A button of a device this build has no adapter for. `device` is a stable device-family
    /// name, `code` its button code; both persist verbatim.
    Unknown { device: String, code: u32 },
}

impl Button {
    /// Whether this is a modifier role (compared EXACTLY by the matcher, never by subset).
    #[must_use]
    pub fn is_modifier(&self) -> bool {
        matches!(self, Self::Modifier(_))
    }

    /// Whether this is a keyboard key (a chord holding one never matches while a text edit has
    /// keyboard focus).
    #[must_use]
    pub fn is_key(&self) -> bool {
        matches!(self, Self::Key(_))
    }

    /// Whether this button may be the button of a pointer gesture (mouse, touch, unknown
    /// device; never a key or a modifier).
    #[must_use]
    pub fn is_pointer_button(&self) -> bool {
        match self {
            Self::Mouse(_) | Self::Touch(_) | Self::Unknown { .. } => true,
            Self::Modifier(_) | Self::Key(_) => false,
        }
    }
}
