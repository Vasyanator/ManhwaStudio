/*
File: hybrid_editor/catalog.rs

Purpose:
The fonts the hybrid editor offers as SOURCES, and how their identities become bytes.
A host builds one `SourceCatalog` per font list (the typing panel from its own list and
provider, the settings pane through `font_admin::hybrid_source_catalog` on its load worker)
and lends it to `draw_hybrid_editor` every frame.

Key structures:
- `SourceCatalog`: the offered sources (bundled interface font first, then the real fonts;
  never a custom font, decision D4) + the resolver every preview, coverage and commit job
  resolves through, + a process-unique serial so the editor notices a replaced catalog.
- `SourceItem`: one offered source (identity + its list entry for label and preview face).

Key functions:
- `SourceCatalog::from_entries`: filters a font list down to the allowed kinds.
- `SourceCatalog::position` / `item`: identity lookup under the one normalization rule.

Notes:
Building a catalog is cheap (entry clones, no file read); the resolver reads files lazily
on the editor's workers. Labels are NOT stored: `SourceItem::label` asks the entry at draw
time, so the bundled entry's localized name follows a UI-language switch.
*/

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::custom_fonts::sources::SourceResolver;
use crate::panel::FontEntry;
use crate::panel::fonts::normalize_font_identity;

/// Source of `SourceCatalog::serial`: one value per catalog built in this process.
static NEXT_CATALOG_SERIAL: AtomicU64 = AtomicU64::new(1);

/// One font the editor offers as a hybrid source.
#[derive(Clone)]
pub struct SourceItem {
    /// The font's render identity — what a recipe stores for it.
    identity: String,
    /// The list entry (display label, own-typeface preview).
    entry: FontEntry,
}

impl std::fmt::Debug for SourceItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceItem").field("identity", &self.identity).finish_non_exhaustive()
    }
}

impl SourceItem {
    /// The identity a recipe stores for this source.
    #[must_use]
    pub(crate) fn identity(&self) -> &str {
        &self.identity
    }

    /// The name shown to the user (localized for the bundled interface font). GUI thread.
    #[must_use]
    pub(crate) fn label(&self) -> &str {
        self.entry.display_label()
    }

    /// The list entry (own-typeface preview source, content hash, face).
    #[must_use]
    pub(crate) fn entry(&self) -> &FontEntry {
        &self.entry
    }
}

/// The fonts offered as hybrid sources plus the resolver that turns their identities into
/// bytes. Immutable once built; hosts share it as `Arc<SourceCatalog>`.
pub struct SourceCatalog {
    /// Offered sources: the bundled interface font first, then the list order; unique by
    /// normalized identity.
    items: Vec<SourceItem>,
    /// Resolves a recipe's source identities (worker threads only).
    resolver: Arc<dyn SourceResolver>,
    /// Process-unique id of this catalog (a replaced catalog re-renders the preview).
    serial: u64,
}

impl std::fmt::Debug for SourceCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceCatalog").field("items", &self.items.len()).field("serial", &self.serial).finish_non_exhaustive()
    }
}

impl SourceCatalog {
    /// Builds a catalog from a font list and the resolver over the same fonts.
    ///
    /// Keeps real font files and the bundled interface font only — custom fonts (saved or
    /// the ad-hoc entry) are never sources (decision D4) — puts the bundled font first and
    /// drops later entries whose normalized identity repeats an earlier one.
    #[must_use]
    pub(crate) fn from_entries(entries: &[FontEntry], resolver: Arc<dyn SourceResolver>) -> Self {
        let allowed = || entries.iter().filter(|entry| entry.custom_font().is_none());
        let bundled = allowed().filter(|entry| entry.bundled_stack_font().is_some());
        let files = allowed().filter(|entry| entry.bundled_stack_font().is_none());
        let mut seen = std::collections::HashSet::new();
        let items = bundled
            .chain(files)
            .filter_map(|entry| {
                let identity = entry.render_identity_name();
                seen.insert(normalize_font_identity(&identity)).then(|| SourceItem { identity, entry: entry.clone() })
            })
            .collect();
        Self { items, resolver, serial: NEXT_CATALOG_SERIAL.fetch_add(1, Ordering::Relaxed) }
    }

    /// The offered sources, in display order.
    #[must_use]
    pub(crate) fn items(&self) -> &[SourceItem] {
        &self.items
    }

    /// The resolver of this catalog's fonts (worker threads only).
    #[must_use]
    pub(crate) fn resolver(&self) -> &Arc<dyn SourceResolver> {
        &self.resolver
    }

    /// Process-unique id of this catalog.
    #[must_use]
    pub(crate) fn serial(&self) -> u64 {
        self.serial
    }

    /// Position of the source whose identity equals `identity` under the one identity
    /// normalization (`normalize_font_identity`), if offered.
    #[must_use]
    pub(crate) fn position(&self, identity: &str) -> Option<usize> {
        let wanted = normalize_font_identity(identity);
        self.items.iter().position(|item| normalize_font_identity(&item.identity) == wanted)
    }

    /// The offered source named `identity` (normalized comparison), if any.
    #[must_use]
    pub(crate) fn item(&self, identity: &str) -> Option<&SourceItem> {
        self.position(identity).and_then(|index| self.items.get(index))
    }

    /// Number of offered sources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether no source is offered (an empty font list).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom_fonts::sources::{ResolvedSource, SourceError};
    use crate::panel::custom_font_entry::{adhoc_custom_font_entry, hybrid_source_fonts};

    /// A resolver that knows nothing (catalog tests never resolve).
    struct NoFonts;

    impl SourceResolver for NoFonts {
        fn resolve_source(&self, identity: &str) -> Result<ResolvedSource, SourceError> {
            Err(SourceError::Unknown(identity.to_owned()))
        }
    }

    #[test]
    fn the_catalog_offers_the_bundled_font_first_and_never_a_custom_font() {
        let mut entries = vec![adhoc_custom_font_entry()];
        entries.extend(hybrid_source_fonts(&[]));
        // The bundled entry twice (once from the panel list, once added): listed once.
        entries.extend(hybrid_source_fonts(&[]));
        let catalog = SourceCatalog::from_entries(&entries, Arc::new(NoFonts));
        assert!(catalog.items().iter().all(|item| item.entry().custom_font().is_none()), "D4");
        if let Some(first) = catalog.items().first() {
            assert!(first.entry().bundled_stack_font().is_some());
            assert_eq!(catalog.len(), 1, "duplicates by normalized identity are dropped");
            let upper = first.identity().to_ascii_uppercase();
            assert_eq!(catalog.position(&format!("  {upper} ")), Some(0));
        }
        assert_eq!(catalog.position("No-Such-Font"), None);
        let other = SourceCatalog::from_entries(&[], Arc::new(NoFonts));
        assert!(other.is_empty());
        assert_ne!(other.serial(), catalog.serial());
    }
}
