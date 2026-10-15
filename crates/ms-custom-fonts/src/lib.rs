/*
File: crates/ms-custom-fonts/src/lib.rs

Purpose:
Crate root of `ms-custom-fonts`, the single owner of "what a custom font recipe is and how it
compiles into a TrueType face". A custom font is DEFINED by its recipe (references to real
fonts by identity + the kind's parameters); the compiled face is a disposable, in-memory
derivative that callers cache by `RecipeKey`.

Main responsibilities:
- `recipe`: `CustomFontRecipe` / `CustomFontKind` (`Hybrid` implemented; `Combined` planned),
  canonical JSON, validation.
- `key`: `SourceDigest`, `RecipeKey` (SHA-256, stable across machines).
- `source` / `kerning` / `outline`: reading sources into normalized units.
- `hybrid/`: the hybrid compiler (character plan, outline blend, metrics, kerning).
- `sfnt`: the TrueType writer shared by every kind.
- `kern_override`: bakes a font's own user kerning pairs into a compiled face (export).

Public surface:
- `compile`, `coverage_report`, `included_chars`, `with_kerning_overrides`, `CompileScope`,
  `CompiledFont`, `CoverageReport`, `CompileError`, `ENGINE_VERSION`, plus the re-exported
  recipe/key/source types (incl. `RecipeKey::verify_full_face` with `VerifiedFullFace` /
  `FaceKeyMismatch`: the one rule for trusting a stored full face).

Contract:
GUI-free, no i18n, no file or network I/O, no threads of its own (per-glyph work runs on the
caller's rayon pool), no logging: errors are typed and carry their context for the caller to
log. Output bytes are a pure function of the inputs.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

mod error;
mod hybrid;
mod kern_override;
mod kerning;
mod key;
mod outline;
mod recipe;
mod sfnt;
mod source;

use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;

pub use error::CompileError;
pub use kern_override::with_kerning_overrides;
pub use kerning::KerningPairPerMille;
pub use key::{FaceKeyMismatch, RecipeKey, SourceDigest, VerifiedFullFace, recipe_key, source_digest};
pub use recipe::{
    CustomFontKind, CustomFontRecipe, HybridParams, HybridSource, MAX_HYBRID_SOURCES, MIN_HYBRID_SOURCES,
    MissingGlyphPolicy, RecipeError,
};
pub use source::SourceFont;

/// Version of the compile algorithm. Part of every `RecipeKey`: bump it whenever a change can
/// alter output bytes for the same inputs (blend math, grid or fitting constants, writer
/// layout, naming, kerning rules), so stale cached faces are regenerated.
pub const ENGINE_VERSION: u32 = 1;

/// Which characters to compile.
#[derive(Debug, Clone, Copy)]
pub enum CompileScope<'a> {
    /// Every character the recipe yields (the face that is cached, used and exported).
    Full,
    /// Only these characters (live preview). The face gets a distinct name per character set
    /// so it never collides with the full face in a renderer's font pool.
    Subset(&'a BTreeSet<char>),
}

/// Character coverage of a recipe over a scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageReport {
    /// Characters of the scope present in at least one source.
    pub total_chars: usize,
    /// Of those, characters absent from at least one source.
    pub missing_in_some: usize,
    /// Of those, characters left out of the face by a `Disable` policy.
    pub disabled: usize,
    /// The first disabled characters in codepoint order (at most 64).
    pub disabled_sample: Vec<char>,
}

/// A compiled custom face.
#[derive(Debug, Clone)]
pub struct CompiledFont {
    /// The TrueType file (`glyf` outlines, `kern` pairs; see `sfnt.rs` for dropped tables).
    pub bytes: Vec<u8>,
    /// Family name inside the face: `MS Custom <short8>` (full) or
    /// `MS Custom Preview <short8> <subset8>` (subset).
    pub family_name: String,
    /// PostScript name: `MSCustom-<short8>` (full) or `MSCustomPreview-<short8>-<subset8>`.
    pub postscript_name: String,
    /// Key of the recipe and its exact source inputs.
    pub key: RecipeKey,
    /// Coverage of the compiled scope (for `Subset`: of the requested characters only).
    pub report: CoverageReport,
}

/// Compiles `recipe` into a TrueType face.
///
/// `sources` are the recipe's sources in recipe order (identities must match exactly).
/// `progress(done, total)` is called on the calling thread with glyph counts (first with
/// `done = 0`, then after each batch); `cancel` is polled per glyph. Per-glyph work runs on
/// the current rayon pool; output bytes do not depend on its thread count.
///
/// # Errors
/// `InvalidRecipe` (bounds, weights, exact-duplicate identities), `SourceMissing` /
/// `SourceMismatch` (sources do not line up with the recipe), `SourceUnparsable`,
/// `InvalidUserKerning`, `TooManyGlyphs`, `GlyphTooComplex`, `OutputTooLarge`, `Cancelled`.
pub fn compile(
    recipe: &CustomFontRecipe,
    sources: &[SourceFont<'_>],
    scope: CompileScope<'_>,
    progress: &(dyn Fn(usize, usize) + Sync),
    cancel: &AtomicBool,
) -> Result<CompiledFont, CompileError> {
    match &recipe.kind {
        CustomFontKind::Hybrid(params) => hybrid::compile(recipe, params, sources, scope, progress, cancel),
    }
}

/// Character coverage of the whole recipe without compiling outlines (cmap lookups only).
///
/// # Errors
/// The same source and recipe errors as `compile`.
pub fn coverage_report(recipe: &CustomFontRecipe, sources: &[SourceFont<'_>]) -> Result<CoverageReport, CompileError> {
    match &recipe.kind {
        CustomFontKind::Hybrid(params) => hybrid::coverage(recipe, params, sources),
    }
}

/// Which of `candidates` the compiled face of `recipe` maps (cmap lookups only, no outlines).
///
/// The answer is the compiler's own per-character decision (for the hybrid kind: the
/// missing-glyph rule of decision D9), so it always equals the cmap of a `compile` over the
/// same inputs restricted to `candidates`. Callers use it to classify a custom font's
/// language coverage without compiling or re-deriving that rule.
///
/// # Errors
/// The same source and recipe errors as `compile`.
pub fn included_chars(
    recipe: &CustomFontRecipe,
    sources: &[SourceFont<'_>],
    candidates: &BTreeSet<char>,
) -> Result<BTreeSet<char>, CompileError> {
    match &recipe.kind {
        CustomFontKind::Hybrid(params) => hybrid::included_chars(recipe, params, sources, candidates),
    }
}
