/*
File: panel/font_provider.rs

Purpose:
App-side implementation of the renderer's `FontProvider` contract. The renderer
resolves the main font (`TextRenderParams.font_name`) and inline `<font=...>` tags
by WORKING NAME through a provider; this module builds that provider from the
typing panel's font list and loads bytes lazily.

Main responsibilities:
- map a normalized working name (the font label) to a resolvable font entry;
- read font bytes lazily OUTSIDE the lock and cache `Arc<Vec<u8>>` + content id so
  a repeated resolve does not re-read the file;
- carry each font's ORIGINAL name (real family/name) through to the renderer for
  callers that need the real identity (e.g. PSD export, future virtual fonts).

Key structures:
- `ProviderEntry`: how to obtain one font's bytes (a real file path today).
- `TabFontProvider`: the panel-owned `FontProvider`.

Notes:
Normalization mirrors the renderer's `normalize_inline_font_label`
(`trim().to_ascii_lowercase()`) so a name resolves identically on both sides. On a
duplicate normalized label the LAST font wins, matching the renderer's inline
font-map `insert` behavior.
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
    /// key = normalized label (`trim().to_ascii_lowercase()`).
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
    /// Builds a provider from the panel's font list. Each font is keyed by its
    /// normalized label (the primary working name), and ADDITIONALLY aliased by its
    /// file stem and its original name so a persisted `font_name` in any of those
    /// forms still resolves. This matters for backward-compat: an old project that
    /// saved only `font_path` derives the name from the file stem (see
    /// `codec::text_render_params_from_render_data`), which for a SYSTEM font is not
    /// the label (`"{stem} [system]"`). The representative face is used.
    ///
    /// Precedence: labels are inserted first (LAST font wins on a duplicate label,
    /// matching the renderer's inline font-map behavior); stem/original-name aliases
    /// are added only for keys not already claimed by a label, so a label never
    /// loses to an alias.
    #[must_use]
    pub(in crate::tabs::typing) fn from_fonts(fonts: &[FontEntry]) -> Self {
        let mut by_name = HashMap::with_capacity(fonts.len());
        let entry_for = |font: &FontEntry| ProviderEntry {
            path: font.path.clone(),
            face_index: font.faces.first().map(|face| face.face_index).unwrap_or(0),
            original_name: font.original_name.clone(),
        };
        // Primary keys: normalized labels (last-wins on collision).
        for font in fonts {
            by_name.insert(normalize_name(&font.label), entry_for(font));
        }
        // Alias keys: file stem + original name, only when not already a label key.
        for font in fonts {
            if let Some(stem) = font.path.file_stem().and_then(|s| s.to_str()) {
                by_name
                    .entry(normalize_name(stem))
                    .or_insert_with(|| entry_for(font));
            }
            if !font.original_name.is_empty() {
                by_name
                    .entry(normalize_name(&font.original_name))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal `FontEntry` fixture for provider-key tests.
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
        }
    }

    #[test]
    fn resolves_by_label_stem_and_original_name() {
        // A system font: label is "arial [system]", stem is "arial", family is "Arial".
        let provider = TabFontProvider::from_fonts(&[font_entry(
            "arial [system]",
            "/usr/share/fonts/arial.ttf",
            "Arial",
        )]);
        // Primary key: the label. Back-compat aliases: file stem and original name.
        // An old project that saved only `font_path` derives the name from the stem,
        // which for a system font differs from the label — the alias must resolve it.
        for name in ["arial [system]", "ARIAL [System]", "arial", "Arial"] {
            assert!(
                provider.by_name.contains_key(&normalize_name(name)),
                "provider must resolve system font by '{name}'"
            );
        }
    }

    #[test]
    fn label_key_wins_over_alias_on_collision() {
        // Font A's LABEL equals font B's file stem ("beta"). The label key must point
        // to A (inserted in the label pass), not be overwritten by B's stem alias.
        let provider = TabFontProvider::from_fonts(&[
            font_entry("beta", "/fonts/a.ttf", "Alpha Family"),
            font_entry("gamma", "/fonts/beta.ttf", "Gamma Family"),
        ]);
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
