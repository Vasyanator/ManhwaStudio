/*
File: src/tabs/typing/render_next/font_registry.rs

Purpose:
Подсистема регистрации базового и inline-шрифтов нового рендера typing.

Main responsibilities:
- инкапсулировать загрузку выбранного font face;
- строить registry inline-шрифтов по label для rich-text path;
- отделить font registration от layout/raster pipeline;
- дедуплицировать загрузку шрифтов через `FontFaceCache`, чтобы переиспользуемая
  `FontSystem` из пула не накапливала дублирующиеся faces;
- следить, чтобы выбранный пользователем шрифт выигрывал сопоставление по имени
  семейства у бандлового фейса с тем же именем (`displace_bundled_faces`);
- отвечать на вопрос «может ли эта база обслужить такие attrs» —
  `family_has_matching_face` (style/stretch) и `family_has_face_of_requested_weight`
  (weight), guard для любой МОДИФИКАЦИИ attrs (см. UNSERVICEABLE-ATTRS GUARD в
  `MODULE_README.md`).

Notes:
Fonts reach the render path by WORKING NAME through a `FontProvider`; the core
loader is `load_font_content`, which takes a resolved `FontContent` (bytes +
face + content id) and never touches the filesystem. Loading is cache-gated by
`content_id`: on a cache hit the bytes are NOT re-hashed and NOT re-loaded into
fontdb; the previously loaded face IDs and metadata are reused. Default font
families are still set every render (cheap, deterministic matching).
`load_selected_font_from_path` is a THIN COMPAT WRAPPER over `load_font_content`
for the app's forms-metric measurement path, which still holds a path and its own
throwaway `FontSystem`. The throwaway-DB helpers `resolve_font_postscript_name` /
`resolve_font_family_name` are export-only and stay uncached.
*/

use super::font_provider::{FontContent, FontProvider, font_content_id};
use super::font_system_pool::FontFaceCache;
use cosmic_text::{Attrs, Family, FontSystem, Stretch, Style, Weight, fontdb};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct RegisteredFontFace {
    pub family_name: Option<String>,
    pub style: Option<Style>,
    pub weight: Option<Weight>,
    pub stretch: Option<Stretch>,
}

impl RegisteredFontFace {
    #[must_use]
    pub fn apply_to_attrs<'a>(&'a self, mut attrs: Attrs<'a>) -> Attrs<'a> {
        if let Some(name) = self.family_name.as_deref() {
            attrs = attrs.family(Family::Name(name));
        }
        if let Some(style) = self.style {
            attrs = attrs.style(style);
        }
        if let Some(weight) = self.weight {
            attrs = attrs.weight(weight);
        }
        if let Some(stretch) = self.stretch {
            attrs = attrs.stretch(stretch);
        }
        attrs
    }
}

pub type InlineFontRegistry = BTreeMap<String, RegisteredFontFace>;

#[derive(Debug, Default)]
pub struct InlineFontRegistryBuild {
    pub registry: InlineFontRegistry,
    pub warnings: Vec<String>,
}

/// Loads `content`'s bytes into `font_system` (deduplicated by `content.content_id`
/// through `font_cache`) and returns the metadata of the face at `face_index`.
///
/// Behaves exactly like the old path loader but keyed on content id instead of a
/// file path: on a cache hit the bytes are neither re-hashed nor re-registered —
/// the previously loaded fontdb IDs and resolved face metadata are reused. On a
/// miss the bytes are registered and both the IDs and metadata are cached. Either
/// way default families are set via `apply_default_families` so matching stays
/// deterministic on a reused system (a no-family face restores the system's
/// pristine defaults).
///
/// Determinism guard: on a miss, if a DIFFERENT already-loaded content (different
/// content id) shares this face's `(family, weight, style, stretch)`, the cache is
/// marked tainted so the pool drops the system instead of reusing it (see
/// `font_system_pool.rs`).
///
/// RESIDENT SHORT-CIRCUIT: when `content` carries the `'static` bytes of a BUNDLED
/// `fonts/ui` font (the "built-in interface font" the typing panel offers), those
/// bytes are already registered in every system built on the base, so the face they
/// produced is reused and NOTHING is added to the database. Without it, selecting a
/// bundled font would put a second `(family, weight, style)` face into every pooled
/// system. See `font_base::resident_face_ids`.
///
/// # Errors
/// Returns an error string if fontdb cannot parse the bytes.
pub fn load_font_content(
    font_system: &mut FontSystem,
    font_cache: &mut FontFaceCache,
    content: &FontContent,
    face_index: usize,
) -> Result<RegisteredFontFace, String> {
    let content_id = content.content_id;
    // Must run before anything is registered: it is what later tells the system's own
    // bundled base from the faces this cache added (see `displace_bundled_faces`).
    font_cache.capture_preexisting_faces(font_system);

    if font_cache.loaded_ids(content_id).is_some() {
        // Cache hit: faces are already in this system's db. Reuse resolved
        // metadata, or resolve it from the already-loaded IDs on first request
        // for this face index.
        let selected = if let Some(face) = font_cache.cached_meta(content_id, face_index) {
            face.clone()
        } else {
            // `loaded_ids` is present, so this re-borrow yields the same slice.
            let ids = font_cache
                .loaded_ids(content_id)
                .ok_or_else(|| "font cache lost its loaded face IDs".to_string())?
                .to_vec();
            let resolved = resolve_registered_face(font_system, &ids, face_index);
            font_cache.store_meta(content_id, face_index, resolved.clone());
            resolved
        };
        apply_default_families(font_system, font_cache, &selected);
        return Ok(selected);
    }

    // Cache miss. If these are the resident bytes of a bundled `fonts/ui` font, the
    // faces are already in this system's database: adopt them instead of registering
    // a duplicate. No collision check runs on this path — nothing was added, so no
    // family became newly ambiguous, and tainting here would make every render with
    // the built-in font throw its pooled system away.
    if let Some(ids) = resident_ids_in(font_system, content.bytes()) {
        let selected = resolve_registered_face(font_system, &ids, face_index);
        font_cache.store_loaded(content_id, ids);
        font_cache.store_meta(content_id, face_index, selected.clone());
        apply_default_families(font_system, font_cache, &selected);
        return Ok(selected);
    }

    // Register the bytes into this system's db. fontdb takes the same erased
    // `Arc<dyn AsRef<[u8]>>` the content holds, so the buffer is shared, not copied.
    let source = fontdb::Source::Binary(content.data.clone());
    let loaded_ids = font_system.db_mut().load_font_source(source);
    if loaded_ids.is_empty() {
        return Err("fontdb не смог распарсить данные шрифта".to_string());
    }

    let selected = resolve_registered_face(font_system, &loaded_ids, face_index);
    // USER INTENT: the font the caller chose must win `Family::Name` matching over a
    // BUNDLED face that declares the same family. It cannot on its own — the base is
    // registered first, so its ids are lower and cosmic-text's primary pick takes the
    // first `weight_diff == 0` face of the family in id order. Drop the shadowing
    // bundled faces from THIS system's database clone; the system is then tainted
    // because it no longer holds the base it was built from.
    let displaced =
        displace_bundled_faces(font_system, font_cache, &loaded_ids, content.name.as_str());
    if displaced > 0 {
        font_cache.mark_tainted();
    }
    // Determinism guard: if a DIFFERENT already-loaded content declares the same
    // (family, weight, style, stretch), `Family::Name` matching becomes
    // history-dependent on this reused system. Taint it so the pool drops it and
    // never serves a future render (the residual is documented in the pool's
    // file header). Detect BEFORE storing this content's metadata so we only
    // compare against prior contents.
    if font_cache.collides_with_other_file(content_id, &selected) {
        font_cache.mark_tainted();
        ms_log::runtime_log::log_warn(format!(
            "render font family collision: content '{}' (face {face_index}) shares family '{}' \
             with an earlier font in the reused FontSystem; dropping the system after this render \
             to keep matching deterministic",
            content.name,
            selected.family_name.as_deref().unwrap_or("<none>"),
        ));
    }
    // `load_font_source` returns a `TinyVec`; store an owned `Vec` in the cache.
    font_cache.store_loaded(content_id, loaded_ids.to_vec());
    font_cache.store_meta(content_id, face_index, selected.clone());
    apply_default_families(font_system, font_cache, &selected);
    Ok(selected)
}

/// Thin compat wrapper over `load_font_content` for the app's forms-metric
/// measurement path, which still holds a file path and its own throwaway
/// `FontSystem`. Reads the file, hashes it into a `content_id` via
/// `font_content_id`, and delegates to `load_font_content`.
///
/// The synthesized `name`/`original_name` (the file stem) are irrelevant to the
/// renderer here — metric measurement only needs the resolved face metadata.
///
/// # Errors
/// Returns an error string if the file cannot be read or fontdb cannot parse it.
pub fn load_selected_font_from_path(
    font_system: &mut FontSystem,
    font_cache: &mut FontFaceCache,
    font_path: &Path,
    selected_face_index: usize,
) -> Result<RegisteredFontFace, String> {
    let bytes = fs::read(font_path).map_err(|error| {
        format!(
            "не удалось прочитать шрифт {}: {error}",
            font_path.display()
        )
    })?;
    let content_id = font_content_id(&bytes);
    let stem = font_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
        .to_string();
    let content = FontContent {
        name: stem.clone(),
        original_name: stem,
        data: Arc::new(bytes),
        face_index: selected_face_index,
        content_id,
    };
    load_font_content(font_system, font_cache, &content, selected_face_index)
}

/// Face ids under which `bytes` are ALREADY registered in `font_system`, or `None`
/// when they are not.
///
/// Answers only for the bundled `fonts/ui` buffers (`font_base::resident_face_ids`),
/// and every id is re-validated against THIS system's database: the face must still
/// exist and its source must be the very same buffer (address + length). fontdb face
/// ids are database-local keys, so a system built on a different database (the
/// throwaway metric system, an out-of-crate harness) must never be trusted to
/// interpret a base id — the source check is what makes that impossible.
fn resident_ids_in(font_system: &FontSystem, bytes: &[u8]) -> Option<Vec<fontdb::ID>> {
    let ids = crate::font_base::resident_face_ids(bytes)?;
    let db = font_system.db();
    let same_buffer = |face: &fontdb::FaceInfo| match &face.source {
        fontdb::Source::Binary(data) => {
            let registered: &[u8] = (**data).as_ref();
            std::ptr::eq(registered.as_ptr(), bytes.as_ptr()) && registered.len() == bytes.len()
        }
        fontdb::Source::File(_) | fontdb::Source::SharedFile(_, _) => false,
    };
    ids.iter()
        .all(|id| db.face(*id).is_some_and(same_buffer))
        .then(|| ids.to_vec())
}

/// Removes from `font_system`'s database every BUNDLED face that declares a family
/// name the just-registered faces `loaded_ids` also declare, and reports how many
/// were removed.
///
/// Why it must happen at all: cosmic-text resolves `Family::Name` to the FIRST
/// `font_weight_diff == 0` face of that family in face-id order
/// (`FontFallbackIter::default_font_match_key`, `font/fallback/mod.rs:275-283`). Face
/// ids are handed out in registration order and the bundled base is always registered
/// first, so a bundled face of the same family wins over the caller's font every time.
/// The user then picks "Noto Sans" (or any of the ~40 bundled families), the panel
/// resolves THEIR file, the loader registers it — and the render still uses the
/// bundled face, with different metrics and no diagnostic. fontdb cannot reorder ids,
/// so removing the shadowing faces is the only way to honour the choice.
///
/// Why the system must then be dropped: the removal is permanent for that database
/// clone. A later render leasing the same pooled system would find the bundled family
/// missing from the fallback chain, so the caller marks the cache tainted and
/// `font_system_pool::should_requeue` drops the system after this render.
///
/// Scope: only the faces the system already held before this cache loaded anything
/// (`FontFaceCache::capture_preexisting_faces`), i.e. the bundled base. A face that an
/// EARLIER render registered into the same pooled system is never touched — two caller
/// fonts sharing a family is the taint-only case (`collides_with_other_file`), because
/// both may be needed together as inline fonts in one render.
#[must_use]
fn displace_bundled_faces(
    font_system: &mut FontSystem,
    font_cache: &FontFaceCache,
    loaded_ids: &[fontdb::ID],
    content_name: &str,
) -> usize {
    let families: BTreeSet<String> = loaded_ids
        .iter()
        .filter_map(|id| font_system.db().face(*id))
        .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
        .collect();
    if families.is_empty() {
        return 0;
    }
    let shadowing: Vec<(fontdb::ID, BTreeSet<String>)> = font_system
        .db()
        .faces()
        .filter(|face| font_cache.is_preexisting_face(face.id))
        .filter_map(|face| {
            let shared: BTreeSet<String> = face
                .families
                .iter()
                .filter(|(name, _)| families.contains(name))
                .map(|(name, _)| name.clone())
                .collect();
            (!shared.is_empty()).then_some((face.id, shared))
        })
        .collect();
    if shadowing.is_empty() {
        return 0;
    }
    // Names of the faces actually removed, for a log line that says what changed.
    let shadowed_names: BTreeSet<&String> = shadowing
        .iter()
        .flat_map(|(_, names)| names.iter())
        .collect();

    let db = font_system.db_mut();
    for (id, _) in &shadowing {
        db.remove_face(*id);
    }
    ms_log::runtime_log::log_info(format!(
        "render font '{content_name}' declares bundled famil(y/ies) {}; {} bundled face(s) were \
         removed from this render's font database so the selected font wins the match, and the \
         FontSystem is dropped after this render instead of being pooled",
        shadowed_names
            .iter()
            .map(|name| format!("'{name}'"))
            .collect::<Vec<_>>()
            .join(", "),
        shadowing.len()
    ));
    shadowing.len()
}

/// Reads the face metadata (family/style/weight/stretch) of the face at
/// `selected_face_index` among `loaded_ids`, falling back to the first ID when
/// the index is out of range. Does not mutate default families.
fn resolve_registered_face(
    font_system: &FontSystem,
    loaded_ids: &[fontdb::ID],
    selected_face_index: usize,
) -> RegisteredFontFace {
    let mut selected = RegisteredFontFace {
        family_name: None,
        style: None,
        weight: None,
        stretch: None,
    };

    let Some(face_id) = loaded_ids
        .get(selected_face_index)
        .copied()
        .or_else(|| loaded_ids.first().copied())
    else {
        // Empty ID list is prevented by the caller, but stay panic-free.
        return selected;
    };
    if let Some(face) = font_system.db().face(face_id) {
        selected.family_name = face
            .families
            .first()
            .map(|(name, _)| name.clone())
            .or_else(|| {
                if face.post_script_name.is_empty() {
                    None
                } else {
                    Some(face.post_script_name.clone())
                }
            });
        selected.style = Some(face.style);
        selected.weight = Some(face.weight);
        selected.stretch = Some(face.stretch);
    }
    selected
}

/// Whether the family `attrs` name still has a face that passes cosmic-text's
/// HARD match filters, i.e. whether `attrs` can be served by the font the caller
/// actually selected.
///
/// This is the guard every attrs MODIFICATION must pass before it is applied.
/// `Attrs::matches` (`cosmic-text-0.14.2/src/attrs.rs:322-327`) filters faces on
/// EXACT `style` and `stretch` equality, and `FontSystem::get_font_matches`
/// (`system.rs:323-332`) builds the whole fallback iteration from that filtered
/// set. Setting a style the selected family does not ship therefore does not
/// "degrade" — it removes the selected font from the run entirely.
///
/// The family condition is NOT optional, and a plain
/// `db.faces().any(|face| attrs.matches(face))` is NOT a sufficient guard:
/// `Attrs::matches` short-circuits to `true` for ANY face whose PostScript name
/// contains "Emoji", and the bundled base ships `12-NotoEmoji-Regular`
/// (`fonts/ui/ext`). So the match set is never empty in production, yet a request
/// the selected family cannot serve resolves to Noto Emoji and renders the whole
/// run as `.notdef` tofu. Requiring a face OF THE REQUESTED FAMILY is what
/// actually reproduces cosmic-text's primary-face pick
/// (`FontFallbackIter::default_font_match_key`, `font/fallback/mod.rs:275-283`).
///
/// Weight is deliberately NOT part of the predicate: it is a ranking key inside
/// `matches`-passing faces, not a filter, so it can never empty the set.
///
/// Public (re-exported from the crate root) so out-of-crate render harnesses that
/// build their own `FontSystem` — today `src/bin/text_render_test` — can apply the
/// SAME guard instead of re-deriving these matching subtleties.
#[must_use]
pub fn family_has_matching_face(font_system: &FontSystem, attrs: &Attrs<'_>) -> bool {
    let db = font_system.db();
    let family_name = db.family_name(&attrs.family);
    db.faces().any(|face| {
        attrs.matches(face)
            && face
                .families
                .iter()
                .any(|(name, _)| name.as_str() == family_name)
    })
}

/// Whether the family `attrs` names ships a face of EXACTLY the weight `attrs`
/// requests, i.e. whether a real-weight request can be served by the caller's own font.
///
/// The weight analog of [`family_has_matching_face`], and it exists for a different
/// reason. Weight never empties the `Attrs::matches` set — it is a ranking key — but
/// cosmic-text's PRIMARY face pick still requires `font_weight_diff == 0`
/// (`FontFallbackIter::default_font_match_key`, `font/fallback/mod.rs:275-283`) and
/// does not rank down inside the family, and the script/common fallback passes filter
/// candidates the same way (`font_match_keys_iter(false)`, `fallback/mod.rs:410-417`).
/// So asking for a weight the selected family does not ship costs twice:
/// - the run jumps to whatever OTHER family happens to have a face at that exact
///   weight (on the bundled base: `Noto Sans Bold`), i.e. a different typeface;
/// - every fallback font of the bundle is a `Weight::NORMAL` face, so the script
///   chains become unreachable for that run and the rare planes render as tofu.
///
/// Both are silent. The renderer therefore asks this before putting `Weight::BOLD`
/// into the attrs and synthesizes faux bold instead when the answer is `false`
/// (`pipeline::synthesized_bold_params`).
///
/// The family condition is required for the same reason as in
/// [`family_has_matching_face`]: `Attrs::matches` short-circuits to `true` for any
/// face whose PostScript name contains "Emoji".
///
/// Public (re-exported from the crate root) so out-of-crate render harnesses that
/// build their own `FontSystem` can apply the same rule as production.
#[must_use]
pub fn family_has_face_of_requested_weight(font_system: &FontSystem, attrs: &Attrs<'_>) -> bool {
    let db = font_system.db();
    let family_name = db.family_name(&attrs.family);
    db.faces().any(|face| {
        attrs.matches(face)
            && face.weight == attrs.weight
            && face
                .families
                .iter()
                .any(|(name, _)| name.as_str() == family_name)
    })
}

/// Makes cosmic-text's generic-family matching deterministic on a reused
/// `FontSystem` regardless of pool history. Runs every render (cheap).
///
/// When `selected` has a family name, installs it as ALL five generic default
/// families so `Family::SansSerif`/etc. resolve to the selected font. When it has
/// NO family name, RESTORES the system's pristine defaults (captured at creation
/// in `font_cache`) so matching falls back to what a fresh `FontSystem` would use
/// instead of a prior render's family that still lingers in the reused db.
fn apply_default_families(
    font_system: &mut FontSystem,
    font_cache: &FontFaceCache,
    selected: &RegisteredFontFace,
) {
    if let Some(family) = selected.family_name.as_ref() {
        let db = font_system.db_mut();
        db.set_sans_serif_family(family.clone());
        db.set_serif_family(family.clone());
        db.set_monospace_family(family.clone());
        db.set_cursive_family(family.clone());
        db.set_fantasy_family(family.clone());
    } else {
        // No family name: a prior render's family may still be set as the
        // generic defaults on this reused system. Restore the pristine defaults
        // so identical params render identically regardless of pool history.
        font_cache.restore_pristine_defaults(font_system);
    }
}

/// Загружает файл шрифта в свежую `fontdb::Database` и возвращает реальное
/// PostScript-имя (OpenType name table id 6) выбранного face.
///
/// Зачем: Photoshop сопоставляет шрифт текстового слоя именно по PostScript-имени
/// (например `MaybugMSRegular`), а не по имени файла или UI-метке. Функция читает
/// это имя напрямую из данных шрифта, как бы файл ни назывался.
///
/// Robustness: при отсутствии/нечитаемости файла, непарсируемом шрифте или
/// выходе `face_index` за границы возвращает `None` (без паники) — экспорт идёт
/// в фоновом потоке и не должен падать.
#[must_use]
pub fn resolve_font_postscript_name(font_path: &str, face_index: usize) -> Option<String> {
    if font_path.is_empty() {
        return None;
    }
    let mut db = fontdb::Database::new();
    // load_font_file сам читает файл; ошибка чтения/парсинга -> None.
    db.load_font_file(font_path).ok()?;
    // Face'ы перечисляем так же, как `register_selected_font`: выбираем по
    // позиции среди загруженных, с откатом на первый при выходе за границы.
    let faces: Vec<_> = db.faces().collect();
    let face = faces.get(face_index).or_else(|| faces.first())?;
    if face.post_script_name.is_empty() {
        None
    } else {
        Some(face.post_script_name.clone())
    }
}

/// Имя семейства (OpenType name table id 1) выбранного face — фолбэк для PSD,
/// когда PostScript-имя недоступно. Та же robustness, что и у резолвера выше.
#[must_use]
pub fn resolve_font_family_name(font_path: &str, face_index: usize) -> Option<String> {
    if font_path.is_empty() {
        return None;
    }
    let mut db = fontdb::Database::new();
    db.load_font_file(font_path).ok()?;
    let faces: Vec<_> = db.faces().collect();
    let face = faces.get(face_index).or_else(|| faces.first())?;
    face.families
        .first()
        .map(|(name, _)| name.clone())
        .filter(|name| !name.is_empty())
}

#[must_use]
pub fn normalize_inline_font_label(label: &str) -> String {
    label.trim().to_ascii_lowercase()
}

/// Builds the inline-font registry for the requested labels, resolving each one
/// through the caller-supplied `fonts` provider and loading its content through
/// the shared `font_cache` so a reused `FontSystem` does not re-register duplicate
/// faces. Unknown names and load failures become warnings, not errors.
pub fn build_inline_font_registry(
    font_system: &mut FontSystem,
    font_cache: &mut FontFaceCache,
    fonts: &dyn FontProvider,
    requested_labels: &[String],
) -> InlineFontRegistryBuild {
    let requested_labels = requested_labels
        .iter()
        .map(|label| normalize_inline_font_label(label))
        .collect::<BTreeSet<_>>();
    if requested_labels.is_empty() {
        return InlineFontRegistryBuild::default();
    }

    let mut build = InlineFontRegistryBuild::default();
    for label in requested_labels {
        let Some(content) = fonts.resolve(&label) else {
            build.warnings.push(format!(
                "render_next inline style tag requested unknown font name '{label}'"
            ));
            continue;
        };

        match load_font_content(font_system, font_cache, &content, content.face_index) {
            Ok(face) => {
                build.registry.insert(label, face);
            }
            Err(error) => build.warnings.push(format!(
                "render_next failed to load inline font '{label}': {error}"
            )),
        }
    }

    build
}

#[cfg(test)]
mod tests {
    use super::{
        RegisteredFontFace, apply_default_families, family_has_face_of_requested_weight,
        family_has_matching_face, load_font_content,
    };
    use crate::font_base;
    use crate::font_provider::{FontContent, font_content_id};
    use crate::font_system_pool::FontFaceCache;
    use cosmic_text::{Attrs, Family, FontSystem, Metrics, Stretch, Style, Weight, fontdb};
    use std::path::PathBuf;
    use std::sync::Arc;

    /// Builds a `RegisteredFontFace` carrying only the given family name (no
    /// explicit style/weight/stretch), matching the metadata shape used on the
    /// no-attribute paths.
    fn face_with_family(family: Option<&str>) -> RegisteredFontFace {
        RegisteredFontFace {
            family_name: family.map(str::to_string),
            style: None,
            weight: None,
            stretch: None,
        }
    }

    #[test]
    fn apply_default_families_sets_named_and_restores_pristine() {
        // Built exactly like a pooled system (bundled base, no OS font scan), so the
        // captured pristine defaults are the ones production actually restores.
        let mut system = font_base::new_render_font_system();
        // Capture the pristine defaults the FRESH system uses, exactly as a
        // pooled system does at creation time.
        let cache = FontFaceCache::for_system(&system);
        let pristine_sans = system.db().family_name(&Family::SansSerif).to_string();
        let pristine_serif = system.db().family_name(&Family::Serif).to_string();
        let pristine_mono = system.db().family_name(&Family::Monospace).to_string();
        let pristine_cursive = system.db().family_name(&Family::Cursive).to_string();
        let pristine_fantasy = system.db().family_name(&Family::Fantasy).to_string();
        assert!(
            !pristine_sans.is_empty(),
            "a fresh FontSystem must expose a non-empty sans-serif default"
        );

        // Some(family): every generic default becomes that family.
        let named = face_with_family(Some("Ms Determinism Test Family"));
        apply_default_families(&mut system, &cache, &named);
        for family in [
            Family::SansSerif,
            Family::Serif,
            Family::Monospace,
            Family::Cursive,
            Family::Fantasy,
        ] {
            assert_eq!(
                system.db().family_name(&family),
                "Ms Determinism Test Family",
                "a named face must install its family as every generic default"
            );
        }

        // None: the pristine defaults are restored, undoing the prior render's
        // family so a nameless face matches fresh-system behavior.
        let nameless = face_with_family(None);
        apply_default_families(&mut system, &cache, &nameless);
        assert_eq!(
            system.db().family_name(&Family::SansSerif),
            pristine_sans,
            "a nameless face must restore the pristine sans-serif default"
        );
        assert_eq!(system.db().family_name(&Family::Serif), pristine_serif);
        assert_eq!(system.db().family_name(&Family::Monospace), pristine_mono);
        assert_eq!(system.db().family_name(&Family::Cursive), pristine_cursive);
        assert_eq!(system.db().family_name(&Family::Fantasy), pristine_fantasy);
    }

    /// The upright-only test fixture, the same one the pipeline/pool tests use.
    fn fixture_font_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf")
    }

    /// Builds a render `FontSystem` with the upright-only fixture registered as
    /// the selected font, and returns it with that face's base attrs metadata.
    fn system_with_fixture() -> Option<(FontSystem, RegisteredFontFace)> {
        let path = fixture_font_path();
        let bytes = std::fs::read(path).ok()?;
        let content_id = font_content_id(&bytes);
        let content = FontContent {
            name: "fixture".to_string(),
            original_name: "fixture".to_string(),
            data: Arc::new(bytes),
            face_index: 0,
            content_id,
        };
        let mut system = font_base::new_render_font_system();
        let mut cache = FontFaceCache::for_system(&system);
        let face = load_font_content(&mut system, &mut cache, &content, 0).ok()?;
        Some((system, face))
    }

    #[test]
    fn base_attrs_of_the_selected_face_always_match_it() {
        let Some((system, face)) = system_with_fixture() else {
            eprintln!("skipping base_attrs_of_the_selected_face_always_match_it: fixture missing");
            return;
        };
        let attrs = face.apply_to_attrs(Attrs::new().metrics(Metrics::new(32.0, 32.0)));
        assert!(
            family_has_matching_face(&system, &attrs),
            "the selected face must always satisfy its own base attrs"
        );

        // Why STRETCH can never empty the match set the way STYLE does, recorded
        // here rather than in prose: `stretch` is only ever copied FROM a
        // registered face (`RegisteredFontFace::apply_to_attrs`, and the inline
        // `<font=...>` branch of `apply_inline_style_to_attrs`), never
        // synthesized, so the face that supplied it always matches it back. The
        // renderer has no "force condensed" flag that could make attrs ask for a
        // stretch nobody ships — unlike `force_italic`, which asks for a style
        // out of thin air. Asserted both ways below.
        assert_eq!(
            attrs.stretch,
            face.stretch.unwrap_or(Stretch::Normal),
            "base attrs must carry exactly the selected face's own stretch"
        );
        let condensed = attrs.clone().stretch(Stretch::Condensed);
        assert!(
            !family_has_matching_face(&system, &condensed),
            "a stretch nobody ships would be unserviceable — no code path may request one"
        );
    }

    /// Face ids of every face in `system` declaring `family`.
    fn face_ids_of_family(system: &FontSystem, family: &str) -> Vec<fontdb::ID> {
        system
            .db()
            .faces()
            .filter(|face| face.families.iter().any(|(name, _)| name == family))
            .map(|face| face.id)
            .collect()
    }

    /// A font the caller selected must WIN `Family::Name` matching over a bundled
    /// face that declares the same family.
    ///
    /// The scenario is a user importing their own copy of one of the ~40 bundled
    /// families (a newer "Noto Sans" with different metrics, say). The panel resolves
    /// their file and the loader registers it, but cosmic-text picks the first
    /// `weight_diff == 0` face of the family in ID ORDER and the base is registered
    /// first — so before the displacement the render silently used the bundled face,
    /// with no warning and no taint. Regression guard for that.
    #[test]
    fn a_selected_font_wins_over_the_bundled_face_of_the_same_family() {
        let Some(shipped) = font_base::test_bundle::stack() else {
            eprintln!(
                "skipping a_selected_font_wins_over_the_bundled_face_of_the_same_family: fonts/ui \
                 is not present next to this checkout"
            );
            return;
        };
        let Some(bundled_font) = shipped.core.first() else {
            eprintln!("skipping: the shipped core tier is empty");
            return;
        };
        let Ok(bytes) = std::fs::read(&bundled_font.path) else {
            eprintln!("skipping: {} is unreadable", bundled_font.path.display());
            return;
        };
        let mut system =
            font_base::test_bundle::font_system().expect("the shipped stack was just resolved");
        let mut cache = FontFaceCache::for_system(&system);

        let family = bundled_font.family_name;
        let bundled_ids = face_ids_of_family(&system, family);
        assert!(
            !bundled_ids.is_empty(),
            "the bundle must ship at least one '{family}' face for this test to mean anything"
        );

        // The user's OWN copy: the same file read fresh, so the bytes are not the
        // resident `'static` buffer and the resident short-circuit cannot fire.
        let content_id = font_content_id(&bytes);
        let content = FontContent {
            name: "user-copy".to_string(),
            original_name: "user-copy".to_string(),
            data: Arc::new(bytes),
            face_index: 0,
            content_id,
        };
        let selected = load_font_content(&mut system, &mut cache, &content, 0)
            .expect("the user's own copy must load");
        assert_eq!(
            selected.family_name.as_deref(),
            Some(family),
            "the fixture for this test must declare the bundled family"
        );

        for id in &bundled_ids {
            assert!(
                system.db().face(*id).is_none(),
                "the bundled '{family}' face must be gone from this render's database"
            );
        }
        let remaining = face_ids_of_family(&system, family);
        let caller_ids = cache
            .loaded_ids(content_id)
            .expect("the caller's font was just registered")
            .to_vec();
        assert_eq!(
            remaining, caller_ids,
            "only the caller's own faces may answer to '{family}' afterwards"
        );

        // The system now differs from the base it was cloned from, so it must never
        // serve another render.
        assert!(
            cache.is_tainted(),
            "a system whose bundled faces were displaced must be dropped, not pooled"
        );
    }

    /// The displacement must be surgical: a caller font that shares no family name
    /// with the bundle leaves the base intact and the system reusable.
    #[test]
    fn an_unrelated_caller_font_displaces_nothing() {
        let Some(shipped) = font_base::test_bundle::stack() else {
            eprintln!(
                "skipping an_unrelated_caller_font_displaces_nothing: fonts/ui is not present \
                 next to this checkout"
            );
            return;
        };
        let Ok(bytes) = std::fs::read(fixture_font_path()) else {
            eprintln!("skipping an_unrelated_caller_font_displaces_nothing: fixture missing");
            return;
        };
        let mut system =
            font_base::test_bundle::font_system().expect("the shipped stack was just resolved");
        let mut cache = FontFaceCache::for_system(&system);
        let content_id = font_content_id(&bytes);
        let content = FontContent {
            name: "fixture".to_string(),
            original_name: "fixture".to_string(),
            data: Arc::new(bytes),
            face_index: 0,
            content_id,
        };

        load_font_content(&mut system, &mut cache, &content, 0).expect("fixture must load");

        assert_eq!(
            system.db().len(),
            shipped.file_count() + 1,
            "an unrelated font must only ADD its own face"
        );
        assert!(
            !cache.is_tainted(),
            "an unrelated font must leave the pooled system reusable"
        );
    }

    /// The weight guard must answer for the family the caller actually selected.
    #[test]
    fn a_family_without_a_bold_face_reports_no_bold_weight() {
        let Some((system, face)) = system_with_fixture() else {
            eprintln!("skipping a_family_without_a_bold_face_reports_no_bold_weight: fixture missing");
            return;
        };
        let attrs = face.apply_to_attrs(Attrs::new().metrics(Metrics::new(32.0, 32.0)));
        assert!(
            family_has_face_of_requested_weight(&system, &attrs),
            "the selected face must satisfy its own weight"
        );
        assert!(
            !family_has_face_of_requested_weight(&system, &attrs.clone().weight(Weight::BOLD)),
            "a family with no Bold file must not report a bold face"
        );
    }

    /// A family that DOES ship the requested weight must keep its real face — the
    /// guard must not degrade a legitimate bold request.
    #[test]
    fn a_family_with_a_bold_face_reports_the_bold_weight() {
        let Some(system) = font_base::test_bundle::font_system() else {
            eprintln!(
                "skipping a_family_with_a_bold_face_reports_the_bold_weight: fonts/ui is not \
                 present next to this checkout"
            );
            return;
        };
        // The bundle ships `bold/00-NotoSans-Bold.ttf` next to the regular face, so
        // "Noto Sans" is exactly the family that must NOT be degraded.
        let attrs = Attrs::new()
            .metrics(Metrics::new(32.0, 32.0))
            .family(Family::Name("Noto Sans"));
        assert!(
            family_has_face_of_requested_weight(&system, &attrs.clone().weight(Weight::BOLD)),
            "the bundled bold tier must satisfy a real bold request on its own family"
        );
    }

    /// End-to-end reason the bold guard exists: a REAL bold request on a family
    /// without a Bold file does not merely change typeface, it makes the whole
    /// script fallback chain unreachable for that run.
    ///
    /// Both halves are asserted against the SHIPPED bundle: at the weight the
    /// renderer keeps after degrading (the selected face's own 400) a rare Han
    /// ideograph reaches the rare-plane chain; at `Weight::BOLD` it cannot, because
    /// the script pass admits only `font_weight_diff == 0` candidates and every
    /// bundled fallback face is a 400 one.
    #[test]
    fn a_bold_request_on_a_boldless_family_would_lose_the_script_chain() {
        let Ok(bytes) = std::fs::read(fixture_font_path()) else {
            eprintln!(
                "skipping a_bold_request_on_a_boldless_family_would_lose_the_script_chain: \
                 fixture missing"
            );
            return;
        };
        let Some(mut system) = font_base::test_bundle::font_system() else {
            eprintln!(
                "skipping a_bold_request_on_a_boldless_family_would_lose_the_script_chain: \
                 fonts/ui is not present next to this checkout"
            );
            return;
        };
        let mut cache = FontFaceCache::for_system(&system);
        let content_id = font_content_id(&bytes);
        let content = FontContent {
            name: "fixture".to_string(),
            original_name: "fixture".to_string(),
            data: Arc::new(bytes),
            face_index: 0,
            content_id,
        };
        let face = load_font_content(&mut system, &mut cache, &content, 0)
            .expect("the fixture must load into a bundle-backed system");
        let attrs = face.apply_to_attrs(Attrs::new().metrics(Metrics::new(32.0, 32.0)));

        assert!(
            !family_has_face_of_requested_weight(&system, &attrs.clone().weight(Weight::BOLD)),
            "the fixture family ships no Bold face, so the renderer must degrade the request"
        );

        // A CJK Extension F ideograph: only the rare-plane fonts cover it.
        let rare = '\u{2CEA1}'.to_string();
        let kept = font_base::test_bundle::shaped_glyphs(&mut system, &rare, &attrs);
        assert!(
            kept.iter().all(|(glyph_id, _)| *glyph_id != 0),
            "at the kept weight the rare ideograph must reach the script chain, got {kept:?}"
        );

        let real_bold = font_base::test_bundle::shaped_glyphs(
            &mut system,
            &rare,
            &attrs.clone().weight(Weight::BOLD),
        );
        assert!(
            real_bold.iter().any(|(glyph_id, _)| *glyph_id == 0),
            "the avoided outcome changed: a real bold request now reaches the script chain \
             ({real_bold:?}); re-check `pipeline::synthesized_bold_params`"
        );
    }

    #[test]
    fn a_real_italic_request_is_unserviceable_on_an_upright_only_family() {
        let Some((system, face)) = system_with_fixture() else {
            eprintln!(
                "skipping a_real_italic_request_is_unserviceable_on_an_upright_only_family: \
                 fixture missing"
            );
            return;
        };
        let attrs = face.apply_to_attrs(Attrs::new().metrics(Metrics::new(32.0, 32.0)));
        assert!(
            !family_has_matching_face(&system, &attrs.clone().style(Style::Italic)),
            "an upright-only family must not report an italic face"
        );
    }

    /// The naive guard — "some face in the database matches" — is NOT enough,
    /// because `Attrs::matches` short-circuits to `true` for any face whose
    /// PostScript name contains "Emoji" and the bundled base ships
    /// `12-NotoEmoji-Regular`. Without the family condition the italic request
    /// above would look serviceable and then render the whole run as Noto Emoji
    /// `.notdef` tofu. Reads the repository bundle directly, because a test
    /// binary's working directory never resolves `ms_fonts::stack()`.
    #[test]
    fn the_emoji_exemption_makes_a_database_wide_match_check_useless() {
        let emoji_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fonts/ui/ext/12-NotoEmoji-Regular.ttf");
        let Ok(emoji_bytes) = std::fs::read(&emoji_path) else {
            eprintln!(
                "skipping the_emoji_exemption_makes_a_database_wide_match_check_useless: {} \
                 is not present next to this checkout",
                emoji_path.display()
            );
            return;
        };
        let Some((mut system, face)) = system_with_fixture() else {
            eprintln!(
                "skipping the_emoji_exemption_makes_a_database_wide_match_check_useless: \
                 fixture missing"
            );
            return;
        };
        let ids = system
            .db_mut()
            .load_font_source(fontdb::Source::Binary(Arc::new(emoji_bytes)));
        assert!(!ids.is_empty(), "the bundled emoji font must parse");

        let italic = face
            .apply_to_attrs(Attrs::new().metrics(Metrics::new(32.0, 32.0)))
            .style(Style::Italic);
        assert!(
            system.db().faces().any(|db_face| italic.matches(db_face)),
            "the emoji face makes a database-wide `matches` check pass for ANY attrs"
        );
        assert!(
            !family_has_matching_face(&system, &italic),
            "the family condition must still reject the request the selected font cannot serve"
        );
    }
}
