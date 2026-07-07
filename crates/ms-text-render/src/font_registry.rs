/*
File: src/tabs/typing/render_next/font_registry.rs

Purpose:
Подсистема регистрации базового и inline-шрифтов нового рендера typing.

Main responsibilities:
- инкапсулировать загрузку выбранного font face;
- строить registry inline-шрифтов по label для rich-text path;
- отделить font registration от layout/raster pipeline;
- дедуплицировать загрузку шрифтов через `FontFaceCache`, чтобы переиспользуемая
  `FontSystem` из пула не накапливала дублирующиеся faces.

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
/// # Errors
/// Returns an error string if fontdb cannot parse the bytes.
pub fn load_font_content(
    font_system: &mut FontSystem,
    font_cache: &mut FontFaceCache,
    content: &FontContent,
    face_index: usize,
) -> Result<RegisteredFontFace, String> {
    let content_id = content.content_id;

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

    // Cache miss: register the bytes into this system's db. fontdb accepts an
    // `Arc<Vec<u8>>` as a binary source, so the content's `Arc` is reused without
    // copying the buffer.
    let source = fontdb::Source::Binary(content.data.clone());
    let loaded_ids = font_system.db_mut().load_font_source(source);
    if loaded_ids.is_empty() {
        return Err("fontdb не смог распарсить данные шрифта".to_string());
    }

    let selected = resolve_registered_face(font_system, &loaded_ids, face_index);
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
    use super::{RegisteredFontFace, apply_default_families};
    use crate::font_system_pool::FontFaceCache;
    use cosmic_text::{Family, FontSystem};

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
        let mut system = FontSystem::new();
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
}
