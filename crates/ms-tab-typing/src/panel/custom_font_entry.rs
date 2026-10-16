/*
File: panel/custom_font_entry.rs

Purpose:
Custom fonts as entries of the typing font model (`dev-docs/hybrid_fonts_plan.md` §3.2):
how a saved recipe becomes a `FontEntry` at the end of the combined list build, the one
panel-only ad-hoc entry, how a custom entry's bytes are produced, and what its
own-typeface preview draws from.

Key structures:
- `CustomFontRow` / `CustomFontUnavailable`: one row per SAVED custom font, available or
  not (a recipe whose sources are missing, a kind this build cannot compile, a damaged
  recipe), so the settings list can show and delete every stored font.

Key functions:
- `build_custom_font_entries`: records + finalized real fonts -> entries + rows (worker
  thread: reads and hashes the source fonts, classifies language coverage).
- `hybrid_source_fonts` / `source_only_resolver`: which fonts may be a custom font's source
  (bundled interface font first, then the real fonts; never a custom font, decision D4) and
  the resolver over them — shared by the list build and the hybrid editor's catalog.
- `adhoc_custom_font_entry`: the synthetic ad-hoc entry (`MSCustom-adhoc-hybrid`).
- `resolve_recipe_sources`: recipe source identities -> `ResolvedSource`s through a
  `SourceResolver` (worker thread).
- `CustomFontRef::full_face_blocking`: the compiled face of a saved entry (worker thread;
  may compile through `custom_fonts::service`).
- `FontPreviewSource::request_family`: the per-frame own-typeface preview lifecycle for a
  file or a compiled custom face (GUI thread; never compiles, never reads a file).
- `pin_custom_face_display` / `CustomFaceDisplayPin`: freezes the face every preview of one
  saved font draws in while its recipe is being tuned (bounds egui font registrations).

Notes:
- Only AVAILABLE saved fonts become entries; every record becomes a row. A saved font's
  sources are resolved against the real fonts + the bundled interface font only (decision
  D4: a custom font is never a source), through a source-only `TabFontProvider` the entry
  keeps, so its bytes can be produced later without the panel list.
- Language coverage of a saved custom entry is classified like a file's
  (`font_coverage`), over the characters its compiled face WILL map: the compiler answers
  that itself (`ms_custom_fonts::included_chars`, its own missing-glyph rule), so nothing
  here re-derives decision D9 and nothing is compiled. A failure is logged and reported as
  `Full` (the "never a false warning" default). The ad-hoc entry is `Full` (no recipe).
- Previews: egui cannot unload a font, so only the compiled FULL face of a SAVED font is
  ever registered (keyed by identity + a hash of its recipe key); the ad-hoc entry always
  draws in the interface font. A face is registered once per (identity, recipe key)
  (`ms_widgets` memoizes registrations and failures). While a recipe is being TUNED (the
  settings properties window of that font is open) the identity is PINNED to the key it was
  opened with (`pin_custom_face_display`): every own-typeface draw of that font keeps that
  face, so no Apply registers a new face. Worst case: one new registration per saved font
  per properties-window session that changed its recipe (the committed face, shown once the
  window closes), never one per Apply.
*/

use super::*;
use crate::custom_fonts::identity::ADHOC_CUSTOM_FONT_IDENTITY;
use crate::custom_fonts::service::{CustomFontError, FaceResult, service};
use crate::custom_fonts::sources::{ResolvedSource, SourceError};
use crate::panel::fonts_data::{CustomFontBody, CustomFontRecord, UnsupportedCustomFont};
use std::sync::{Mutex, OnceLock, PoisonError};

/// How long after a disk-cache load request for a compiled face a preview may ask again.
///
/// `CustomFontService::request_disk_load` spawns a worker per request that finds nothing in
/// memory, so asking every frame for a face that is not cached would start a thread per frame
/// per visible row. Once per interval is enough: a face compiled meanwhile by a render lands in
/// memory anyway (`peek_full` sees it without any request).
const DISK_LOAD_RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// Why a saved custom font has no usable entry this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CustomFontUnavailable {
    /// These source identities are not installed or could not be read.
    MissingSources(Vec<String>),
    /// The stored kind is not one this build implements (carries its tag).
    UnsupportedKind(String),
    /// The stored recipe is damaged (carries the log-ready reason).
    InvalidRecipe(String),
}

/// One saved custom font as the font model sees it: what the document records plus its
/// entry when it can be used. `FontEntry` is not `Debug`, so neither is this.
pub(crate) struct CustomFontRow {
    /// The saved identity (`MSCustom-<16 hex>`).
    pub(crate) identity: String,
    /// The stored, user-visible name.
    pub(crate) name: String,
    /// The list entry, `None` exactly when `unavailable` is `Some`.
    pub(crate) entry: Option<FontEntry>,
    /// Why the font cannot be used this run.
    pub(crate) unavailable: Option<CustomFontUnavailable>,
}

/// Hand-written: the source resolver is a trait object.
impl std::fmt::Debug for CustomFontRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomFontRef")
            .field("origin", &self.origin)
            .field("key", &self.key.as_ref().map(RecipeKey::short8))
            .field("has_recipe", &self.recipe.is_some())
            .finish_non_exhaustive()
    }
}

impl CustomFontRef {
    /// Whether this is the panel's ad-hoc entry.
    #[must_use]
    pub(crate) fn is_adhoc(&self) -> bool {
        self.origin == CustomFontOrigin::AdHoc
    }

    /// The compiled-face key of a saved font as of the list build (`None` for ad-hoc).
    #[must_use]
    pub(crate) fn saved_key(&self) -> Option<RecipeKey> {
        self.key
    }

    /// The compiled FULL face of a saved font: sources resolved through the entry's own
    /// source provider, then memory / disk cache / compile in the compile service.
    /// `None` for the ad-hoc entry, which has no recipe of its own.
    ///
    /// BLOCKS (file reads, hashing, possibly a compile of seconds): worker threads only.
    #[must_use]
    pub(crate) fn full_face_blocking(&self) -> Option<FaceResult> {
        let recipe = self.recipe.as_ref()?;
        let sources = self.sources.as_ref()?;
        Some(
            resolve_recipe_sources(recipe, sources.as_ref())
                .map_err(CustomFontError::from)
                .and_then(|resolved| service().full_face_blocking(recipe, &resolved)),
        )
    }
}

/// Resolves every source identity of `recipe`, in recipe order, through `resolver`.
/// Blocks (it may read font files): worker threads only.
///
/// # Errors
/// The first source that is unknown, a custom font or unreadable.
pub(crate) fn resolve_recipe_sources(
    recipe: &CustomFontRecipe,
    resolver: &dyn SourceResolver,
) -> Result<Vec<ResolvedSource>, SourceError> {
    recipe.source_identities().into_iter().map(|identity| resolver.resolve_source(identity)).collect()
}

/// The 64-bit preview / registration discriminant of a compiled face: the first 8 bytes of
/// its recipe key (a SHA-256, so it changes with every recipe or source change).
#[must_use]
fn key_content_hash(key: &RecipeKey) -> u64 {
    let mut head = [0_u8; 8];
    head.copy_from_slice(&key.0[..8]);
    u64::from_be_bytes(head)
}

/// A custom `FontEntry` shell: no file, one face, fixed identity, the given language
/// coverage (see the file header).
fn custom_entry(
    custom: CustomFontRef,
    identity: &str,
    label: &str,
    content_hash: u64,
    custom_kerning: Vec<fonts_data::CustomKerningPair>,
    coverage: FontLanguageCoverage,
) -> FontEntry {
    FontEntry {
        kind: FontEntryKind::Custom(Arc::new(custom)),
        label: label.to_owned(),
        // No file stands behind a custom font; `file_path()` answers location questions
        // only, and an empty path belongs to no folder.
        path: PathBuf::new(),
        alt_paths: Vec::new(),
        groups: vec![None],
        disambig: None,
        faces: default_single_face(),
        coverage,
        // The identity doubles as the "family" alias, so no extra name resolves to it.
        original_name: identity.to_owned(),
        // Empty: the entry is not one parsed face; its identity is minted, not read.
        post_script_name: String::new(),
        content_hash,
        display_name: None,
        identity_name: identity.to_owned(),
        custom_kerning,
        virtual_group_aliases: BTreeMap::new(),
    }
}

/// The panel-only AD-HOC custom entry: identity `MSCustom-adhoc-hybrid`, label localized at
/// display time (`FontEntry::display_label`), no recipe, no bytes. Selecting it makes the
/// panel's own recipe (`custom_font_select`) the font.
#[must_use]
pub(crate) fn adhoc_custom_font_entry() -> FontEntry {
    custom_entry(
        CustomFontRef { origin: CustomFontOrigin::AdHoc, recipe: None, key: None, sources: None },
        ADHOC_CUSTOM_FONT_IDENTITY,
        ADHOC_CUSTOM_FONT_IDENTITY,
        0,
        Vec::new(),
        FontLanguageCoverage::default(),
    )
}

/// The fonts a custom font may take as sources, built from a finalized real-font list:
/// the bundled interface font first (it wins its reserved names, as in the panel), then
/// every non-custom entry of `real` (a custom one is skipped, decision D4). Cheap clones.
#[must_use]
pub(crate) fn hybrid_source_fonts(real: &[FontEntry]) -> Vec<FontEntry> {
    let mut source_fonts: Vec<FontEntry> = Vec::with_capacity(real.len() + 1);
    source_fonts.extend(bundled_ui_font_entry());
    source_fonts.extend(real.iter().filter(|font| font.custom_font().is_none()).cloned());
    source_fonts
}

/// A source-only resolver over `source_fonts` (see [`hybrid_source_fonts`]): it never
/// holds a custom font, so a recipe naming one is refused by construction (decision D4).
/// Cheap (no font file is read until a source is resolved, on a worker).
#[must_use]
pub(crate) fn source_only_resolver(source_fonts: &[FontEntry]) -> Arc<dyn SourceResolver> {
    Arc::new(TabFontProvider::from_fonts(source_fonts))
}

/// Builds the entries of the AVAILABLE saved custom fonts plus one row per record.
///
/// `real` is the finalized combined list of real fonts (identities, overrides and kerning
/// applied, no custom entry); the bundled interface font is added as a possible source.
/// Every source of a recipe is resolved (read + hashed) here, so the key of each compiled
/// face is known when the list lands. BLOCKS: run on the list-build worker.
pub(crate) fn build_custom_font_entries(
    real: &[FontEntry],
    records: &[(String, CustomFontRecord)],
) -> (Vec<FontEntry>, Vec<CustomFontRow>) {
    if records.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let resolver = source_only_resolver(&hybrid_source_fonts(real));

    let mut entries = Vec::new();
    let mut rows = Vec::with_capacity(records.len());
    for (identity, record) in records {
        let (entry, unavailable) = match &record.body {
            CustomFontBody::Unsupported { reason: UnsupportedCustomFont::UnknownKind(kind), .. } => {
                (None, Some(CustomFontUnavailable::UnsupportedKind(kind.clone())))
            }
            CustomFontBody::Unsupported { reason: UnsupportedCustomFont::Invalid(reason), .. } => {
                (None, Some(CustomFontUnavailable::InvalidRecipe(reason.clone())))
            }
            // A duplicate spelling of another entry's identity is unusable settings: greyed
            // and deletable like a damaged recipe (the decode already logged it).
            CustomFontBody::Unsupported { reason: UnsupportedCustomFont::IdentityCollision { kept }, .. } => {
                (None, Some(CustomFontUnavailable::InvalidRecipe(format!("the identity duplicates the custom font '{kept}'"))))
            }
            CustomFontBody::Known(recipe) => match saved_entry(identity, &record.name, recipe, &resolver) {
                Ok(entry) => (Some(entry), None),
                Err(missing) => (None, Some(CustomFontUnavailable::MissingSources(missing))),
            },
        };
        if let Some(entry) = &entry {
            entries.push(entry.clone());
        }
        rows.push(CustomFontRow { identity: identity.clone(), name: record.name.clone(), entry, unavailable });
    }
    (entries, rows)
}

/// The entry of one saved recipe, or the identities of the sources that could not be used
/// (every one of them, not just the first, so the row can name them all).
fn saved_entry(
    identity: &str,
    name: &str,
    recipe: &Arc<CustomFontRecipe>,
    resolver: &Arc<dyn SourceResolver>,
) -> Result<FontEntry, Vec<String>> {
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for source in recipe.source_identities() {
        match resolver.resolve_source(source) {
            Ok(found) => resolved.push(found),
            Err(err) => {
                ms_log::runtime_log::log_warn(format!(
                    "typing fonts: custom font '{name}' ({identity}) is unavailable: {err}"
                ));
                missing.push(source.to_owned());
            }
        }
    }
    if !missing.is_empty() {
        return Err(missing);
    }
    let key = service().recipe_key(recipe, &resolved);
    let coverage = recipe_language_coverage(recipe, &resolved).unwrap_or_else(|err| {
        ms_log::runtime_log::log_warn(format!(
            "typing fonts: language coverage of custom font '{name}' ({identity}) could not be \
             computed ({err}); it is shown as fully covering the typesetting language"
        ));
        FontLanguageCoverage::default()
    });
    let custom = CustomFontRef {
        origin: CustomFontOrigin::Saved,
        recipe: Some(Arc::clone(recipe)),
        key: Some(key),
        sources: Some(Arc::clone(resolver)),
    };
    // The font's OWN user kerning pairs (keyed by its saved identity), applied by the
    // provider on top of the inherited pairs baked into the compiled face.
    let kerning = font_settings_store::font_custom_kerning(identity);
    Ok(custom_entry(custom, identity, name, key_content_hash(&key), kerning, coverage))
}

/// How well the face `recipe` compiles to over `sources` covers the current typesetting
/// language. See [`recipe_language_coverage_for`]. BLOCKS: worker threads only.
///
/// # Errors
/// As [`recipe_language_coverage_for`].
fn recipe_language_coverage(
    recipe: &CustomFontRecipe,
    sources: &[ResolvedSource],
) -> Result<FontLanguageCoverage, CustomFontError> {
    recipe_language_coverage_for(recipe, sources, ms_text_util::language::text_language())
}

/// How well the face `recipe` compiles to over `sources` covers `language`: the compiler
/// says which of the language's characters the face will map (cmap lookups only, no
/// outlines), and `font_coverage` classifies that set exactly as it classifies a font file.
/// BLOCKS (parses the sources): worker threads only.
///
/// # Errors
/// A source whose face index does not fit, or a source / recipe the compiler rejects.
fn recipe_language_coverage_for(
    recipe: &CustomFontRecipe,
    sources: &[ResolvedSource],
    language: ms_text_util::language::TextLanguage,
) -> Result<FontLanguageCoverage, CustomFontError> {
    let candidates = font_coverage::required_chars_for(language);
    let views = sources.iter().map(ResolvedSource::as_source_font).collect::<Result<Vec<_>, _>>()?;
    let included = ms_custom_fonts::included_chars(recipe, &views, &candidates)?;
    Ok(font_coverage::classify_char_set_for(&included, language))
}

/// Process-global memo of the disk-cache loads previews asked for: key -> when.
fn disk_load_requests() -> &'static Mutex<HashMap<RecipeKey, Instant>> {
    static REQUESTS: OnceLock<Mutex<HashMap<RecipeKey, Instant>>> = OnceLock::new();
    REQUESTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Asks the compile service to load `key` from the disk cache, at most once per
/// [`DISK_LOAD_RETRY_INTERVAL`]. GUI-safe (map lookup + a worker spawn).
fn request_disk_load_throttled(key: RecipeKey) {
    {
        // A poisoned memo only means a panic mid-insert of a timestamp; the map is still a
        // valid "asked at" table.
        let mut requests = disk_load_requests().lock().unwrap_or_else(PoisonError::into_inner);
        if requests.get(&key).is_some_and(|asked| asked.elapsed() < DISK_LOAD_RETRY_INTERVAL) {
            return;
        }
        requests.insert(key, Instant::now());
    }
    service().request_disk_load(key);
}

/// Process-global display pins: normalized saved identity -> (the pinned recipe key, number
/// of live [`CustomFaceDisplayPin`]s holding it).
fn display_pins() -> &'static Mutex<HashMap<String, (RecipeKey, usize)>> {
    static PINS: OnceLock<Mutex<HashMap<String, (RecipeKey, usize)>>> = OnceLock::new();
    PINS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The pin table; a poisoned lock is recovered because every critical section leaves the map
/// consistent (single insert / counter update / remove).
fn lock_display_pins() -> std::sync::MutexGuard<'static, HashMap<String, (RecipeKey, usize)>> {
    display_pins().lock().unwrap_or_else(PoisonError::into_inner)
}

/// The key every own-typeface preview of `font_identity` must draw while it is pinned, or
/// `None` when it is not.
fn pinned_display_key(font_identity: &str) -> Option<RecipeKey> {
    lock_display_pins().get(&fonts::normalize_font_identity(font_identity)).map(|(key, _)| *key)
}

/// Holds one saved custom font's own-typeface previews on ONE compiled face (see
/// [`pin_custom_face_display`]); dropping it releases the pin.
#[derive(Debug)]
pub struct CustomFaceDisplayPin {
    /// The normalized saved identity this pin holds.
    identity: String,
}

impl Drop for CustomFaceDisplayPin {
    fn drop(&mut self) {
        let mut pins = lock_display_pins();
        if let Some((_, holders)) = pins.get_mut(&self.identity) {
            *holders = holders.saturating_sub(1);
            if *holders == 0 {
                pins.remove(&self.identity);
            }
        }
    }
}

/// Pins the own-typeface previews of the SAVED custom font `font` to the face of its current
/// recipe key until the returned guard drops: while pinned, [`FontPreviewSource::request_family`]
/// draws that face for every later key of the same identity (rows, group members, combos,
/// glyph grids), so recipe edits register no new egui font (egui cannot unload one).
///
/// The settings properties window holds it while it is open. `None` for any other font (a
/// file, the bundled font, the ad-hoc entry: nothing to bound). A second pin of the same
/// identity keeps the FIRST key (the face already on screen). Cheap; GUI-thread safe.
#[must_use]
pub fn pin_custom_face_display(font: &FontEntry) -> Option<CustomFaceDisplayPin> {
    let key = font.custom_font()?.saved_key()?;
    let identity = fonts::normalize_font_identity(&font.render_identity_name());
    let mut pins = lock_display_pins();
    pins.entry(identity.clone()).and_modify(|(_, holders)| *holders = holders.saturating_add(1)).or_insert((key, 1));
    Some(CustomFaceDisplayPin { identity })
}

impl FontPreviewSource {
    /// One frame of the own-typeface preview of the font named `font_identity` (bytes
    /// discriminated by `content_hash`, face `face_index`).
    ///
    /// - `File`: `ms_widgets::request_font_family` (file read off the GUI thread).
    /// - `CompiledCustom`: registered from the compile service's memory when the face is
    ///   there (`ms_widgets::request_font_family_from_bytes`); otherwise a throttled
    ///   disk-cache load is requested and the row draws in the interface font meanwhile
    ///   (`Pending`, no repaint is scheduled: the face appears on a later frame). Never
    ///   compiles. While `font_identity` is pinned (`pin_custom_face_display`) the pinned key
    ///   and its content hash replace `key` / `content_hash`: a recipe being tuned registers
    ///   no new face.
    /// - `InterfaceFont`: `Unavailable` — always the interface font.
    ///
    /// GUI thread, inside a frame.
    #[must_use]
    pub fn request_family(
        &self,
        ctx: &egui::Context,
        font_identity: &str,
        content_hash: u64,
        face_index: usize,
    ) -> ms_widgets::PreviewFontFamily {
        match self {
            Self::File(path) => ms_widgets::request_font_family(ctx, font_identity, content_hash, path, face_index),
            Self::CompiledCustom(listed_key) => {
                let (key, content_hash) = match pinned_display_key(font_identity) {
                    Some(pinned) => (pinned, key_content_hash(&pinned)),
                    None => (*listed_key, content_hash),
                };
                match service().peek_full(&key) {
                    Some(face) => ms_widgets::request_font_family_from_bytes(
                        ctx,
                        font_identity,
                        content_hash,
                        face_index,
                        &face.bytes,
                    ),
                    None => {
                        request_disk_load_throttled(key);
                        ms_widgets::PreviewFontFamily::Pending
                    }
                }
            }
            Self::InterfaceFont => ms_widgets::PreviewFontFamily::Unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom_fonts::identity::new_saved_identity;
    use ms_custom_fonts::{CustomFontKind, HybridParams, HybridSource, MissingGlyphPolicy};

    /// A real-file entry fixture with an explicit identity and path.
    fn file_entry(identity: &str, path: &Path) -> FontEntry {
        FontEntry {
            kind: FontEntryKind::File,
            label: identity.to_owned(),
            path: path.to_path_buf(),
            alt_paths: Vec::new(),
            groups: vec![None],
            disambig: None,
            faces: default_single_face(),
            coverage: FontLanguageCoverage::default(),
            original_name: identity.to_owned(),
            post_script_name: identity.to_owned(),
            content_hash: 7,
            display_name: None,
            identity_name: identity.to_owned(),
            custom_kerning: Vec::new(),
            virtual_group_aliases: BTreeMap::new(),
        }
    }

    fn hybrid(a: &str, b: &str) -> Arc<CustomFontRecipe> {
        Arc::new(CustomFontRecipe {
            kind: CustomFontKind::Hybrid(HybridParams {
                sources: vec![
                    HybridSource { identity: a.to_owned(), weight: 1.0, missing_override: None },
                    HybridSource { identity: b.to_owned(), weight: 1.0, missing_override: None },
                ],
                missing_default: MissingGlyphPolicy::Disable,
                algorithm: ms_custom_fonts::BlendAlgorithm::Auto,
                property_weights: None,
            }),
        })
    }

    fn record(name: &str, body: CustomFontBody) -> CustomFontRecord {
        CustomFontRecord { name: name.to_owned(), body }
    }

    /// A unique scratch dir holding two readable (not necessarily valid) "font" files.
    fn scratch_fonts(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ms-custom-entry-{}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir must be creatable");
        let a = dir.join("a.ttf");
        let b = dir.join("b.ttf");
        fs::write(&a, b"font a bytes").expect("fixture must be writable");
        fs::write(&b, b"font b bytes").expect("fixture must be writable");
        (dir, a, b)
    }

    #[test]
    fn rows_cover_every_record_and_only_available_fonts_become_entries() {
        let _lock = font_settings_store::test_lock();
        font_settings_store::test_reset();
        let (dir, a, b) = scratch_fonts("rows");
        let real = vec![file_entry("Alpha-Regular", &a), file_entry("Beta-Regular", &b)];
        let available = new_saved_identity();
        let missing = new_saved_identity();
        let records = vec![
            (available.clone(), record("Гибрид", CustomFontBody::Known(hybrid("Alpha-Regular", "beta-regular")))),
            (missing.clone(), record("Сирота", CustomFontBody::Known(hybrid("Alpha-Regular", "Gone-Regular")))),
            (
                "MSCustom-00000000000000aa".to_owned(),
                record(
                    "Новый",
                    CustomFontBody::Unsupported {
                        raw: serde_json::json!({"kind": "combined"}),
                        reason: UnsupportedCustomFont::UnknownKind("combined".to_owned()),
                    },
                ),
            ),
            (
                "MSCustom-00000000000000bb".to_owned(),
                record(
                    "Сломан",
                    CustomFontBody::Unsupported {
                        raw: serde_json::json!({"kind": "hybrid"}),
                        reason: UnsupportedCustomFont::Invalid("bad".to_owned()),
                    },
                ),
            ),
        ];

        let (entries, rows) = build_custom_font_entries(&real, &records);

        assert_eq!(entries.len(), 1, "only the font whose sources all resolve is an entry");
        let entry = &entries[0];
        assert_eq!(entry.render_identity_name(), available);
        assert_eq!(entry.display_label(), "Гибрид");
        let custom = entry.custom_font().expect("a custom entry");
        assert!(!custom.is_adhoc());
        let key = custom.saved_key().expect("a saved entry carries its key");
        assert_eq!(entry.content_hash(), key_content_hash(&key));
        assert_eq!(
            entry.byte_source().preview_source(),
            FontPreviewSource::CompiledCustom(key),
            "a saved font previews from its compiled face"
        );

        assert_eq!(rows.len(), 4);
        assert!(rows[0].entry.is_some() && rows[0].unavailable.is_none());
        assert_eq!(rows[1].unavailable, Some(CustomFontUnavailable::MissingSources(vec!["Gone-Regular".to_owned()])));
        assert_eq!(rows[2].unavailable, Some(CustomFontUnavailable::UnsupportedKind("combined".to_owned())));
        assert_eq!(rows[3].unavailable, Some(CustomFontUnavailable::InvalidRecipe("bad".to_owned())));
        assert_eq!(rows[1].identity, missing);

        let _ = fs::remove_dir_all(&dir);
    }

    /// A saved custom entry under `identity` whose compiled face has `key` (no sources needed:
    /// the pin reads only the identity and the key).
    fn saved_entry_with_key(identity: &str, key: RecipeKey) -> FontEntry {
        let custom = CustomFontRef {
            origin: CustomFontOrigin::Saved,
            recipe: Some(hybrid("Alpha-Regular", "Beta-Regular")),
            key: Some(key),
            sources: None,
        };
        custom_entry(custom, identity, "Гибрид", key_content_hash(&key), Vec::new(), FontLanguageCoverage::default())
    }

    /// The display pin bounds egui registrations while a recipe is tuned: the identity keeps
    /// the key it was pinned with (a later key of the same font does not replace it), it is
    /// matched under the identity normalization, it lives exactly as long as its holders, and
    /// only saved custom fonts can be pinned.
    #[test]
    fn a_display_pin_freezes_the_face_until_its_last_holder_drops() {
        let identity = new_saved_identity();
        let opened = RecipeKey([0x11; 32]);
        let applied = RecipeKey([0x22; 32]);
        assert_eq!(pinned_display_key(&identity), None);

        let first = pin_custom_face_display(&saved_entry_with_key(&identity, opened)).expect("a saved font is pinnable");
        assert_eq!(pinned_display_key(&identity), Some(opened));
        assert_eq!(pinned_display_key(&identity.to_ascii_uppercase()), Some(opened), "matched normalized");

        let second = pin_custom_face_display(&saved_entry_with_key(&identity, applied)).expect("pinnable");
        assert_eq!(pinned_display_key(&identity), Some(opened), "the face already on screen stays");
        drop(first);
        assert_eq!(pinned_display_key(&identity), Some(opened), "one holder is still alive");
        drop(second);
        assert_eq!(pinned_display_key(&identity), None, "released with the last holder");

        assert!(pin_custom_face_display(&adhoc_custom_font_entry()).is_none(), "the ad-hoc entry has no face");
        assert!(pin_custom_face_display(&file_entry("Alpha-Regular", Path::new("/fonts/a.ttf"))).is_none());
    }

    #[test]
    fn a_custom_font_is_never_a_source() {
        let _lock = font_settings_store::test_lock();
        font_settings_store::test_reset();
        let (dir, a, _) = scratch_fonts("d4");
        let real = vec![file_entry("Alpha-Regular", &a)];
        let other = new_saved_identity();
        let records = vec![(new_saved_identity(), record("X", CustomFontBody::Known(hybrid("Alpha-Regular", &other))))];
        let (entries, rows) = build_custom_font_entries(&real, &records);
        assert!(entries.is_empty());
        assert_eq!(rows[0].unavailable, Some(CustomFontUnavailable::MissingSources(vec![other])));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_saved_entry_carries_its_own_kerning_pairs() {
        let _lock = font_settings_store::test_lock();
        font_settings_store::test_reset();
        let (dir, a, b) = scratch_fonts("kerning");
        let real = vec![file_entry("Alpha-Regular", &a), file_entry("Beta-Regular", &b)];
        let identity = new_saved_identity();
        let pair = fonts_data::CustomKerningPair { left: 'A', right: 'V', offset_per_mille: -30.0 };
        assert!(font_settings_store::set_font_custom_kerning(&identity, vec![pair.clone()]));
        let records = vec![(identity, record("K", CustomFontBody::Known(hybrid("Alpha-Regular", "Beta-Regular"))))];
        let (entries, _) = build_custom_font_entries(&real, &records);
        assert_eq!(entries[0].custom_kerning(), &[pair]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A bundled `fonts/ui` file as a resolved source, or `None` (logged) when absent.
    fn bundled_source(identity: &str, relative: &str) -> Option<ResolvedSource> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fonts/ui").join(relative);
        match fs::read(&path) {
            Ok(bytes) => Some(ResolvedSource {
                identity: identity.to_owned(),
                content_id: crate::render_next::font_content_id(&bytes),
                data: Arc::new(bytes),
                face_index: 0,
                user_kerning: Arc::from(Vec::new()),
            }),
            Err(err) => {
                eprintln!("skipping: bundled font {} is unavailable ({err})", path.display());
                None
            }
        }
    }

    #[test]
    fn custom_coverage_follows_the_missing_glyph_rule_of_the_compiler() {
        use ms_text_util::language::TextLanguage;
        let (Some(noto), Some(thai)) = (
            bundled_source("Noto", "core/00-NotoSans-Regular.ttf"),
            bundled_source("Thai", "ext/30-NotoSansThai-Regular.ttf"),
        ) else {
            return;
        };
        let sources = [noto, thai];
        let with_policy = |policy| {
            CustomFontRecipe {
                kind: CustomFontKind::Hybrid(HybridParams {
                    sources: vec![
                        HybridSource { identity: "Noto".to_owned(), weight: 1.0, missing_override: None },
                        HybridSource { identity: "Thai".to_owned(), weight: 1.0, missing_override: None },
                    ],
                    missing_default: policy,
                    algorithm: ms_custom_fonts::BlendAlgorithm::Auto,
                    property_weights: None,
                }),
            }
        };
        // Noto Sans Thai has no Cyrillic: with `Disable` every Cyrillic letter is left out of
        // the hybrid, with `NoInfluence` Noto Sans alone shapes them.
        let disabled = recipe_language_coverage_for(&with_policy(MissingGlyphPolicy::Disable), &sources, TextLanguage::Ru)
            .expect("readable sources");
        assert_eq!(disabled.support, FontLanguageSupport::Unsupported);
        let kept = recipe_language_coverage_for(&with_policy(MissingGlyphPolicy::NoInfluence), &sources, TextLanguage::Ru)
            .expect("readable sources");
        assert_eq!(kept.support, FontLanguageSupport::Full);
        // A damaged source is an error, never a guess.
        let broken = [sources[0].clone(), ResolvedSource { data: Arc::new(b"not a font".to_vec()), ..sources[1].clone() }];
        assert!(recipe_language_coverage_for(&with_policy(MissingGlyphPolicy::Disable), &broken, TextLanguage::Ru).is_err());
    }

    #[test]
    fn the_adhoc_entry_is_synthetic_and_previews_in_the_interface_font() {
        let entry = adhoc_custom_font_entry();
        assert!(entry.is_adhoc_custom());
        assert!(entry.is_panel_synthetic());
        assert_eq!(entry.render_identity_name(), ADHOC_CUSTOM_FONT_IDENTITY);
        assert_eq!(entry.display_label(), t!("typing.fonts.adhoc_hybrid_font_label"));
        assert_eq!(entry.byte_source().preview_source(), FontPreviewSource::InterfaceFont);
        assert!(entry.custom_font().and_then(|custom| custom.full_face_blocking()).is_none());
        assert!(entry.file_path().as_os_str().is_empty());
    }
}
