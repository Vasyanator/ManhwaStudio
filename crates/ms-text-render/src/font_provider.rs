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
- define the per-font USER-AUTHORED kerning override table
  (`CustomKerningTable`) the provider attaches to a font;
- provide a trivial in-memory provider for tests/standalone bins
  (`FontContentSet`);
- provide the stable content-id hash used as the per-`FontSystem` load-cache key
  (`font_content_id`).

Key structures:
- `FontContent`
- `CustomKerningTable`
- `FontContentSet`

Key traits/functions:
- `FontProvider::resolve`
- `font_content_id`

Notes:
`content_id` MUST be produced by `font_content_id` on every provider (app-side and
the path-based compat loader alike) so identical bytes share one id and register
into a reused `FontSystem` only once.

`CustomKerningTable` is the RENDER-SIDE runtime type. The app persists the same
facts as `ms_tab_typing::font_admin::CustomKerningPair`; the two are deliberately
NOT the same type — persistence owns a serializable ordered list, the renderer
owns a lookup-optimised map — and the app-side provider converts between them.
*/

use std::collections::HashMap;
use std::sync::Arc;

/// Shared, erased font byte buffer — exactly the type `fontdb::Source::Binary`
/// takes, so a provider's bytes reach fontdb without a copy.
///
/// Erased rather than `Arc<Vec<u8>>` on purpose: it lets a provider hand over the
/// `&'static` bytes `ms-fonts` already holds for the bundled `fonts/ui` stack
/// (`Arc::new(&'static [u8])`) instead of reading the same file into a second
/// buffer (`dev-docs/unicode_base_font_plan.md`, phase 5). An owned `Vec<u8>`
/// still coerces into it, so file-backed providers are unchanged.
pub type FontBytes = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// User-authored per-pair kerning overrides for ONE font, in THOUSANDTHS OF AN EM.
///
/// A matching pair REPLACES the font's own kerning for those two characters (see
/// `pipeline::horizontal_run_layout`): the pen steps by the left glyph's raw
/// `hmtx` advance plus this delta, exactly as `KerningMode::Fixed` steps, under
/// EVERY kerning mode. An entry of `0.0` is therefore meaningful — it CANCELS a
/// built-in pair — and must never be filtered out.
///
/// The unit is per-mille of the em so the table is font-size independent:
/// `delta_px = offset_per_mille / 1000 * font_size_px`.
///
/// The font FILE is never modified; the override lives entirely in layout.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CustomKerningTable {
    /// `(left, right)` -> advance delta between them, in thousandths of an em.
    pairs: HashMap<(char, char), f32>,
}

impl CustomKerningTable {
    /// Builds the table from `(left, right, offset_per_mille)` triples.
    ///
    /// A later triple for the same pair wins, so the caller's own ordering decides
    /// duplicates. Zero offsets are KEPT (they cancel a built-in pair); non-finite
    /// offsets are dropped, because a `NaN` delta would poison every pen position
    /// downstream of the pair.
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (char, char, f32)>) -> Self {
        Self {
            pairs: pairs
                .into_iter()
                .filter(|(_, _, offset)| offset.is_finite())
                .map(|(left, right, offset)| ((left, right), offset))
                .collect(),
        }
    }

    /// Whether the table overrides nothing. An empty table must behave exactly like
    /// no table at all (it is the guard every fast path checks).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Number of overridden pairs. Diagnostics only.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Advance delta in px for this pair at `font_size_px`, or `None` when the pair
    /// is not overridden.
    ///
    /// `Some(0.0)` and `None` mean DIFFERENT things: the first cancels the font's
    /// own kerning for the pair, the second leaves it alone. Callers must not
    /// collapse them.
    #[must_use]
    pub fn delta_px(&self, left: char, right: char, font_size_px: f32) -> Option<f32> {
        self.pairs
            .get(&(left, right))
            .map(|offset| offset / 1000.0 * font_size_px)
    }

    /// Advance delta in THOUSANDTHS OF AN EM for this pair, or `None` when the pair
    /// is not overridden. The size-independent form [`Self::delta_px`] scales.
    ///
    /// Used by the width metrics, which measure in per-mille units already
    /// (`wrap::forms::WIDTH_METRIC_EM`), so they need no round trip through px.
    #[must_use]
    pub fn delta_per_mille(&self, left: char, right: char) -> Option<f32> {
        self.pairs.get(&(left, right)).copied()
    }

    /// Every overridden pair with its per-mille offset, in unspecified order.
    /// For callers that must pre-compute something per pair (the wrap metric's
    /// font-kerning correction cache).
    pub fn pairs(&self) -> impl Iterator<Item = (char, char, f32)> + '_ {
        self.pairs
            .iter()
            .map(|(&(left, right), &offset)| (left, right, offset))
    }
}

/// A fully-resolved font available to a render: the working name it is referenced
/// by, its original name (the real family/name from the file for real fonts; a
/// synthesized "VirtualFont_a_b_c" for virtual fonts), the raw bytes, the face to
/// use, and a stable content id used as the per-FontSystem load-cache key.
#[derive(Clone)]
pub struct FontContent {
    /// Working/reference name. `TextRenderParams.font_name` and inline `<font=...>`
    /// tags resolve to this.
    pub name: String,
    /// Original name from the font file for real fonts; for virtual fonts a
    /// synthesized name. Not used by the renderer itself (kept for callers that
    /// need the real identity, e.g. PSD export).
    pub original_name: String,
    /// Font file bytes (a real .ttf/.otf today; composed/renamed virtual later).
    /// May be an owned buffer or the shared `'static` bytes of a bundled font.
    pub data: FontBytes,
    /// Face index within `data`.
    pub face_index: usize,
    /// Stable identity of the ORIGINAL `data` (content hash). Used as the
    /// load-cache key so the same bytes register into a reused FontSystem only
    /// once.
    ///
    /// It identifies the bytes the PROVIDER supplied, not necessarily the bytes
    /// fontdb ends up holding: with `TextRenderParams.force_remove_ellipsis_glyph`
    /// the loader registers a patched copy (`font_ligature_patch`). That stays
    /// unambiguous because the registered face is a deterministic function of
    /// these bytes plus the `EllipsisLigatureMode` of the `FontSystem`, and a
    /// system belongs to exactly one mode for its whole life (the pool is
    /// partitioned by mode — see `font_system_pool.rs`). So one content id can
    /// never mean two different faces inside one system.
    pub content_id: u64,
    /// User-authored kerning pair overrides for THIS font, or `None` when the font
    /// has none. Shared (`Arc`) so `FontContent` stays cheap to clone into every
    /// background render thread, and deliberately NOT part of `content_id`: the
    /// table changes nothing about the BYTES fontdb registers, so a font whose
    /// overrides were edited must still hit the face load cache.
    ///
    /// `None` and `Some(empty)` are equivalent to every consumer; `None` is what a
    /// provider that knows nothing about overrides supplies, and it keeps layout
    /// byte-identical to the behaviour before overrides existed.
    pub custom_kerning: Option<Arc<CustomKerningTable>>,
}

impl FontContent {
    /// The font bytes. Cheap (one deref); the slice borrows `self.data`.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        (*self.data).as_ref()
    }

    /// The font's kerning overrides, or `None` when it has none OR the table it
    /// carries is empty. Collapsing the two here is what lets every consumer test
    /// "are there overrides" with a single `is_some()`.
    #[must_use]
    pub fn custom_kerning_table(&self) -> Option<&Arc<CustomKerningTable>> {
        self.custom_kerning
            .as_ref()
            .filter(|table| !table.is_empty())
    }
}

/// Hand-written because `data` is a trait object (`dyn AsRef<[u8]>`) and cannot
/// derive `Debug`; the buffer is summarized by its length, never dumped.
impl std::fmt::Debug for FontContent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FontContent")
            .field("name", &self.name)
            .field("original_name", &self.original_name)
            .field("data_len", &self.bytes().len())
            .field("face_index", &self.face_index)
            .field("content_id", &self.content_id)
            .field(
                "custom_kerning_pairs",
                &self.custom_kerning.as_ref().map_or(0, |table| table.len()),
            )
            .finish()
    }
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

#[cfg(test)]
mod tests {
    use super::CustomKerningTable;

    #[test]
    fn delta_px_scales_per_mille_with_the_font_size() {
        let table = CustomKerningTable::from_pairs([('A', 'V', -40.0)]);
        // -40 per mille of a 100 px em is -4 px; of a 50 px em, -2 px.
        let at_100 = table.delta_px('A', 'V', 100.0).expect("pair is overridden");
        let at_50 = table.delta_px('A', 'V', 50.0).expect("pair is overridden");
        assert!((at_100 - (-4.0)).abs() < 1e-4, "got {at_100}");
        assert!((at_50 - (-2.0)).abs() < 1e-4, "got {at_50}");
        assert!((table.delta_per_mille('A', 'V').unwrap_or(0.0) - (-40.0)).abs() < 1e-4);
    }

    #[test]
    fn a_zero_entry_is_kept_and_reads_back_as_some_zero() {
        // `0.0` CANCELS the font's own pair kerning, so it must never be filtered
        // out and must never be reported as "not overridden".
        let table = CustomKerningTable::from_pairs([('T', 'o', 0.0)]);
        assert!(!table.is_empty());
        assert_eq!(table.len(), 1);
        let delta = table.delta_px('T', 'o', 64.0);
        assert_eq!(delta, Some(0.0), "a zero entry must read back as Some(0.0)");
    }

    #[test]
    fn an_absent_pair_reads_back_as_none_and_is_direction_sensitive() {
        let table = CustomKerningTable::from_pairs([('A', 'V', -40.0)]);
        assert_eq!(table.delta_px('V', 'A', 64.0), None, "pairs are ordered");
        assert_eq!(table.delta_px('A', 'W', 64.0), None);
        assert_eq!(table.delta_per_mille('A', 'W'), None);
    }

    #[test]
    fn an_empty_table_overrides_nothing() {
        let table = CustomKerningTable::default();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert_eq!(table.delta_px('A', 'V', 64.0), None);
    }

    #[test]
    fn non_finite_offsets_are_dropped_and_the_last_duplicate_wins() {
        // A NaN delta would propagate into every pen position after the pair.
        let table = CustomKerningTable::from_pairs([
            ('A', 'V', f32::NAN),
            ('B', 'C', f32::INFINITY),
            ('D', 'E', 10.0),
            ('D', 'E', 20.0),
        ]);
        assert_eq!(table.delta_px('A', 'V', 64.0), None);
        assert_eq!(table.delta_px('B', 'C', 64.0), None);
        assert_eq!(table.delta_per_mille('D', 'E'), Some(20.0));
    }

    #[test]
    fn pairs_iteration_yields_every_override() {
        let table = CustomKerningTable::from_pairs([('A', 'V', -40.0), ('T', 'o', 0.0)]);
        let mut listed: Vec<(char, char, f32)> = table.pairs().collect();
        listed.sort_by_key(|pair| (pair.0, pair.1));
        assert_eq!(listed, vec![('A', 'V', -40.0), ('T', 'o', 0.0)]);
    }
}
