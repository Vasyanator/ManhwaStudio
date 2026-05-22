/*
File: src/tabs/typing/render_next/font_registry.rs

Purpose:
Подсистема регистрации базового и inline-шрифтов нового рендера typing.

Main responsibilities:
- инкапсулировать загрузку выбранного font face;
- строить registry inline-шрифтов по label для rich-text path;
- отделить font registration от layout/raster pipeline;
- стать целевым местом для переноса логики `register_selected_font`.
*/

use super::types::InlineFontEntry;
use cosmic_text::{Attrs, Family, FontSystem, Stretch, Style, Weight, fontdb};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(crate) struct RegisteredFontFace {
    pub(crate) family_name: Option<String>,
    pub(crate) style: Option<Style>,
    pub(crate) weight: Option<Weight>,
    pub(crate) stretch: Option<Stretch>,
}

impl RegisteredFontFace {
    #[must_use]
    pub(crate) fn apply_to_attrs<'a>(&'a self, mut attrs: Attrs<'a>) -> Attrs<'a> {
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

pub(crate) type InlineFontRegistry = BTreeMap<String, RegisteredFontFace>;

#[derive(Debug, Default)]
pub(crate) struct InlineFontRegistryBuild {
    pub(crate) registry: InlineFontRegistry,
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn load_selected_font_from_path(
    font_system: &mut FontSystem,
    font_path: &Path,
    selected_face_index: usize,
) -> Result<RegisteredFontFace, String> {
    let font_bytes = fs::read(font_path).map_err(|error| {
        format!(
            "не удалось прочитать шрифт {}: {error}",
            font_path.display()
        )
    })?;
    register_selected_font(font_system, font_bytes, selected_face_index)
}

fn register_selected_font(
    font_system: &mut FontSystem,
    font_bytes: Vec<u8>,
    selected_face_index: usize,
) -> Result<RegisteredFontFace, String> {
    let source = fontdb::Source::Binary(Arc::new(font_bytes));
    let loaded_ids = font_system.db_mut().load_font_source(source);
    if loaded_ids.is_empty() {
        return Err("fontdb не смог распарсить файл шрифта".to_string());
    }

    let mut selected = RegisteredFontFace {
        family_name: None,
        style: None,
        weight: None,
        stretch: None,
    };

    let face_id = loaded_ids
        .get(selected_face_index)
        .copied()
        .unwrap_or(loaded_ids[0]);
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

    if let Some(family) = selected.family_name.as_ref() {
        let db = font_system.db_mut();
        db.set_sans_serif_family(family.clone());
        db.set_serif_family(family.clone());
        db.set_monospace_family(family.clone());
        db.set_cursive_family(family.clone());
        db.set_fantasy_family(family.clone());
    }

    Ok(selected)
}

#[must_use]
pub(crate) fn normalize_inline_font_label(label: &str) -> String {
    label.trim().to_ascii_lowercase()
}

pub(crate) fn build_inline_font_registry(
    font_system: &mut FontSystem,
    available_fonts: &[InlineFontEntry],
    requested_labels: &[String],
) -> InlineFontRegistryBuild {
    let requested_labels = requested_labels
        .iter()
        .map(|label| normalize_inline_font_label(label))
        .collect::<BTreeSet<_>>();
    if requested_labels.is_empty() {
        return InlineFontRegistryBuild::default();
    }

    let mut available_by_label = BTreeMap::<String, &InlineFontEntry>::new();
    for font in available_fonts {
        available_by_label.insert(normalize_inline_font_label(&font.label), font);
    }

    let mut build = InlineFontRegistryBuild::default();
    for label in requested_labels {
        let Some(entry) = available_by_label.get(&label).copied() else {
            build.warnings.push(format!(
                "render_next inline style tag requested unknown font label '{label}'"
            ));
            continue;
        };

        match load_selected_font_from_path(font_system, &entry.font_path, entry.face_index) {
            Ok(face) => {
                build.registry.insert(label, face);
            }
            Err(error) => build.warnings.push(format!(
                "render_next failed to load inline font '{}' from {}: {error}",
                entry.label,
                entry.font_path.display(),
            )),
        }
    }

    build
}
