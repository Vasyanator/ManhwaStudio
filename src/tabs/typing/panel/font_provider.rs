/*
File: panel/font_provider.rs

Purpose:
App-side implementation of the renderer's `FontProvider` contract. The renderer
resolves the main font (`TextRenderParams.font_name`) and inline `<font=...>` tags
by WORKING NAME through a provider; this module builds that provider from the
typing panel's font list and loads bytes lazily.

Main responsibilities:
- map a normalized working name to a resolvable font entry, keyed PRIMARILY by each
  font's COLLISION-AWARE identity (`FontEntry.identity_name`: the family name when
  unique in the list, the file-stem label on a shared family), with the family name,
  file stem and display label kept as legacy aliases;
- read font bytes lazily OUTSIDE the lock and cache `Arc<Vec<u8>>` + content id so
  a repeated resolve does not re-read the file;
- carry each font's ORIGINAL name (real family/name) through to the renderer for
  callers that need the real identity (e.g. PSD export, future virtual fonts).

Key structures:
- `ProviderEntry`: how to obtain one font's bytes (a real file path today).
- `TabFontProvider`: the panel-owned `FontProvider`.

Notes:
Normalization mirrors the renderer's `normalize_inline_font_label`
(`trim().to_ascii_lowercase()`) so a name resolves identically on both sides. The
identity primary key is unique in the common case (`assign_font_identity_names`
already split a shared family into distinct file-stem identities); any residual
key collision is deterministic FIRST-wins over the font list and logged (see
`from_fonts`).
*/

use super::*;
use crate::tabs::typing::render_next::{FontContent, FontProvider, font_content_id};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Loaded font bytes plus their stable content id, cached per source path.
type CachedFontBytes = (Arc<Vec<u8>>, u64);

/// One resolvable font entry: how to obtain its bytes (a real file path today) plus
/// the face to use and the font's original name.
#[derive(Debug, Clone)]
struct ProviderEntry {
    path: PathBuf,
    face_index: usize,
    original_name: String,
}

/// App-side font provider: maps a working name (font label, normalized) to a font.
/// Reads bytes lazily (outside the lock), caches `Arc<Vec<u8>>` + content id so a
/// name resolves without re-reading. Built once per font-list revision and shared
/// (Arc) with background render threads. Future virtual fonts add synthesized
/// entries whose bytes are composed rather than read.
#[derive(Debug)]
pub(in crate::tabs::typing) struct TabFontProvider {
    /// key = normalized name (`trim().to_ascii_lowercase()`): primarily each font's
    /// collision-aware `identity_name`, plus family-name, file-stem and label aliases
    /// (see `from_fonts`).
    by_name: HashMap<String, ProviderEntry>,
    /// path -> (bytes, content_id); populated lazily on first resolve of a font.
    cache: Mutex<HashMap<PathBuf, CachedFontBytes>>,
}

/// Normalizes a working font name for lookup, mirroring the renderer's
/// `normalize_inline_font_label` so both sides key on the same string.
fn normalize_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

impl TabFontProvider {
    /// Builds a provider from the panel's font list. The PRIMARY key is each font's
    /// normalized COLLISION-AWARE identity (`FontEntry.identity_name`) — the canonical
    /// render identity persisted in `render_data`/`TextRenderParams.font_name` and
    /// emitted in `<font=...>` tags. The original family name, file stem and display
    /// LABEL are kept as ALIAS keys so a persisted `font_name` in any legacy form still
    /// resolves: a blob written with a (now-colliding) family name, an old project that
    /// saved only `font_path` and derives the name from the file stem (see
    /// `codec::text_render_params_from_render_data`), and older inline tags using the
    /// label/stem (`"{stem} [system]"` for a system font). The representative face is
    /// used.
    ///
    /// The user display-name OVERRIDE (`display_label`) is never a key — it is a
    /// presentation-only rename and must not affect resolution.
    ///
    /// Precedence and collisions:
    /// - Identities are inserted FIRST with deterministic FIRST-wins over the given
    ///   font order. `assign_font_identity_names` already gives a shared family two
    ///   distinct file-stem identities, so a residual identity collision is rare; when
    ///   it happens the first font in the list claims the key and it is logged (naming
    ///   both paths). Renderer resolution is name-only BY DESIGN — exact-file selection
    ///   is a panel concern (path-first), not the provider's.
    /// - The family-name alias is inserted next (FIRST-wins via `or_insert`), so a blob
    ///   still carrying a colliding family name resolves to the first font that declares
    ///   it. Label and stem aliases follow, each only for keys not already claimed, so
    ///   an identity/family key never loses to a weaker alias.
    #[must_use]
    pub(in crate::tabs::typing) fn from_fonts(fonts: &[FontEntry]) -> Self {
        let mut by_name = HashMap::with_capacity(fonts.len());
        let entry_for = |font: &FontEntry| ProviderEntry {
            path: font.path.clone(),
            face_index: font.faces.first().map(|face| face.face_index).unwrap_or(0),
            original_name: font.original_name.clone(),
        };
        // Primary keys: normalized collision-aware identities, FIRST-wins on collision.
        for font in fonts {
            let identity = font.identity_name.trim();
            if identity.is_empty() {
                continue;
            }
            let key = normalize_name(identity);
            match by_name.entry(key) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(entry_for(font));
                }
                std::collections::hash_map::Entry::Occupied(existing) => {
                    // Two entries resolved to the same identity (should be rare after
                    // `assign_font_identity_names`): keep the first and warn.
                    if existing.get().path != font.path {
                        crate::runtime_log::log_warn(format!(
                            "TabFontProvider: render identity '{}' is shared by two files; \
                             resolving to '{}' and shadowing '{}'. The shadowed font \
                             stays reachable only by its file-stem/label alias.",
                            identity,
                            existing.get().path.display(),
                            font.path.display(),
                        ));
                    }
                }
            }
        }
        // Family-name alias first (FIRST-wins), so a blob persisted with a family name
        // that later became a collision still resolves to the first declaring font —
        // matching the panel's whole-list first-match resolution.
        for font in fonts {
            let original = font.original_name.trim();
            if !original.is_empty() {
                by_name
                    .entry(normalize_name(original))
                    .or_insert_with(|| entry_for(font));
            }
        }
        // Label/stem aliases, only for keys not already claimed (so an identity or
        // family key never loses to an alias). Labels before stems so a label beats
        // another font's identical stem, matching the historical alias precedence.
        for font in fonts {
            by_name
                .entry(normalize_name(&font.label))
                .or_insert_with(|| entry_for(font));
        }
        for font in fonts {
            if let Some(stem) = font.path.file_stem().and_then(|s| s.to_str()) {
                by_name
                    .entry(normalize_name(stem))
                    .or_insert_with(|| entry_for(font));
            }
        }
        Self {
            by_name,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl FontProvider for TabFontProvider {
    /// Resolves a working `name` to its content. Reads the backing file lazily on a
    /// cache miss (never holding the lock across the read) and caches the bytes +
    /// content id. Returns `None` for an unknown name or an unreadable file (a
    /// missing font surfaces as a render error upstream).
    fn resolve(&self, name: &str) -> Option<FontContent> {
        let entry = self.by_name.get(&normalize_name(name))?.clone();

        // Fast path: bytes already cached. Clone the Arc + id and release the lock.
        if let Ok(cache) = self.cache.lock()
            && let Some((data, content_id)) = cache.get(&entry.path)
        {
            return Some(FontContent {
                name: name.to_string(),
                original_name: entry.original_name,
                data: Arc::clone(data),
                face_index: entry.face_index,
                content_id: *content_id,
            });
        }

        // Slow path: read the file OUTSIDE the lock, then insert.
        let bytes = std::fs::read(&entry.path).ok()?;
        let content_id = font_content_id(&bytes);
        let data = Arc::new(bytes);
        if let Ok(mut cache) = self.cache.lock() {
            cache
                .entry(entry.path.clone())
                .or_insert_with(|| (Arc::clone(&data), content_id));
        }
        Some(FontContent {
            name: name.to_string(),
            original_name: entry.original_name,
            data,
            face_index: entry.face_index,
            content_id,
        })
    }
}

/// Test-only introspection: the source path a normalized `name` currently resolves to,
/// WITHOUT reading the backing file (so fixtures with non-existent paths are testable).
/// Lets sibling panel tests assert that a name resolves to the SAME font the panel picks.
#[cfg(test)]
impl TabFontProvider {
    pub(in crate::tabs::typing::panel) fn resolved_path_for(&self, name: &str) -> Option<&Path> {
        self.by_name
            .get(&normalize_name(name))
            .map(|entry| entry.path.as_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal `FontEntry` fixture for provider-key tests, with the per-entry
    /// default identity (as `assign_font_identity_names` would leave a unique family).
    fn font_entry(label: &str, path: &str, original_name: &str) -> FontEntry {
        FontEntry {
            label: label.to_string(),
            path: PathBuf::from(path),
            alt_paths: Vec::new(),
            groups: vec![None],
            disambig: None,
            faces: vec![FontFaceEntry {
                label: "Face 0".to_string(),
                face_index: 0,
            }],
            coverage: FontLanguageCoverage::default(),
            original_name: original_name.to_string(),
            display_name: None,
            identity_name: super::super::fonts::default_font_identity_name(original_name, label),
            virtual_group_aliases: std::collections::BTreeMap::new(),
        }
    }

    /// Builds a font list and runs the collision-aware identity assignment on it, so
    /// provider tests key on the SAME identities the panel does.
    fn fonts_with_identities(fonts: Vec<FontEntry>) -> Vec<FontEntry> {
        let mut fonts = fonts;
        super::super::fonts::assign_font_identity_names(&mut fonts);
        fonts
    }

    #[test]
    fn resolves_by_original_name_label_and_stem() {
        // A system font: label is "arial [system]", stem is "arial", family is "Arial".
        let fonts = fonts_with_identities(vec![font_entry(
            "arial [system]",
            "/usr/share/fonts/arial.ttf",
            "Arial",
        )]);
        let provider = TabFontProvider::from_fonts(&fonts);
        // Primary key: the identity (unique family "Arial"). Back-compat aliases: the
        // family name, display label and file stem all still resolve.
        for name in ["Arial", "arial", "arial [system]", "ARIAL [System]"] {
            assert!(
                provider.by_name.contains_key(&normalize_name(name)),
                "provider must resolve font by '{name}'"
            );
        }
    }

    #[test]
    fn family_name_is_primary_key() {
        // With a unique family the identity IS the family name; it maps to THIS font.
        let fonts = fonts_with_identities(vec![font_entry(
            "основной",
            "/fonts/основной.ttf",
            "Anime Ace v05",
        )]);
        let provider = TabFontProvider::from_fonts(&fonts);
        let entry = provider
            .by_name
            .get(&normalize_name("Anime Ace v05"))
            .expect("family name must be a key");
        assert_eq!(entry.path, PathBuf::from("/fonts/основной.ttf"));
        // The legacy stem/label alias still resolves to the same font.
        assert_eq!(
            provider.by_name.get(&normalize_name("основной")).map(|e| &e.path),
            Some(&PathBuf::from("/fonts/основной.ttf"))
        );
    }

    #[test]
    fn shared_family_pair_each_file_resolves_to_itself() {
        // Regular + Bold shipped as separate files share one family name. After the
        // collision-aware assignment each keeps its OWN file-stem identity, so each
        // renders itself (no silent swap); the family-name alias falls to the first.
        let fonts = fonts_with_identities(vec![
            font_entry("myfont-regular", "/fonts/regular.ttf", "MyFont"),
            font_entry("myfont-bold", "/fonts/bold.ttf", "MyFont"),
        ]);
        let provider = TabFontProvider::from_fonts(&fonts);
        // Each file resolves to ITSELF by its own identity (the file-stem label).
        assert_eq!(
            provider.by_name.get(&normalize_name("myfont-regular")).map(|e| &e.path),
            Some(&PathBuf::from("/fonts/regular.ttf")),
            "the Regular file resolves to itself"
        );
        assert_eq!(
            provider.by_name.get(&normalize_name("myfont-bold")).map(|e| &e.path),
            Some(&PathBuf::from("/fonts/bold.ttf")),
            "the Bold file resolves to itself"
        );
        // The shared family alias falls, FIRST-wins, to the first font in the list.
        assert_eq!(
            provider.by_name.get(&normalize_name("MyFont")).map(|e| &e.path),
            Some(&PathBuf::from("/fonts/regular.ttf")),
            "a blob still naming the family resolves to the first declaring font"
        );
    }

    #[test]
    fn display_name_override_is_never_a_key() {
        let mut entry = font_entry("basic", "/fonts/basic.ttf", "Basic Family");
        entry.display_name = Some("My Pretty Name".to_string());
        let fonts = fonts_with_identities(vec![entry]);
        let provider = TabFontProvider::from_fonts(&fonts);
        assert!(
            !provider.by_name.contains_key(&normalize_name("My Pretty Name")),
            "a display-name override is presentation-only and must not be a resolution key"
        );
        // The real identity + aliases still resolve.
        for name in ["Basic Family", "basic"] {
            assert!(provider.by_name.contains_key(&normalize_name(name)));
        }
    }

    #[test]
    fn label_key_wins_over_alias_on_collision() {
        // Font A's LABEL equals font B's file stem ("beta"). The label key must point
        // to A (inserted in the label pass), not be overwritten by B's stem alias.
        // A's identity is its unique family "Alpha Family", so "beta" is only A's label.
        let fonts = fonts_with_identities(vec![
            font_entry("beta", "/fonts/a.ttf", "Alpha Family"),
            font_entry("gamma", "/fonts/beta.ttf", "Gamma Family"),
        ]);
        let provider = TabFontProvider::from_fonts(&fonts);
        let entry = provider
            .by_name
            .get(&normalize_name("beta"))
            .expect("'beta' must resolve");
        assert_eq!(
            entry.path,
            PathBuf::from("/fonts/a.ttf"),
            "a label must win over another font's stem alias"
        );
    }
}
