/*
File: crates/ms-text-render/src/font_provider.rs

Purpose:
Caller-supplied font source for the renderer. Fonts reach the render path by
WORKING NAME through a `FontProvider`; the renderer never touches the filesystem
to load a font. This is the groundwork for future "virtual fonts" (renamed or
composed from several files) that have no single backing file.

Main responsibilities:
- define the resolved font payload the renderer consumes (`FontContent`);
- define the read-only, thread-safe lookup contract (`FontProvider`);
- provide a trivial in-memory provider for tests/standalone bins
  (`FontContentSet`);
- provide the stable content-id hash used as the per-`FontSystem` load-cache key
  (`font_content_id`).

Key structures:
- `FontContent`
- `FontContentSet`

Key traits/functions:
- `FontProvider::resolve`
- `font_content_id`

Notes:
`content_id` MUST be produced by `font_content_id` on every provider (app-side and
the path-based compat loader alike) so identical bytes share one id and register
into a reused `FontSystem` only once.
*/

use std::sync::Arc;

/// A fully-resolved font available to a render: the working name it is referenced
/// by, its original name (the real family/name from the file for real fonts; a
/// synthesized "VirtualFont_a_b_c" for virtual fonts), the raw bytes, the face to
/// use, and a stable content id used as the per-FontSystem load-cache key.
#[derive(Debug, Clone)]
pub struct FontContent {
    /// Working/reference name. `TextRenderParams.font_name` and inline `<font=...>`
    /// tags resolve to this.
    pub name: String,
    /// Original name from the font file for real fonts; for virtual fonts a
    /// synthesized name. Not used by the renderer itself (kept for callers that
    /// need the real identity, e.g. PSD export).
    pub original_name: String,
    /// Font file bytes (a real .ttf/.otf today; composed/renamed virtual later).
    pub data: Arc<Vec<u8>>,
    /// Face index within `data`.
    pub face_index: usize,
    /// Stable identity of `data` (content hash). Used as the load-cache key so the
    /// same bytes register into a reused FontSystem only once.
    pub content_id: u64,
}

/// Read-only source of fonts by working name, shared with background render
/// threads. The caller (the typing tab) owns the implementation (lazy file read
/// today; virtual fonts later); the renderer only asks by name and never touches
/// the filesystem.
pub trait FontProvider: Send + Sync {
    /// Resolve a working `name` to its content, or `None` if unknown.
    fn resolve(&self, name: &str) -> Option<FontContent>;
}

/// Simple in-memory provider over a fixed list of fonts (tests, standalone bins,
/// and any caller that already holds all content). Resolves by exact match first,
/// then case-insensitive.
#[derive(Debug, Clone, Default)]
pub struct FontContentSet {
    fonts: Vec<FontContent>,
}

impl FontContentSet {
    /// Builds a provider over the given fixed list of fonts.
    #[must_use]
    pub fn new(fonts: Vec<FontContent>) -> Self {
        Self { fonts }
    }
}

impl FontProvider for FontContentSet {
    fn resolve(&self, name: &str) -> Option<FontContent> {
        if let Some(f) = self.fonts.iter().find(|f| f.name == name) {
            return Some(f.clone());
        }
        self.fonts
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(name))
            .cloned()
    }
}

/// Stable content id for font bytes (a `DefaultHasher` over the whole buffer).
/// Used as the load-cache key; the app-side provider and the path-based compat
/// loader must use THIS function so identical bytes share one id.
#[must_use]
pub fn font_content_id(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}
