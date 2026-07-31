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
- read font bytes lazily OUTSIDE the lock and cache the shared buffer + content id so
  a repeated resolve does not re-read the file;
- serve the synthetic BUNDLED `fonts/ui` entry from the `'static` bytes `ms-fonts`
  already holds, so the built-in font is never read a second time;
- carry each font's ORIGINAL name (real family/name) through to the renderer for
  callers that need the real identity (e.g. PSD export, future virtual fonts).

Key structures:
- `ProviderEntry`: how to obtain one font's bytes (a file path, or a bundled stack
  font whose bytes are already resident).
- `TabFontProvider`: the panel-owned `FontProvider`.

Notes:
Normalization mirrors the renderer's `normalize_inline_font_label`
(`trim().to_ascii_lowercase()`) so a name resolves identically on both sides. The
identity primary key is unique in the common case (`assign_font_identity_names`
already split a shared family into distinct file-stem identities); any residual
key collision is deterministic FIRST-wins over the font list and logged (see
`from_fonts`).

A failing resolve is never silent: an unreadable file is logged with its path and
the OS reason (once per path, since the resolve is retried on every render), and a
poisoned cache mutex is recovered and logged once instead of dropping the cache and
re-reading the file on every resolve. Only an UNKNOWN name resolves to `None`
without a log — it is not an error at this layer.
*/

use super::*;
use crate::tabs::typing::render_next::{FontBytes, FontContent, FontProvider, font_content_id};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Loaded font bytes plus their stable content id, cached per source path.
type CachedFontBytes = (FontBytes, u64);

/// One resolvable font entry: how to obtain its bytes plus the face to use and the
/// font's original name.
///
/// `bundled` is `Some` only for the synthetic built-in entry; its bytes come from
/// `ms_fonts::bytes` (read once per process, shared with the egui UI and with the
/// renderer's own font base) instead of from a second read of the same file.
#[derive(Debug, Clone)]
struct ProviderEntry {
    path: PathBuf,
    face_index: usize,
    original_name: String,
    bundled: Option<&'static ms_fonts::StackFont>,
}

/// App-side font provider: maps a working name (font label, normalized) to a font.
/// Obtains bytes lazily (outside the lock) and caches the shared buffer + content id
/// so a name resolves without re-reading. Built once per font-list revision and
/// shared (Arc) with background render threads. Future virtual fonts add synthesized
/// entries whose bytes are composed rather than read.
pub(in crate::tabs::typing) struct TabFontProvider {
    /// key = normalized name (`trim().to_ascii_lowercase()`): primarily each font's
    /// collision-aware `identity_name`, plus family-name, file-stem and label aliases
    /// (see `from_fonts`).
    by_name: HashMap<String, ProviderEntry>,
    /// path -> (bytes, content_id); populated lazily on first resolve of a font.
    cache: Mutex<HashMap<PathBuf, CachedFontBytes>>,
    /// Paths whose read already failed and was reported. A failing resolve is retried on
    /// every later resolve (the file may reappear), so without this set the same warning
    /// would be written on every render.
    reported_read_failures: Mutex<HashSet<PathBuf>>,
    /// Set once a poisoned provider mutex has been reported, so the recovery is logged
    /// once per provider instead of on every resolve.
    poison_reported: AtomicBool,
}

/// Hand-written because the cached buffers are erased (`dyn AsRef<[u8]>`, the type
/// fontdb takes) and cannot derive `Debug`. Reports sizes only — never font bytes.
impl std::fmt::Debug for TabFontProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cached = self.cache.lock().map(|cache| cache.len()).ok();
        formatter
            .debug_struct("TabFontProvider")
            .field("names", &self.by_name.len())
            .field("cached_files", &cached)
            .finish()
    }
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
            bundled: font.bundled_stack_font(),
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
            // The bundled entry deliberately claims NO stem alias: its path is a file
            // of the shipped `fonts/ui` bundle, and letting it own that stem would
            // shadow a user's own copy of the same file for stem-only lookups. It is
            // reachable by its reserved identity, which is the only name ever
            // persisted for it.
            if font.bundled_stack_font().is_some() {
                continue;
            }
            if let Some(stem) = font.path.file_stem().and_then(|s| s.to_str()) {
                by_name
                    .entry(normalize_name(stem))
                    .or_insert_with(|| entry_for(font));
            }
        }
        Self {
            by_name,
            cache: Mutex::new(HashMap::new()),
            reported_read_failures: Mutex::new(HashSet::new()),
            poison_reported: AtomicBool::new(false),
        }
    }

    /// Locks one of the provider's maps, recovering it from a poisoned mutex and
    /// reporting the poisoning ONCE per provider.
    ///
    /// The guarded sections only look up and insert into a map, so a panic elsewhere
    /// cannot leave one half-updated. Recovery is what keeps the byte cache usable:
    /// abandoning it would make every later resolve re-read the font file from disk on
    /// a background render thread, silently and forever.
    fn lock_map<'a, T>(&self, mutex: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
        match mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                if !self.poison_reported.swap(true, Ordering::Relaxed) {
                    crate::runtime_log::log_warn(format!(
                        "TabFontProvider: the {what} mutex is poisoned (a thread panicked \
                         while holding it); the map is recovered and font resolution \
                         continues with it. Reported once per provider."
                    ));
                }
                poisoned.into_inner()
            }
        }
    }

    /// Reports a font-file read failure, at most once per path for this provider.
    ///
    /// The resolve itself still returns `None` on every attempt (see `resolve`); only the
    /// log line is de-duplicated, so a font file that reappears is still picked up.
    fn report_read_failure(&self, path: &Path, error: &std::io::Error) {
        let mut reported = self.lock_map(&self.reported_read_failures, "read-failure");
        if !reported.insert(path.to_path_buf()) {
            return;
        }
        crate::runtime_log::log_warn(format!(
            "TabFontProvider: cannot read font file; operation: resolve font bytes; \
             path: '{}'; error: {error}. The name resolves to nothing, which the renderer \
             reports as a missing font. Reported once per path.",
            path.display()
        ));
    }
}

impl FontProvider for TabFontProvider {
    /// Resolves a working `name` to its content. Obtains the bytes lazily on a cache
    /// miss (never holding the lock across the read) and caches them with their
    /// content id. Returns `None` for an unknown name or an unreadable file (a
    /// missing font surfaces as a render error upstream) — the two cases are told
    /// apart in the log: an unreadable file is reported with its path and reason
    /// (once per path), an unknown name is not an error here.
    ///
    /// The synthetic BUNDLED entry takes its bytes from `ms_fonts::bytes` — the very
    /// `'static` buffer the egui UI and the renderer's font base already share — so
    /// the file is not read again AND the renderer can recognize the buffer as
    /// already registered instead of adding a duplicate face
    /// (`ms_text_render::font_base::resident_face_ids`).
    fn resolve(&self, name: &str) -> Option<FontContent> {
        let entry = self.by_name.get(&normalize_name(name))?.clone();

        // Fast path: bytes already cached. Clone the Arc + id and release the lock.
        {
            let cache = self.lock_map(&self.cache, "byte-cache");
            if let Some((data, content_id)) = cache.get(&entry.path) {
                return Some(FontContent {
                    name: name.to_string(),
                    original_name: entry.original_name,
                    data: Arc::clone(data),
                    face_index: entry.face_index,
                    content_id: *content_id,
                });
            }
        }

        // Slow path: obtain the bytes OUTSIDE the lock, then insert.
        let (data, content_id): (FontBytes, u64) = match entry.bundled {
            // `ms_fonts::bytes` logs its own failure with the path and the reason.
            Some(font) => {
                let bytes = ms_fonts::bytes(font)?;
                (Arc::new(bytes), font_content_id(bytes))
            }
            None => match std::fs::read(&entry.path) {
                Ok(bytes) => {
                    let content_id = font_content_id(&bytes);
                    (Arc::new(bytes), content_id)
                }
                Err(error) => {
                    self.report_read_failure(&entry.path, &error);
                    return None;
                }
            },
        };
        {
            let mut cache = self.lock_map(&self.cache, "byte-cache");
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
            kind: FontEntryKind::File,
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

    /// Builds the panel list the way a real reload does — user fonts finalized, then
    /// the bundled entry prepended — or `None` when this process has no `fonts/ui`
    /// stack (then there is nothing to assert).
    fn fonts_with_bundled(fonts: Vec<FontEntry>) -> Option<Vec<FontEntry>> {
        let mut fonts = fonts_with_identities(fonts);
        super::super::fonts::prepend_bundled_ui_font(&mut fonts);
        fonts
            .first()
            .and_then(FontEntry::bundled_stack_font)
            .is_some()
            .then_some(fonts)
    }

    #[test]
    fn built_in_font_resolves_by_its_reserved_identity() {
        let Some(fonts) = fonts_with_bundled(vec![font_entry(
            "myfont",
            "/fonts/myfont.ttf",
            "My Font",
        )]) else {
            eprintln!("skipping built_in_font_resolves_by_its_reserved_identity: no fonts/ui stack");
            return;
        };
        let bundled_path = fonts[0].path.clone();
        let provider = TabFontProvider::from_fonts(&fonts);

        assert_eq!(
            provider.resolved_path_for(fonts::BUNDLED_UI_FONT_IDENTITY),
            Some(bundled_path.as_path()),
            "the reserved identity must resolve to the bundled core font file"
        );
        // The bundle's file STEM is deliberately not an alias of the built-in entry.
        let stem = bundled_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        assert!(
            !stem.is_empty() && provider.resolved_path_for(stem).is_none(),
            "the built-in entry must not claim the bundled file's stem as an alias"
        );
    }

    #[test]
    fn a_user_font_claiming_the_reserved_name_is_shadowed_by_the_built_in_entry() {
        // A user font whose FAMILY is exactly the reserved name: its identity becomes
        // that name too, so both would key the same slot. The built-in entry is first
        // in the list, so it wins here — and, by the same FIRST-wins rule over the
        // same list, in the panel's `find_font_idx_by_label_norm` as well.
        let Some(fonts) = fonts_with_bundled(vec![font_entry(
            "impostor",
            "/fonts/impostor.ttf",
            fonts::BUNDLED_UI_FONT_IDENTITY,
        )]) else {
            eprintln!(
                "skipping a_user_font_claiming_the_reserved_name_is_shadowed_by_the_built_in_entry: \
                 no fonts/ui stack"
            );
            return;
        };
        let bundled_path = fonts[0].path.clone();
        let provider = TabFontProvider::from_fonts(&fonts);

        assert_eq!(
            provider.resolved_path_for(fonts::BUNDLED_UI_FONT_IDENTITY),
            Some(bundled_path.as_path()),
            "the built-in entry must keep its reserved name against a user font"
        );
        // The shadowed font stays reachable by its own label/stem.
        assert_eq!(
            provider.resolved_path_for("impostor"),
            Some(Path::new("/fonts/impostor.ttf")),
        );
    }

    /// A build WITHOUT the built-in entry (an older app version opening a project that
    /// uses it) must report the font as unknown, so the panel shows the normal
    /// "font not found" state instead of silently rendering with someone else's font.
    #[test]
    fn without_the_built_in_entry_the_reserved_name_resolves_to_nothing() {
        let fonts = fonts_with_identities(vec![
            font_entry("myfont", "/fonts/myfont.ttf", "My Font"),
            font_entry("other", "/fonts/other.ttf", "Other Family"),
        ]);
        let provider = TabFontProvider::from_fonts(&fonts);
        assert!(
            provider
                .resolved_path_for(fonts::BUNDLED_UI_FONT_IDENTITY)
                .is_none(),
            "the reserved name must not fall back to an unrelated font"
        );
    }

    /// Selecting the built-in font must NOT add a face to the render database: its
    /// bytes are the very `'static` buffer `font_base` already registered, and a
    /// second registration would put a duplicate `(family, weight, style)` face into
    /// every pooled `FontSystem`.
    #[test]
    fn the_built_in_font_registers_no_duplicate_face_in_the_renderer() {
        use crate::tabs::typing::render_next::{
            FontFaceCache, load_font_content, new_render_font_system,
        };

        let Some(fonts) = fonts_with_bundled(Vec::new()) else {
            eprintln!(
                "skipping the_built_in_font_registers_no_duplicate_face_in_the_renderer: \
                 no fonts/ui stack"
            );
            return;
        };
        let provider = TabFontProvider::from_fonts(&fonts);
        let content = provider
            .resolve(fonts::BUNDLED_UI_FONT_IDENTITY)
            .expect("the built-in font must resolve to content");

        let mut system = new_render_font_system();
        let mut cache = FontFaceCache::for_system(&system);
        let faces_before = system.db().len();
        let face = load_font_content(&mut system, &mut cache, &content, content.face_index)
            .expect("the bundled core font must load");

        assert_eq!(
            system.db().len(),
            faces_before,
            "loading the built-in font must reuse the resident face, not add one"
        );
        // It resolves to the REAL family of the core file, which is what the renderer
        // shapes with; the reserved identity is only the provider-side key.
        let core_family = fonts[0]
            .bundled_stack_font()
            .map(|font| font.family_name)
            .unwrap_or_default();
        assert_eq!(
            face.family_name.as_deref(),
            Some(core_family),
            "the selected face must be the bundled core face"
        );
    }

    /// An unreadable (here: missing) file must resolve to `None` on every attempt, cache
    /// nothing, and be reported exactly ONCE — the renderer retries the resolve for every
    /// text image, so an undeduplicated warning would flood the session log.
    #[test]
    fn an_unreadable_file_resolves_to_none_and_is_reported_once() {
        let missing = "/definitely/not/a/font/dir/ghost.ttf";
        let fonts = fonts_with_identities(vec![font_entry("ghost", missing, "Ghost Family")]);
        let provider = TabFontProvider::from_fonts(&fonts);

        for _ in 0..3 {
            assert!(
                provider.resolve("Ghost Family").is_none(),
                "an unreadable file must resolve to nothing"
            );
        }

        let reported = provider
            .reported_read_failures
            .lock()
            .expect("no test thread panics while holding this lock");
        assert_eq!(
            reported.len(),
            1,
            "the read failure must be logged once, not once per resolve"
        );
        assert!(reported.contains(&PathBuf::from(missing)));
        let cache = provider
            .cache
            .lock()
            .expect("no test thread panics while holding this lock");
        assert!(
            cache.is_empty(),
            "a failed read must not cache anything (the file may reappear)"
        );
    }

    /// A poisoned cache mutex must not disable caching: the map is recovered, so a font
    /// resolved once is not re-read from disk on every later resolve.
    #[test]
    fn a_poisoned_cache_is_recovered_instead_of_disabling_the_cache() {
        let Some(fonts) = fonts_with_bundled(Vec::new()) else {
            eprintln!(
                "skipping a_poisoned_cache_is_recovered_instead_of_disabling_the_cache: \
                 no fonts/ui stack"
            );
            return;
        };
        let provider = Arc::new(TabFontProvider::from_fonts(&fonts));

        // Poison the cache mutex the only way a mutex gets poisoned: panic while holding it.
        let poisoner = Arc::clone(&provider);
        let panicked = std::thread::spawn(move || {
            let _guard = poisoner
                .cache
                .lock()
                .expect("the lock is not poisoned yet in this test");
            panic!("deliberate panic to poison the cache mutex");
        })
        .join();
        assert!(panicked.is_err(), "the helper thread must have panicked");
        assert!(provider.cache.is_poisoned());

        let first = provider
            .resolve(fonts::BUNDLED_UI_FONT_IDENTITY)
            .expect("a poisoned cache must not break resolution");
        assert!(
            provider
                .lock_map(&provider.cache, "byte-cache")
                .contains_key(&fonts[0].path),
            "the recovered cache must still be populated"
        );
        let second = provider
            .resolve(fonts::BUNDLED_UI_FONT_IDENTITY)
            .expect("the second resolve must hit the recovered cache");
        assert_eq!(first.content_id, second.content_id);
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
