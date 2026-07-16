/*
File: crates/ms-gifs/src/typing.rs

Purpose:
Declares the embedded animated hints for character-metric controls in the typing
tab, pairing each WebP byte stream with its stable cache identity.
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

/// Complete stable-order inventory of typing hint assets.
pub(crate) const ALL: [Hint; 4] = [KERNING, CHAR_WIDTH, CHAR_HEIGHT, LINE_SPACING];
