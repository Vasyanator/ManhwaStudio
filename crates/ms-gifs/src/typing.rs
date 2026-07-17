/*
File: crates/ms-gifs/src/typing.rs

Purpose:
Declares the embedded animated hints explaining typing-tab behavior — character
metrics, per-overlay text options, and canvas gestures — pairing each WebP byte
stream with its stable cache identity.
*/

use crate::Hint;

/// Hint animation for character kerning adjustment.
pub const KERNING: Hint = Hint::new(
    "typing.kerning",
    include_bytes!("../assets/kerning.webp"),
);

/// Hint animation for character width adjustment.
pub const CHAR_WIDTH: Hint = Hint::new(
    "typing.char_width",
    include_bytes!("../assets/char_width.webp"),
);

/// Hint animation for character height adjustment.
pub const CHAR_HEIGHT: Hint = Hint::new(
    "typing.char_height",
    include_bytes!("../assets/char_height.webp"),
);

/// Hint animation for line-spacing adjustment.
pub const LINE_SPACING: Hint = Hint::new(
    "typing.line_spacing",
    include_bytes!("../assets/line_spacing.webp"),
);

/// Hint animation for text alignment selection.
pub const ALIGNMENT: Hint = Hint::new(
    "typing.alignment",
    include_bytes!("../assets/alignment.webp"),
);

/// Hint animation for global text rotation adjustment.
pub const GLOBAL_ROTATION: Hint = Hint::new(
    "typing.global_rotation",
    include_bytes!("../assets/global_rotation.webp"),
);

/// Hint animation for text anti-aliasing control.
pub const ANTI_ALIASING: Hint = Hint::new(
    "typing.anti_aliasing",
    include_bytes!("../assets/anti_aliasing.webp"),
);

/// Hint animation for per-overlay hanging-punctuation control.
pub const HANGING_PUNCTUATION: Hint = Hint::new(
    "typing.hanging_punctuation",
    include_bytes!("../assets/hanging_punctuation.webp"),
);

/// Hint animation for the canvas gesture that selects a text area.
///
/// The name intentionally matches the `canvas.bottom_hint.typing.select_text_area_*`
/// localization key stem: this hint explains that shortcut in the canvas bottom hint.
pub const SELECT_TEXT_AREA: Hint = Hint::new(
    "typing.select_text_area",
    include_bytes!("../assets/select_text_area.webp"),
);

/// Complete stable-order inventory of typing hint assets.
pub(crate) const ALL: [Hint; 9] = [
    KERNING,
    CHAR_WIDTH,
    CHAR_HEIGHT,
    LINE_SPACING,
    ALIGNMENT,
    GLOBAL_ROTATION,
    ANTI_ALIASING,
    HANGING_PUNCTUATION,
    SELECT_TEXT_AREA,
];
