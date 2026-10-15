/*
File: crates/ms-tab-typing/src/custom_fonts/export.rs

Purpose:
Custom fonts in the PSD export (decision D2, `dev-docs/hybrid_fonts_plan.md` §3.6): which
custom font a text layer uses, and the compiled FULL face behind each one, so the PSD names
the layer's font by the face's PostScript name and the export can put the `.ttf` next to the
documents.

Key structures:
- `CustomFontUseKey`: the dedup key of one custom font a layer uses (saved identity, or the
  canonical JSON of an ad-hoc recipe).
- `CustomFontExportResolver`: snapshot taken on the GUI thread at dispatch (saved recipes by
  identity + the panel's source resolver); resolved on the export worker, where it also reads
  each saved font's OWN kerning pairs from the font settings store.
- `CustomExportFaces`: faces per use key, who uses them (name, own pairs) and every failure.
- `BundlePlan` / `BundledFontFile` / `BundleIssue`: the `.ttf` files to write next to the PSDs
  (own pairs baked in) and what the user must be told about them.
- `CustomFontExportError` / `ExportFontName`: why a font failed, and how to name it to the user.
- `test_support` (tests only): fake counting compiler + source table, shared with
  `tab/export.rs` tests.

Key functions:
- `custom_font_use_of_render_data`: overlay `render_data` -> `CustomFontUse` (through the one
  reader, `text_params_schema::custom_font_use`).
- `CustomFontExportResolver::resolve_chapter`: BLOCKING (source reads, hashing, possibly a
  compile through `service::full_face_blocking`); export worker only.
- `CustomExportFaces::bundle_plan`: one file per compiled face; a saved font's own pairs are
  baked in through `ms_custom_fonts::with_kerning_overrides` (worker only: it rewrites bytes).

Notes:
- A failure never aborts an export: the caller turns it into a warning and the layer keeps its
  stored font name.
- The PostScript name comes from the compiled face (`CompiledFace::post_script_name`, minted by
  `RecipeKey::full_face_post_script_name`); this file never spells the format.
- File writing stays with the export pipeline (`tab/export.rs`), which owns the destination.
- Own kerning: the cached face carries the INHERITED kerning only (its `RecipeKey` excludes the
  font's own pairs, and in-app rendering applies them through the provider). Photoshop applies
  only what the file declares, so the bundled copy of a saved font with own pairs gets them
  baked in, under the SAME PostScript name the PSD layers already reference. One face (one
  recipe key, one name) used with DIFFERENT own pairs — two saved fonts with equal recipes, or
  a saved font and an equal ad-hoc recipe — can have only one file: the plan bakes the pairs of
  the saved font with the smallest identity among those with pairs and reports the conflict.
*/

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use ms_custom_fonts::{CompileError, CustomFontRecipe, KerningPairPerMille, with_kerning_overrides};
use serde_json::Value;

use super::service::{CompiledFace, CustomFontError, CustomFontService, service};
use super::sources::{SourceResolver, user_kerning_per_mille};
use crate::panel::custom_font_entry::resolve_recipe_sources;
use crate::panel::font_settings_store;
use crate::panel::fonts::normalize_font_identity;
use crate::render_next::FontBytes;
use crate::panel::text_params_schema::{CustomFontUse, custom_font_use};

/// One custom font as the export deduplicates it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CustomFontUseKey {
    /// A saved custom font: its NORMALIZED saved identity.
    Saved(String),
    /// An ad-hoc (layer-owned) recipe: its canonical JSON (`CustomFontRecipe::to_json`;
    /// serde_json sorts object keys, so equal recipes give equal strings).
    AdHoc(String),
}

impl CustomFontUseKey {
    /// The key of `font_use`, or `None` for an ad-hoc layer whose recipe is missing or damaged
    /// (it has no face to name).
    #[must_use]
    pub(crate) fn of(font_use: &CustomFontUse) -> Option<Self> {
        match font_use {
            CustomFontUse::Saved(identity) => Some(Self::Saved(normalize_font_identity(identity))),
            CustomFontUse::AdHoc(Ok(recipe)) => Some(Self::AdHoc(recipe.to_json().to_string())),
            CustomFontUse::AdHoc(Err(_)) => None,
        }
    }
}

/// The custom font an overlay's `render_data` uses, read from its stored `text_params`
/// object; `None` for a real font, an image overlay or a payload without `text_params`.
#[must_use]
pub(crate) fn custom_font_use_of_render_data(render_data: &Value) -> Option<CustomFontUse> {
    render_data.get("text_params").and_then(Value::as_object).and_then(custom_font_use)
}

/// How a failed custom font is named in the user-facing warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExportFontName {
    /// A saved font: its user-visible name (its identity when the panel list does not know it).
    Saved(String),
    /// A layer's ad-hoc hybrid (the UI names it with the ad-hoc entry's label).
    AdHoc,
}

/// Why one custom font could not be exported.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub(crate) enum CustomFontExportError {
    /// The saved identity is not an available font of the panel list (deleted, or its
    /// sources are missing).
    #[error("saved custom font '{0}' is not available")]
    UnknownSaved(String),
    /// The ad-hoc layer's stored recipe is missing or damaged (log-ready reason).
    #[error("the layer's ad-hoc recipe is unusable: {0}")]
    InvalidAdHocRecipe(String),
    /// Sources or compile failed.
    #[error(transparent)]
    Face(#[from] CustomFontError),
}

/// One custom font that has no face in this export.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CustomExportFailure {
    /// Which font, for the user.
    pub(crate) font: ExportFontName,
    /// Why (log text).
    pub(crate) error: CustomFontExportError,
}

/// Who uses a resolved face: the name for the user and the font's OWN kerning pairs.
#[derive(Debug, Clone)]
struct FaceUse {
    /// Which font, for the user.
    font: ExportFontName,
    /// The saved font's own pairs (‰ em, store order); empty for an ad-hoc recipe or a saved
    /// font without own pairs.
    own_kerning: Arc<[KerningPairPerMille]>,
}

/// Result of `CustomFontExportResolver::resolve_chapter`.
#[derive(Debug, Default)]
pub(crate) struct CustomExportFaces {
    /// The compiled full face of every resolved use key.
    pub(crate) faces: HashMap<CustomFontUseKey, Arc<CompiledFace>>,
    /// Name and own pairs of every key of `faces`.
    uses: HashMap<CustomFontUseKey, FaceUse>,
    /// Every font that failed, at most once per use key, in first-use order.
    pub(crate) failures: Vec<CustomExportFailure>,
}

/// One `.ttf` to put next to the PSD files.
pub(crate) struct BundledFontFile {
    /// The compiled face (its PostScript name names the file).
    pub(crate) face: Arc<CompiledFace>,
    /// The bytes to write: the face's own bytes, or a copy with a saved font's own kerning
    /// pairs baked in (same names, same tables otherwise).
    pub(crate) bytes: FontBytes,
}

impl std::fmt::Debug for BundledFontFile {
    /// The face and the byte count; never the bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BundledFontFile").field("face", &self.face).field("bytes", &(*self.bytes).as_ref().len()).finish()
    }
}

/// Something the user must know about the bundled files (the files are still written).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BundleIssue {
    /// Several fonts with DIFFERENT own kerning share one face (one file, one PostScript name);
    /// the file carries the pairs of `kept`, so `others` may be spaced differently in Photoshop.
    KerningConflict {
        /// PostScript name of the shared face.
        face: String,
        /// The font whose own pairs the file carries.
        kept: ExportFontName,
        /// The other fonts sharing the face, sorted for a stable message.
        others: Vec<ExportFontName>,
    },
    /// The font's own pairs could not be baked into its file; the file is written with the
    /// inherited kerning only.
    KerningNotApplied {
        /// Which font.
        font: ExportFontName,
        /// Why (log text).
        error: CompileError,
    },
}

/// Result of `CustomExportFaces::bundle_plan`.
#[derive(Debug, Default)]
pub(crate) struct BundlePlan {
    /// One file per distinct face, sorted by PostScript name.
    pub(crate) files: Vec<BundledFontFile>,
    /// Issues in file order; each is also logged.
    pub(crate) issues: Vec<BundleIssue>,
}

impl CustomExportFaces {
    /// Every distinct compiled face (deduplicated by recipe key: a saved font and an ad-hoc
    /// recipe with the same key are one face), sorted by PostScript name for a stable order.
    #[must_use]
    pub(crate) fn unique_faces(&self) -> Vec<Arc<CompiledFace>> {
        let mut by_key: HashMap<ms_custom_fonts::RecipeKey, Arc<CompiledFace>> = HashMap::new();
        for face in self.faces.values() {
            by_key.entry(face.key).or_insert_with(|| Arc::clone(face));
        }
        let mut faces: Vec<Arc<CompiledFace>> = by_key.into_values().collect();
        faces.sort_by(|a, b| a.post_script_name.cmp(&b.post_script_name));
        faces
    }

    /// The `.ttf` files to bundle: one per distinct face (`unique_faces` order). A face used by
    /// a saved font with own kerning pairs is written with them baked in
    /// (`ms_custom_fonts::with_kerning_overrides`: inherited pairs overridden, a `0` cancels),
    /// so Photoshop spaces the text as the app does; every other face is written as compiled.
    /// One face used with different own pairs: see `BundleIssue::KerningConflict`. A bake that
    /// fails leaves the compiled bytes and reports `KerningNotApplied`.
    ///
    /// BLOCKS (parses and re-assembles font files): the export worker only.
    #[must_use]
    pub(crate) fn bundle_plan(&self) -> BundlePlan {
        let mut plan = BundlePlan::default();
        for face in self.unique_faces() {
            let mut users: Vec<(&CustomFontUseKey, &FaceUse)> = self
                .faces
                .iter()
                .filter(|(_, used)| used.key == face.key)
                .filter_map(|(key, _)| self.uses.get(key).map(|face_use| (key, face_use)))
                .collect();
            // Saved fonts by identity first (ad-hoc keys after them), so the choice below is
            // deterministic whatever the map order.
            users.sort_by(|(a, _), (b, _)| use_key_order(a).cmp(&use_key_order(b)));
            let Some(&(_, chosen)) = users.iter().find(|(_, face_use)| !face_use.own_kerning.is_empty()) else {
                plan.files.push(BundledFontFile { bytes: Arc::clone(&face.bytes), face });
                continue;
            };
            let chosen_pairs = canonical_pairs(&chosen.own_kerning);
            let mut others: Vec<ExportFontName> = users
                .iter()
                .filter(|(_, face_use)| canonical_pairs(&face_use.own_kerning) != chosen_pairs)
                .map(|(_, face_use)| face_use.font.clone())
                .collect();
            if !others.is_empty() {
                others.sort_by(|a, b| export_font_name_order(a).cmp(&export_font_name_order(b)));
                others.dedup();
                ms_log::runtime_log::log_warn(format!(
                    "typing PSD export: custom font face {} is shared by fonts with different own kerning pairs; the bundled file carries the pairs of {:?}, not those of {others:?}",
                    face.post_script_name, chosen.font
                ));
                plan.issues.push(BundleIssue::KerningConflict { face: face.post_script_name.clone(), kept: chosen.font.clone(), others });
            }
            match with_kerning_overrides((*face.bytes).as_ref(), &chosen.own_kerning) {
                Ok(bytes) => plan.files.push(BundledFontFile { bytes: Arc::new(bytes), face }),
                Err(error) => {
                    ms_log::runtime_log::log_warn(format!(
                        "typing PSD export: could not bake the {} own kerning pair(s) of {:?} into face {}; the bundled file keeps the inherited kerning only: {error}",
                        chosen.own_kerning.len(),
                        chosen.font,
                        face.post_script_name
                    ));
                    plan.issues.push(BundleIssue::KerningNotApplied { font: chosen.font.clone(), error });
                    plan.files.push(BundledFontFile { bytes: Arc::clone(&face.bytes), face });
                }
            }
        }
        plan
    }
}

/// Sort key of a use key: saved identities first, in identity order, then ad-hoc recipes.
fn use_key_order(key: &CustomFontUseKey) -> (u8, &str) {
    match key {
        CustomFontUseKey::Saved(identity) => (0, identity.as_str()),
        CustomFontUseKey::AdHoc(json) => (1, json.as_str()),
    }
}

/// Sort key of a font name in a message: saved names first, then the ad-hoc label.
fn export_font_name_order(name: &ExportFontName) -> (u8, &str) {
    match name {
        ExportFontName::Saved(name) => (0, name.as_str()),
        ExportFontName::AdHoc => (1, ""),
    }
}

/// The pairs as the face would get them: first occurrence of a `(left, right)` wins, value by
/// bits (so two lists with the same effect compare equal whatever their order).
fn canonical_pairs(pairs: &[KerningPairPerMille]) -> BTreeMap<(char, char), u32> {
    let mut canonical = BTreeMap::new();
    for pair in pairs {
        canonical.entry((pair.left, pair.right)).or_insert(pair.em.to_bits());
    }
    canonical
}

/// A saved custom font as the export sees it.
#[derive(Debug, Clone)]
struct SavedExportFont {
    /// The saved identity as the font list spells it (the store key of its own pairs).
    identity: String,
    /// User-visible name (warnings only).
    name: String,
    recipe: Arc<CustomFontRecipe>,
}

/// Reads a saved font's OWN kerning pairs by identity (store order, ‰ em). Called on the export
/// worker; the production lookup reads the process-global font settings store.
type OwnKerningLookup = Arc<dyn Fn(&str) -> Arc<[KerningPairPerMille]> + Send + Sync>;

/// The production `OwnKerningLookup`: the font settings store, the same record the font list
/// hands the renderer (`custom_font_entry`, keyed by the saved identity).
fn store_own_kerning() -> OwnKerningLookup {
    Arc::new(|identity: &str| user_kerning_per_mille(&font_settings_store::font_custom_kerning(identity)))
}

/// What the export worker needs to turn custom-font uses into compiled faces: the SAVED
/// recipes of the panel's font list, keyed by normalized identity, and the panel's source
/// resolver (real + bundled fonts; custom fonts refused). Built on the GUI thread (cheap Arc
/// clones), resolved on the export worker.
pub(crate) struct CustomFontExportResolver {
    saved: HashMap<String, SavedExportFont>,
    sources: Arc<dyn SourceResolver>,
    service: Arc<CustomFontService>,
    own_kerning: OwnKerningLookup,
}

impl std::fmt::Debug for CustomFontExportResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomFontExportResolver").field("saved", &self.saved.len()).finish_non_exhaustive()
    }
}

impl CustomFontExportResolver {
    /// A resolver over `saved` (`(identity, user-visible name, recipe)` of every available
    /// saved custom font; a later duplicate identity is ignored) and `sources`, compiling
    /// through the process-global service. Own kerning pairs are read from the font settings
    /// store when resolving (on the export worker), so the bundle carries the pairs as saved
    /// at export time.
    #[must_use]
    pub(crate) fn new(
        saved: impl IntoIterator<Item = (String, String, Arc<CustomFontRecipe>)>,
        sources: Arc<dyn SourceResolver>,
    ) -> Self {
        Self::with_service(saved, sources, Arc::clone(service()))
    }

    /// `new` over an explicit service (the test seam: a fake compiler, no disk cache). Own
    /// pairs still come from the font settings store; tests replace that with
    /// `with_own_kerning`.
    #[must_use]
    pub(crate) fn with_service(
        saved: impl IntoIterator<Item = (String, String, Arc<CustomFontRecipe>)>,
        sources: Arc<dyn SourceResolver>,
        service: Arc<CustomFontService>,
    ) -> Self {
        let mut map = HashMap::new();
        for (identity, name, recipe) in saved {
            map.entry(normalize_font_identity(&identity)).or_insert(SavedExportFont { identity, name, recipe });
        }
        Self { saved: map, sources, service, own_kerning: store_own_kerning() }
    }

    /// Replaces the own-kerning lookup (tests: no process-global store).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_own_kerning(mut self, lookup: impl Fn(&str) -> Vec<KerningPairPerMille> + Send + Sync + 'static) -> Self {
        self.own_kerning = Arc::new(move |identity: &str| Arc::from(lookup(identity)));
        self
    }

    /// Resolves every custom font in `uses` to its compiled full face: each distinct use key
    /// once, from memory / disk cache or by compiling (joining an in-flight compile of the same
    /// recipe key), and reads each resolved saved font's own kerning pairs. Failures are
    /// collected, never fatal; each is logged.
    ///
    /// BLOCKS (file reads, hashing, compiles of seconds): the export worker only.
    #[must_use]
    pub(crate) fn resolve_chapter(&self, uses: impl IntoIterator<Item = CustomFontUse>) -> CustomExportFaces {
        let mut out = CustomExportFaces::default();
        let mut failed_adhoc_recipe = false;
        let mut seen: HashSet<CustomFontUseKey> = HashSet::new();
        for font_use in uses {
            let Some(key) = CustomFontUseKey::of(&font_use) else {
                // A damaged ad-hoc recipe: one warning per export is enough, the reason is the
                // same kind for every such layer.
                if let CustomFontUse::AdHoc(Err(reason)) = font_use
                    && !failed_adhoc_recipe
                {
                    failed_adhoc_recipe = true;
                    record_failure(&mut out, ExportFontName::AdHoc, CustomFontExportError::InvalidAdHocRecipe(reason));
                }
                continue;
            };
            if !seen.insert(key.clone()) {
                continue;
            }
            let (name, recipe, saved_identity) = match &font_use {
                CustomFontUse::Saved(stored) => match self.saved.get(&normalize_font_identity(stored)) {
                    Some(saved) => (ExportFontName::Saved(saved.name.clone()), Arc::clone(&saved.recipe), Some(saved.identity.as_str())),
                    None => {
                        record_failure(&mut out, ExportFontName::Saved(stored.clone()), CustomFontExportError::UnknownSaved(stored.clone()));
                        continue;
                    }
                },
                CustomFontUse::AdHoc(Ok(recipe)) => (ExportFontName::AdHoc, Arc::clone(recipe), None),
                // Unreachable: a damaged ad-hoc recipe has no key and was handled above.
                CustomFontUse::AdHoc(Err(_)) => continue,
            };
            let face = resolve_recipe_sources(&recipe, self.sources.as_ref())
                .map_err(CustomFontError::from)
                .and_then(|sources| self.service.full_face_blocking(&recipe, &sources));
            match face {
                Ok(face) => {
                    // An ad-hoc recipe has no own pairs: they are keyed by a saved identity.
                    let own_kerning = saved_identity.map_or_else(|| Arc::from(Vec::new()), |identity| (self.own_kerning)(identity));
                    out.uses.insert(key.clone(), FaceUse { font: name, own_kerning });
                    out.faces.insert(key, face);
                }
                Err(err) => record_failure(&mut out, name, err.into()),
            }
        }
        out
    }
}

/// Records (and logs) one failure.
fn record_failure(out: &mut CustomExportFaces, font: ExportFontName, error: CustomFontExportError) {
    ms_log::runtime_log::log_warn(format!(
        "typing PSD export: custom font {font:?} has no compiled face; its layers keep the stored font name: {error}"
    ));
    out.failures.push(CustomExportFailure { font, error });
}

/// Fixtures shared by the export tests here and in `tab/export.rs`: a fake compiler (counts
/// its calls, returns a correctly keyed filler face or fixed real bytes), a fixed two-font
/// source table, a two-source recipe and a real compiled face over the tracked bundled fonts
/// (`None` when they are absent). No disk cache, no font settings store.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use ms_custom_fonts::{
        CompileError, CompileScope, CompiledFont, CoverageReport, CustomFontKind, CustomFontRecipe, HybridParams,
        HybridSource, KerningPairPerMille, MissingGlyphPolicy, SourceDigest, SourceFont, compile, recipe_key,
        source_digest,
    };

    use super::CustomFontExportResolver;
    use crate::custom_fonts::identity::compiled_face_post_script_name;
    use crate::custom_fonts::service::{CompileFn, CustomFontService};
    use crate::custom_fonts::sources::{ResolvedSource, SourceError, SourceResolver};
    use crate::render_next::font_content_id;

    /// Identities the fake source table knows.
    pub(crate) const FAKE_SOURCES: [&str; 2] = ["Fake-A", "Fake-B"];

    /// A two-source hybrid recipe over `identities`, second weight `weight_b`.
    pub(crate) fn recipe(identities: [&str; 2], weight_b: f32) -> Arc<CustomFontRecipe> {
        Arc::new(CustomFontRecipe {
            kind: CustomFontKind::Hybrid(HybridParams {
                sources: vec![
                    HybridSource { identity: identities[0].to_owned(), weight: 1.0, missing_override: None },
                    HybridSource { identity: identities[1].to_owned(), weight: weight_b, missing_override: None },
                ],
                missing_default: MissingGlyphPolicy::NoInfluence,
            }),
        })
    }

    struct TableResolver(HashMap<String, ResolvedSource>);

    impl SourceResolver for TableResolver {
        fn resolve_source(&self, identity: &str) -> Result<ResolvedSource, SourceError> {
            self.0.get(identity).cloned().ok_or_else(|| SourceError::Unknown(identity.to_owned()))
        }
    }

    /// A resolver over `FAKE_SOURCES` (bytes = the identity's UTF-8).
    pub(crate) fn resolver() -> Arc<dyn SourceResolver> {
        let table = FAKE_SOURCES
            .into_iter()
            .map(|identity| {
                let bytes = identity.as_bytes().to_vec();
                let content_id = font_content_id(&bytes);
                let source = ResolvedSource {
                    identity: identity.to_owned(),
                    data: Arc::new(bytes),
                    face_index: 0,
                    content_id,
                    user_kerning: Arc::from(Vec::new()),
                };
                (identity.to_owned(), source)
            })
            .collect();
        Arc::new(TableResolver(table))
    }

    /// Boxes a closure as a `CompileFn` (the bound lets the higher-ranked signature be inferred).
    fn boxed(
        compiler: impl Fn(&CustomFontRecipe, &[SourceFont<'_>], CompileScope<'_>, &(dyn Fn(usize, usize) + Sync), &AtomicBool) -> Result<CompiledFont, CompileError>
            + Send
            + Sync
            + 'static,
    ) -> Box<CompileFn> {
        Box::new(compiler)
    }

    /// A service whose compiler counts its calls in `calls` and returns a keyed 16-byte filler
    /// face; no disk cache.
    pub(crate) fn counting_service(calls: Arc<AtomicUsize>) -> Arc<CustomFontService> {
        fixed_bytes_service(vec![7_u8; 16], calls)
    }

    /// A service whose compiler counts its calls in `calls` and returns `bytes` as the face of
    /// any recipe (key and names derived from the requested recipe); no disk cache.
    pub(crate) fn fixed_bytes_service(bytes: Vec<u8>, calls: Arc<AtomicUsize>) -> Arc<CustomFontService> {
        let compiler = boxed(move |recipe, sources, _scope, _progress, _cancel| {
            calls.fetch_add(1, Ordering::SeqCst);
            let digests: Vec<(SourceDigest, &[KerningPairPerMille])> =
                sources.iter().map(|source| (source_digest(source.data, source.face_index), source.user_kerning)).collect();
            let key = recipe_key(recipe, &digests);
            Ok(CompiledFont {
                bytes: bytes.clone(),
                family_name: format!("MS Custom {}", key.short8()),
                postscript_name: compiled_face_post_script_name(&key),
                key,
                report: CoverageReport::default(),
            })
        });
        Arc::new(CustomFontService::new(compiler, None, 1024 * 1024))
    }

    /// An export resolver over `saved` and the fake sources, compiling through a counting
    /// service; no saved font has own kerning pairs.
    pub(crate) fn export_resolver(saved: Vec<(String, String, Arc<CustomFontRecipe>)>, calls: &Arc<AtomicUsize>) -> CustomFontExportResolver {
        CustomFontExportResolver::with_service(saved, resolver(), counting_service(Arc::clone(calls))).with_own_kerning(|_| Vec::new())
    }

    /// A REAL compiled hybrid face (Noto Sans Regular + Bold, characters "AVT"), whose inherited
    /// `kern` has the built-in A/V pair; `None` (logged) when the tracked bundled fonts are
    /// absent. Compiled once per test process.
    pub(crate) fn real_face_bytes() -> Option<Vec<u8>> {
        static FACE: OnceLock<Option<Vec<u8>>> = OnceLock::new();
        FACE.get_or_init(|| {
            let fonts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fonts/ui");
            let read = |relative: &str| match std::fs::read(fonts.join(relative)) {
                Ok(bytes) => Some(bytes),
                Err(err) => {
                    eprintln!("skipping: bundled font {relative} is unavailable ({err})");
                    None
                }
            };
            let (regular, bold) = (read("core/00-NotoSans-Regular.ttf")?, read("bold/00-NotoSans-Bold.ttf")?);
            let recipe = recipe(["Regular", "Bold"], 1.0);
            let sources = [
                SourceFont { identity: "Regular", data: &regular, face_index: 0, user_kerning: &[] },
                SourceFont { identity: "Bold", data: &bold, face_index: 0, user_kerning: &[] },
            ];
            let chars = "AVT".chars().collect();
            let compiled = compile(&recipe, &sources, CompileScope::Subset(&chars), &|_, _| {}, &AtomicBool::new(false));
            Some(compiled.expect("the bundled fonts compile").bytes)
        })
        .clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::test_support::{FAKE_SOURCES, export_resolver, fixed_bytes_service, real_face_bytes, recipe, resolver};
    use super::*;
    use crate::custom_fonts::identity::ADHOC_CUSTOM_FONT_IDENTITY;
    use crate::custom_fonts::sources::SourceError;

    const SAVED_ID: &str = "MSCustom-0123456789abcdef";

    #[test]
    fn two_adhoc_layers_with_one_recipe_give_one_face() {
        let calls = Arc::new(AtomicUsize::new(0));
        let export = export_resolver(Vec::new(), &calls);
        let shared = recipe(["Fake-A", "Fake-B"], 0.5);
        let faces = export.resolve_chapter([
            CustomFontUse::AdHoc(Ok(Arc::clone(&shared))),
            CustomFontUse::AdHoc(Ok(Arc::new((*shared).clone()))),
        ]);
        assert!(faces.failures.is_empty(), "{:?}", faces.failures);
        assert_eq!(faces.faces.len(), 1);
        assert_eq!(faces.unique_faces().len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let face = &faces.unique_faces()[0];
        assert!(face.post_script_name.starts_with("MSCustom-"), "{}", face.post_script_name);
    }

    #[test]
    fn saved_and_adhoc_mix_resolves_both_and_dedups_by_recipe_key() {
        let calls = Arc::new(AtomicUsize::new(0));
        let saved_recipe = recipe(["Fake-A", "Fake-B"], 2.0);
        let export = export_resolver(vec![(SAVED_ID.to_owned(), "Мой гибрид".to_owned(), Arc::clone(&saved_recipe))], &calls);
        // The saved identity is matched case-insensitively (identity normalization).
        let faces = export.resolve_chapter([
            CustomFontUse::Saved(SAVED_ID.to_ascii_uppercase()),
            CustomFontUse::AdHoc(Ok(recipe(["Fake-A", "Fake-B"], 0.5))),
            // The same recipe as the saved font, used ad hoc: a second use key, one face.
            CustomFontUse::AdHoc(Ok(Arc::clone(&saved_recipe))),
        ]);
        assert!(faces.failures.is_empty(), "{:?}", faces.failures);
        assert_eq!(faces.faces.len(), 3);
        assert_eq!(faces.unique_faces().len(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let saved_face = &faces.faces[&CustomFontUseKey::Saved(SAVED_ID.to_ascii_lowercase())];
        let adhoc_same = &faces.faces[&CustomFontUseKey::AdHoc(saved_recipe.to_json().to_string())];
        assert_eq!(saved_face.post_script_name, adhoc_same.post_script_name);
    }

    #[test]
    fn failures_are_collected_not_fatal() {
        let calls = Arc::new(AtomicUsize::new(0));
        let export = export_resolver(Vec::new(), &calls);
        let faces = export.resolve_chapter([
            CustomFontUse::Saved(SAVED_ID.to_owned()),
            CustomFontUse::Saved(SAVED_ID.to_owned()),
            CustomFontUse::AdHoc(Ok(recipe(["Fake-A", "Missing-Font"], 1.0))),
            CustomFontUse::AdHoc(Err("'font_recipe' is missing".to_owned())),
            CustomFontUse::AdHoc(Err("other".to_owned())),
            CustomFontUse::AdHoc(Ok(recipe(["Fake-A", "Fake-B"], 1.0))),
        ]);
        assert_eq!(faces.faces.len(), 1, "the healthy recipe still resolves");
        assert_eq!(faces.failures.len(), 3, "{:?}", faces.failures);
        assert_eq!(faces.failures[0].font, ExportFontName::Saved(SAVED_ID.to_owned()));
        assert_eq!(faces.failures[0].error, CustomFontExportError::UnknownSaved(SAVED_ID.to_owned()));
        assert_eq!(faces.failures[1].font, ExportFontName::AdHoc);
        assert!(matches!(
            &faces.failures[1].error,
            CustomFontExportError::Face(CustomFontError::Source(SourceError::Unknown(identity))) if identity == "Missing-Font"
        ));
        assert!(matches!(&faces.failures[2].error, CustomFontExportError::InvalidAdHocRecipe(_)));
    }

    /// The `kern` value of a character pair in `bytes`, `None` when there is no pair.
    fn kern(bytes: &[u8], left: char, right: char) -> Option<i16> {
        let face = ttf_parser::Face::parse(bytes, 0).expect("valid face");
        let (l, r) = (face.glyph_index(left)?, face.glyph_index(right)?);
        face.tables().kern?.subtables.into_iter().find_map(|subtable| subtable.glyphs_kerning(l, r))
    }

    fn post_script_name(bytes: &[u8]) -> Option<String> {
        let face = ttf_parser::Face::parse(bytes, 0).expect("valid face");
        face.names().into_iter().filter(|name| name.name_id == ttf_parser::name_id::POST_SCRIPT_NAME).find_map(|name| name.to_string())
    }

    fn bytes_of(bytes: &FontBytes) -> &[u8] {
        (**bytes).as_ref()
    }

    fn pair(left: char, right: char, em: f32) -> KerningPairPerMille {
        KerningPairPerMille { left, right, em }
    }

    /// A resolver over the real compiled face whose saved font `SAVED_ID` (recipe weight 2) has
    /// `own` pairs; `None` when the bundled fonts are absent.
    fn real_resolver(own: Vec<KerningPairPerMille>) -> Option<CustomFontExportResolver> {
        let bytes = real_face_bytes()?;
        let service = fixed_bytes_service(bytes, Arc::new(AtomicUsize::new(0)));
        let saved = vec![(SAVED_ID.to_owned(), "Мой гибрид".to_owned(), recipe(FAKE_SOURCES, 2.0))];
        Some(CustomFontExportResolver::with_service(saved, resolver(), service).with_own_kerning(move |identity| {
            assert_eq!(identity, SAVED_ID, "own pairs are looked up by the saved identity as listed");
            own.clone()
        }))
    }

    #[test]
    fn own_pairs_are_baked_into_the_bundled_copy_only() {
        let Some(export) = real_resolver(vec![pair('A', 'V', -150.0), pair('T', 'A', 0.0), pair('A', 'Ж', -10.0)]) else {
            return;
        };
        let faces = export.resolve_chapter([CustomFontUse::Saved(SAVED_ID.to_owned())]);
        let plan = faces.bundle_plan();
        assert!(plan.issues.is_empty(), "{:?}", plan.issues);
        let [file] = plan.files.as_slice() else { panic!("one file expected, got {:?}", plan.files) };
        assert_eq!(kern(bytes_of(&file.bytes), 'A', 'V'), Some(-150), "the own pair overrides the inherited one");
        assert_eq!(kern(bytes_of(&file.bytes), 'T', 'A'), None, "an own 0 cancels the pair");
        let inherited = kern(bytes_of(&file.face.bytes), 'A', 'V');
        assert!(inherited.is_some_and(|value| value != -150), "the cached face keeps the inherited pair: {inherited:?}");
        assert_eq!(post_script_name(bytes_of(&file.bytes)), post_script_name(bytes_of(&file.face.bytes)), "the PostScript name is unchanged");
    }

    #[test]
    fn fonts_without_own_pairs_are_bundled_as_compiled() {
        let Some(export) = real_resolver(Vec::new()) else {
            return;
        };
        let faces = export.resolve_chapter([
            CustomFontUse::Saved(SAVED_ID.to_owned()),
            CustomFontUse::AdHoc(Ok(recipe(FAKE_SOURCES, 0.5))),
        ]);
        let plan = faces.bundle_plan();
        assert!(plan.issues.is_empty(), "{:?}", plan.issues);
        assert_eq!(plan.files.len(), 2);
        for file in &plan.files {
            assert!(Arc::ptr_eq(&file.bytes, &file.face.bytes), "no rewrite without own pairs");
        }
    }

    #[test]
    fn one_face_with_different_own_pairs_is_a_reported_conflict() {
        let Some(export) = real_resolver(vec![pair('A', 'V', -150.0)]) else {
            return;
        };
        // The same recipe used ad hoc: one face, one file name, but no own pairs.
        let faces = export.resolve_chapter([
            CustomFontUse::AdHoc(Ok(recipe(FAKE_SOURCES, 2.0))),
            CustomFontUse::Saved(SAVED_ID.to_owned()),
        ]);
        let plan = faces.bundle_plan();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(kern(bytes_of(&plan.files[0].bytes), 'A', 'V'), Some(-150), "the saved font's pairs win");
        assert_eq!(
            plan.issues,
            vec![BundleIssue::KerningConflict {
                face: plan.files[0].face.post_script_name.clone(),
                kept: ExportFontName::Saved("Мой гибрид".to_owned()),
                others: vec![ExportFontName::AdHoc],
            }]
        );
    }

    #[test]
    fn a_failed_bake_keeps_the_compiled_bytes_and_is_reported() {
        let calls = Arc::new(AtomicUsize::new(0));
        let saved = vec![(SAVED_ID.to_owned(), "Мой гибрид".to_owned(), recipe(FAKE_SOURCES, 2.0))];
        // The fake compiler's 16 filler bytes are no font: the bake must fail.
        let export = export_resolver(saved, &calls).with_own_kerning(|_| vec![pair('A', 'V', -150.0)]);
        let plan = export.resolve_chapter([CustomFontUse::Saved(SAVED_ID.to_owned())]).bundle_plan();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(bytes_of(&plan.files[0].bytes), [7_u8; 16].as_slice());
        assert!(matches!(
            plan.issues.as_slice(),
            [BundleIssue::KerningNotApplied { font: ExportFontName::Saved(name), error: CompileError::UnsupportedFace { .. } }] if name == "Мой гибрид"
        ));
    }

    #[test]
    fn render_data_reader_goes_through_the_schema_reader() {
        assert_eq!(custom_font_use_of_render_data(&json!({ "text_params": { "font": "Alpha-Regular" } })), None);
        assert_eq!(custom_font_use_of_render_data(&json!({ "image": true })), None);
        assert_eq!(
            custom_font_use_of_render_data(&json!({ "text_params": { "font": SAVED_ID } })),
            Some(CustomFontUse::Saved(SAVED_ID.to_owned()))
        );
        let adhoc = recipe(["Fake-A", "Fake-B"], 1.0);
        let use_ = custom_font_use_of_render_data(&json!({
            "text_params": { "font": ADHOC_CUSTOM_FONT_IDENTITY, "font_recipe": adhoc.to_json() }
        }))
        .expect("an ad-hoc layer uses a custom font");
        assert_eq!(CustomFontUseKey::of(&use_), Some(CustomFontUseKey::AdHoc(adhoc.to_json().to_string())));
    }
}
