/*
FILE OVERVIEW: crates/ms-tab-translation/src/neurolate/view.rs
The NeuroLate settings block shared by the text detector panel and the OCR panel: the language
and comic-type pickers (each with an "Auto (...)" first entry naming what Auto resolves to now),
for the OCR engine only the "create mask and text blocks" checkbox, and for the detector mode
only the "recognize text right away" checkbox.

Key items:
- `NeurolateSettingsView`: the read-only inputs the "Auto" entries resolve with.
- `draw_neurolate_settings()`: edits the tab's `NeurolateSettings` in place.

Notes:
Edits the ONE settings store of the tab (the caller lends it); resolves "Auto" through the
owners in `crate::neurolate` (`effective_language`, `resolve_comic_type`), so the label shows
exactly what a request would send. The language list is a catalog (58 codes), so it is a
`SearchableComboBox` (code plus, where the OCR-language table has one, the localized language
name, both searchable); the type picker is a `WheelComboBox`. Ids derive from the caller's
`id_salt`, so two panels drawing this block never share widget state.
*/

use super::{NeurolateLanguageChoice, NeurolateSettings, NeurolateTypeChoice, effective_language, resolve_comic_type};
use ms_ai_api::neurolate::{NeurolateComicType, NeurolateLanguage};
use ms_project::ComicType;
use ms_widgets::{RowLayout, SearchableComboBox, SearchableComboItem, WheelComboBox};

use crate::panels::ocr_langs::{EASYOCR_FULL_LANGUAGES, PADDLEOCR_FULL_LANGUAGES, lang_label};

/// What the "Auto" entries resolve with this frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NeurolateSettingsView<'a> {
    /// The MT panel's source language code (`MtPanelOptions::source_lang`).
    pub(crate) mt_source_lang: &'a str,
    /// The project's comic type (`ProjectData::comic_type`).
    pub(crate) project_type: Option<ComicType>,
    /// Draw the OCR engine's "create mask and text blocks" checkbox (OCR panel only).
    pub(crate) show_detect_mask: bool,
    /// Draw the detector mode's "recognize text right away" checkbox (detector panel only).
    pub(crate) show_detect_with_text: bool,
}

/// The localized name of a NeuroLate comic type.
fn comic_type_label(comic_type: NeurolateComicType) -> &'static str {
    match comic_type {
        NeurolateComicType::Webtoon => t!("translation.neurolate.type_webtoon_label"),
        NeurolateComicType::Manga => t!("translation.neurolate.type_manga_label"),
    }
}

/// The localized name of `language` from the OCR-language catalogs (keyed by the lowercase
/// ISO code), or `None` where they have no entry (then only the code is shown).
fn language_name(language: NeurolateLanguage) -> Option<&'static str> {
    let code = language.code().to_ascii_lowercase();
    EASYOCR_FULL_LANGUAGES.iter().chain(PADDLEOCR_FULL_LANGUAGES).find(|(wire, _)| *wire == code).map(|(_, key)| lang_label(key))
}

/// Draws the NeuroLate settings into `ui`, editing `settings` in place; returns whether the
/// user changed any of them this frame. `id_salt` must be unique per panel (the pickers'
/// ids are `"{id_salt}.language"` / `"{id_salt}.type"`). Starts no work.
pub(crate) fn draw_neurolate_settings(ui: &mut egui::Ui, id_salt: &str, settings: &mut NeurolateSettings, view: NeurolateSettingsView<'_>) -> bool {
    let mut changed = false;

    // Row 0 is "Auto (<code>)", then every language by its wire code (the short form the API
    // documents) with its localized name as the searchable second line where one is known.
    let auto_language = effective_language(NeurolateLanguageChoice::Auto, view.mt_source_lang);
    let mut language_idx = match settings.language {
        NeurolateLanguageChoice::Auto => 0,
        NeurolateLanguageChoice::Fixed(language) => NeurolateLanguage::ALL.iter().position(|&known| known == language).map_or(0, |idx| idx + 1),
    };
    let auto_text = tf!("translation.neurolate.language_auto_label", code = auto_language.code());
    let items: Vec<SearchableComboItem<'_>> = std::iter::once(SearchableComboItem::new(&auto_text))
        .chain(NeurolateLanguage::ALL.iter().map(|language| match language_name(*language) {
            Some(name) => SearchableComboItem::with_secondary(language.code(), name),
            None => SearchableComboItem::new(language.code()),
        }))
        .collect();
    let language_changed = ui
        .horizontal(|ui| {
            let response = SearchableComboBox::new(format!("{id_salt}.language")).row_layout(RowLayout::Wide).show(ui, &mut language_idx, &items);
            // The widget draws no label; this mirrors a combo's trailing label.
            ui.label(t!("translation.neurolate.language_label"));
            response.changed
        })
        .inner;
    if language_changed {
        settings.language = language_idx.checked_sub(1).and_then(|idx| NeurolateLanguage::ALL.get(idx)).map_or(NeurolateLanguageChoice::Auto, |&language| NeurolateLanguageChoice::Fixed(language));
        changed = true;
    }

    let auto_type = resolve_comic_type(NeurolateTypeChoice::Auto, view.project_type);
    let mut type_idx = NeurolateTypeChoice::ALL.iter().position(|&choice| choice == settings.comic_type).unwrap_or(0);
    let type_response = WheelComboBox::from_label(t!("translation.neurolate.type_label"))
        .id_salt(format!("{id_salt}.type"))
        .show_index(ui, &mut type_idx, NeurolateTypeChoice::ALL.len(), |idx| match NeurolateTypeChoice::ALL.get(idx).copied().unwrap_or_default() {
            NeurolateTypeChoice::Auto => tf!("translation.neurolate.type_auto_label", kind = comic_type_label(auto_type)),
            NeurolateTypeChoice::Webtoon => comic_type_label(NeurolateComicType::Webtoon).to_string(),
            NeurolateTypeChoice::Manga => comic_type_label(NeurolateComicType::Manga).to_string(),
        });
    if type_response.changed() {
        settings.comic_type = NeurolateTypeChoice::ALL.get(type_idx).copied().unwrap_or_default();
        changed = true;
    }

    if view.show_detect_mask {
        changed |= ui
            .checkbox(&mut settings.detect_mask, t!("translation.ocr_panel.detect_mask_label"))
            .on_hover_text(t!("translation.ocr_panel.detect_mask_tooltip"))
            .changed();
    }
    if view.show_detect_with_text {
        changed |= ui
            .checkbox(&mut settings.detect_with_text, t!("translation.text_detector_panel.neurolate_with_text_label"))
            .on_hover_text(tf!("translation.text_detector_panel.neurolate_with_text_tooltip", button = t!("translation.text_detector_panel.recognize_all_button")))
            .changed();
    }
    changed
}
