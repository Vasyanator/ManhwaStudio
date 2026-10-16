/*
File: crates/ms-custom-fonts/src/error.rs

Purpose:
`CompileError`, the typed failure of `compile`, `coverage_report`, `included_chars` and
`with_kerning_overrides`.

Notes:
Every variant's `Display` names the font identity or character involved, so a caller can log
it as is (technical detail) and map the variant to its own localized user text. Bad font data
is always an error, never a panic.
*/

use crate::recipe::RecipeError;

/// Why a custom font could not be compiled (or its coverage computed).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CompileError {
    /// The recipe violates its invariants (see `CustomFontRecipe::validate`).
    #[error("invalid custom font recipe: {0}")]
    InvalidRecipe(#[from] RecipeError),
    /// The recipe names a source the caller did not supply (fewer sources than the recipe).
    #[error("source font '{identity}' of the recipe was not supplied")]
    SourceMissing {
        /// Identity of the first recipe source without bytes.
        identity: String,
    },
    /// The caller supplied a source in a recipe slot that names a different font, or more
    /// sources than the recipe has (`expected` is then empty).
    #[error("source font mismatch at position {index}: the recipe expects '{expected}', got '{found}'")]
    SourceMismatch {
        /// Zero-based position in recipe order.
        index: usize,
        /// Identity the recipe holds at that position (empty when the recipe has fewer).
        expected: String,
        /// Identity the caller passed.
        found: String,
    },
    /// The source bytes are not a font `ttf-parser` can read (or the face index is wrong).
    #[error("source font '{identity}' could not be read: {reason}")]
    SourceUnparsable {
        /// Identity of the source.
        identity: String,
        /// Parser diagnostic.
        reason: String,
    },
    /// A user kerning pair of a source carries a non-finite value.
    #[error("user kerning pair '{left}{right}' of font '{identity}' is not a finite number")]
    InvalidUserKerning {
        /// Identity of the source.
        identity: String,
        /// Left character of the pair.
        left: char,
        /// Right character of the pair.
        right: char,
    },
    /// The custom font would need more glyphs than a TrueType font can hold (65 535).
    #[error("the custom font would need {count} glyphs, more than a TrueType font can hold")]
    TooManyGlyphs {
        /// Glyphs required (including `.notdef`).
        count: usize,
    },
    /// One glyph's outline cannot be stored or blended (coordinates beyond the 16-bit range,
    /// too many points to store, an aligned extent too large to blend, or more flattened
    /// points than the blend work budget); `ch` is the first character mapped to it. Fails
    /// the whole compile.
    #[error("glyph for U+{:04X} cannot be stored: {reason}", u32::from(*ch))]
    GlyphTooComplex {
        /// First character mapped to the glyph.
        ch: char,
        /// What limit was exceeded.
        reason: String,
    },
    /// A table of the output exceeds a size limit of the format.
    #[error("the custom font's '{table}' table is too large for the format")]
    OutputTooLarge {
        /// Table tag.
        table: &'static str,
    },
    /// `with_kerning_overrides` was given bytes it cannot rewrite: not a parsable TrueType
    /// face, or one with tables a compiled face never has (GPOS, a non-format-0 `kern`).
    #[error("the font cannot take kerning overrides: {reason}")]
    UnsupportedFace {
        /// What is wrong with the face.
        reason: String,
    },
    /// A kerning override does not fit the `kern` table's 16-bit value in font units.
    #[error("kerning pair '{left}{right}' of {em} thousandths of an em is out of the font format's range")]
    UserKerningOutOfRange {
        /// Left character of the pair.
        left: char,
        /// Right character of the pair.
        right: char,
        /// The offending value in thousandths of an em.
        em: f32,
    },
    /// The caller's cancel flag was set.
    #[error("custom font compilation was cancelled")]
    Cancelled,
}
