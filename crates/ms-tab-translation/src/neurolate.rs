/*
FILE OVERVIEW: crates/ms-tab-translation/src/neurolate.rs
The Translation tab's NeuroLate settings (per title) and the one owner of how they turn into the
parameters of a NeuroLate request (`ms_ai_api::neurolate::OcrParams`). Used by the text detector
mode and the OCR engine alike: both read the tab's ONE `NeurolateSettings` store.

Main types:
- `NeurolateLanguageChoice`: `Auto` (follow the MT source language, else Korean) or a fixed code.
- `NeurolateTypeChoice`: `Auto` (follow the project's comic type) or a fixed comic type.
- `NeurolateSettings`: language, comic type, the OCR engine's "create mask and text blocks" and
  the detector mode's "recognize text right away".

Key functions:
- `language_for_mt_source()`: MT source language code -> NeuroLate language, when one maps.
- `effective_language()`: the language a request uses (the "Auto" rule).
- `resolve_comic_type()`: the ONE owner of project comic type -> NeuroLate type.
- `request_params()`: settings + MT source + project type -> `OcrParams`.
- `settings_from_json()` / `NeurolateSettings::write_json()`: persistence under
  `OCR.params.neurolate.{language,type,detect_mask,detect_with_text}` of the title
  `settings.json`.

Submodules:
- `view`: `draw_neurolate_settings` (language and type pickers, optional detect-mask checkbox).

Notes:
Pure: no I/O, no logging, no locks; the persisted values are protocol-like keys (`"auto"`, the
language codes, `"webtoon"` / `"manga"`) and never localized.
*/

mod view;

pub(crate) use view::{NeurolateSettingsView, draw_neurolate_settings};

use ms_ai_api::neurolate::{NeurolateComicType, NeurolateLanguage, OcrParams};
use ms_project::ComicType;
use serde_json::{Map, Value};

/// Key of the NeuroLate object inside `OCR.params`.
pub(crate) const SETTINGS_KEY: &str = "neurolate";
/// Persisted value of an "Auto" choice (language and type alike).
const AUTO_KEY: &str = "auto";
/// The language a request uses when "Auto" finds no mapping of the MT source language.
pub(crate) const FALLBACK_LANGUAGE: NeurolateLanguage = NeurolateLanguage::Ko;

/// The persisted language choice: follow the MT source language, or a fixed language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum NeurolateLanguageChoice {
    /// The MT source language when it maps (`language_for_mt_source`), else Korean.
    #[default]
    Auto,
    Fixed(NeurolateLanguage),
}

impl NeurolateLanguageChoice {
    /// The persisted value: `"auto"` or the language code (`"KO"`, ...).
    #[must_use]
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Auto => AUTO_KEY,
            Self::Fixed(language) => language.code(),
        }
    }

    /// Parses a persisted value; anything unknown reads as `Auto`.
    #[must_use]
    pub(crate) fn from_key(raw: &str) -> Self {
        NeurolateLanguage::from_code(raw).map_or(Self::Auto, Self::Fixed)
    }
}

/// The persisted comic-type choice: follow the project, or a fixed type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum NeurolateTypeChoice {
    /// The project's comic type (`resolve_comic_type`).
    #[default]
    Auto,
    Webtoon,
    Manga,
}

impl NeurolateTypeChoice {
    /// Every choice, in the order the picker lists them.
    pub(crate) const ALL: [Self; 3] = [Self::Auto, Self::Webtoon, Self::Manga];

    /// The persisted value.
    #[must_use]
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Auto => AUTO_KEY,
            Self::Webtoon => "webtoon",
            Self::Manga => "manga",
        }
    }

    /// Parses a persisted value, case-insensitively; anything unknown reads as `Auto`.
    #[must_use]
    pub(crate) fn from_key(raw: &str) -> Self {
        Self::ALL.into_iter().find(|choice| choice.key().eq_ignore_ascii_case(raw.trim())).unwrap_or(Self::Auto)
    }
}

/// The tab's NeuroLate settings: the ONE store read by the detector panel, the OCR panel and
/// every NeuroLate request of the tab. Persisted per title with the OCR section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct NeurolateSettings {
    pub(crate) language: NeurolateLanguageChoice,
    pub(crate) comic_type: NeurolateTypeChoice,
    /// The OCR engine's "create mask and text blocks" parameter (a selection OCR also yields
    /// the detector mask and blocks from the same answer).
    pub(crate) detect_mask: bool,
    /// The detector mode's "recognize text right away": a page detection sends `MASK_AND_TEXT`
    /// and every detected block with text becomes a bubble (as block OCR would make it).
    pub(crate) detect_with_text: bool,
}

impl NeurolateSettings {
    /// Writes the settings into `params` (the `OCR.params` object) under `"neurolate"`,
    /// keeping any other key of that object.
    pub(crate) fn write_json(&self, params: &mut Map<String, Value>) {
        let mut section = params.get(SETTINGS_KEY).and_then(Value::as_object).cloned().unwrap_or_default();
        section.insert("language".to_string(), Value::String(self.language.key().to_string()));
        section.insert("type".to_string(), Value::String(self.comic_type.key().to_string()));
        section.insert("detect_mask".to_string(), Value::Bool(self.detect_mask));
        section.insert("detect_with_text".to_string(), Value::Bool(self.detect_with_text));
        params.insert(SETTINGS_KEY.to_string(), Value::Object(section));
    }
}

/// Reads the settings from `params` (the `OCR.params` object; `None` when the title has none).
/// An absent section or field, or a value of the wrong type, reads as its default, so a title
/// saved before NeuroLate existed (or by another title) never inherits the previous title's
/// choices.
#[must_use]
pub(crate) fn settings_from_json(params: Option<&Map<String, Value>>) -> NeurolateSettings {
    let defaults = NeurolateSettings::default();
    let Some(section) = params.and_then(|params| params.get(SETTINGS_KEY)).and_then(Value::as_object) else {
        return defaults;
    };
    NeurolateSettings {
        language: section.get("language").and_then(Value::as_str).map_or(defaults.language, NeurolateLanguageChoice::from_key),
        comic_type: section.get("type").and_then(Value::as_str).map_or(defaults.comic_type, NeurolateTypeChoice::from_key),
        detect_mask: section.get("detect_mask").and_then(Value::as_bool).unwrap_or(defaults.detect_mask),
        detect_with_text: section.get("detect_with_text").and_then(Value::as_bool).unwrap_or(defaults.detect_with_text),
    }
}

/// The NeuroLate language of an MT source language code (`MtPanelOptions::source_lang`), or
/// `None` when NeuroLate has none (`auto`, an unknown code). Both Chinese scripts map to `ZH`,
/// the only Chinese the API accepts; any other plain 2-letter code maps when the API lists it.
#[must_use]
pub(crate) fn language_for_mt_source(mt_source_lang: &str) -> Option<NeurolateLanguage> {
    let code = mt_source_lang.trim().to_ascii_lowercase();
    match code.as_str() {
        "zh-cn" | "zh-tw" | "zh" => Some(NeurolateLanguage::Zh),
        // `auto` and region-tagged forms are not language codes of the API.
        _ if code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_lowercase()) => NeurolateLanguage::from_code(&code),
        _ => None,
    }
}

/// The language a request uses: a fixed choice as is; `Auto` follows the MT source language
/// when it maps, else [`FALLBACK_LANGUAGE`] (Korean).
#[must_use]
pub(crate) fn effective_language(choice: NeurolateLanguageChoice, mt_source_lang: &str) -> NeurolateLanguage {
    match choice {
        NeurolateLanguageChoice::Fixed(language) => language,
        NeurolateLanguageChoice::Auto => language_for_mt_source(mt_source_lang).unwrap_or(FALLBACK_LANGUAGE),
    }
}

/// The comic type a request uses: a fixed choice as is; `Auto` follows the project — a
/// ribbon (vertical strip) is a webtoon, a paged comic is manga, and a custom or unknown layout
/// is treated as a webtoon. The one owner of this mapping.
#[must_use]
pub(crate) fn resolve_comic_type(choice: NeurolateTypeChoice, project_type: Option<ComicType>) -> NeurolateComicType {
    match choice {
        NeurolateTypeChoice::Webtoon => NeurolateComicType::Webtoon,
        NeurolateTypeChoice::Manga => NeurolateComicType::Manga,
        NeurolateTypeChoice::Auto => match project_type {
            Some(ComicType::Pages) => NeurolateComicType::Manga,
            Some(ComicType::Ribbon | ComicType::Custom) | None => NeurolateComicType::Webtoon,
        },
    }
}

/// The parameters of a NeuroLate request under `settings`, the MT source language and the
/// project's comic type.
#[must_use]
pub(crate) fn request_params(settings: &NeurolateSettings, mt_source_lang: &str, project_type: Option<ComicType>) -> OcrParams {
    OcrParams {
        language: effective_language(settings.language, mt_source_lang),
        comic_type: resolve_comic_type(settings.comic_type, project_type),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auto_type_follows_the_project_layout() {
        let auto = NeurolateTypeChoice::Auto;
        assert_eq!(resolve_comic_type(auto, Some(ComicType::Ribbon)), NeurolateComicType::Webtoon);
        assert_eq!(resolve_comic_type(auto, Some(ComicType::Pages)), NeurolateComicType::Manga);
        assert_eq!(resolve_comic_type(auto, Some(ComicType::Custom)), NeurolateComicType::Webtoon);
        assert_eq!(resolve_comic_type(auto, None), NeurolateComicType::Webtoon);
        for project in [Some(ComicType::Ribbon), Some(ComicType::Pages), Some(ComicType::Custom), None] {
            assert_eq!(resolve_comic_type(NeurolateTypeChoice::Webtoon, project), NeurolateComicType::Webtoon, "{project:?}");
            assert_eq!(resolve_comic_type(NeurolateTypeChoice::Manga, project), NeurolateComicType::Manga, "{project:?}");
        }
    }

    #[test]
    fn mt_source_languages_map_onto_neurolate_codes() {
        // Every MT source language except `auto` is one the API lists.
        for language in crate::panels::machine_translation::MT_SOURCE_LANGUAGES {
            let expected = match language.code {
                "auto" => None,
                "zh-cn" | "zh-tw" => Some("ZH".to_string()),
                code => Some(code.to_ascii_uppercase()),
            };
            assert_eq!(language_for_mt_source(language.code).map(|language| language.code().to_string()), expected, "{}", language.code);
        }
        assert_eq!(language_for_mt_source(" KO ").map(NeurolateLanguage::code), Some("KO"));
        for mt in ["auto", "", "kor", "pt-br", "xx", "und"] {
            assert_eq!(language_for_mt_source(mt), None, "{mt}");
        }
    }

    #[test]
    fn auto_language_falls_back_to_korean() {
        let auto = NeurolateLanguageChoice::Auto;
        assert_eq!(effective_language(auto, "auto"), NeurolateLanguage::Ko);
        assert_eq!(effective_language(auto, "xx"), NeurolateLanguage::Ko);
        assert_eq!(effective_language(auto, "it"), NeurolateLanguage::It);
        assert_eq!(effective_language(auto, "zh-tw"), NeurolateLanguage::Zh);
        assert_eq!(effective_language(auto, "ja"), NeurolateLanguage::Ja);
        assert_eq!(effective_language(NeurolateLanguageChoice::Fixed(NeurolateLanguage::Ru), "ja"), NeurolateLanguage::Ru);
    }

    #[test]
    fn request_params_combine_both_rules() {
        let settings = NeurolateSettings { language: NeurolateLanguageChoice::Auto, comic_type: NeurolateTypeChoice::Auto, detect_mask: false, detect_with_text: false };
        assert_eq!(
            request_params(&settings, "ja", Some(ComicType::Pages)),
            OcrParams { language: NeurolateLanguage::Ja, comic_type: NeurolateComicType::Manga }
        );
    }

    #[test]
    fn settings_round_trip_through_json_and_keep_foreign_keys() {
        let settings = NeurolateSettings {
            language: NeurolateLanguageChoice::Fixed(NeurolateLanguage::Th),
            comic_type: NeurolateTypeChoice::Manga,
            detect_mask: true,
            detect_with_text: true,
        };
        let mut params = json!({ "neurolate": { "custom": 1 }, "paddle": {} }).as_object().cloned().unwrap_or_default();
        settings.write_json(&mut params);
        assert_eq!(params["neurolate"], json!({ "custom": 1, "language": "TH", "type": "manga", "detect_mask": true, "detect_with_text": true }));
        assert!(params.contains_key("paddle"));
        assert_eq!(settings_from_json(Some(&params)), settings);
        // Each flag is its own key.
        let only_text = NeurolateSettings { detect_mask: false, ..settings };
        let mut fresh = serde_json::Map::new();
        only_text.write_json(&mut fresh);
        assert_eq!(settings_from_json(Some(&fresh)), only_text);
        for choice in NeurolateTypeChoice::ALL {
            assert_eq!(NeurolateTypeChoice::from_key(choice.key()), choice);
        }
        for language in NeurolateLanguage::ALL {
            let choice = NeurolateLanguageChoice::Fixed(language);
            assert_eq!(NeurolateLanguageChoice::from_key(choice.key()), choice);
        }
    }

    #[test]
    fn missing_or_unknown_values_read_as_defaults() {
        assert_eq!(settings_from_json(None), NeurolateSettings::default());
        for params in [
            json!({}),
            json!({ "neurolate": 3 }),
            json!({ "neurolate": { "language": "kor", "type": "comic", "detect_mask": "yes", "detect_with_text": 1 } }),
            json!({ "neurolate": { "language": 1, "type": null } }),
        ] {
            let map = params.as_object().cloned().unwrap_or_default();
            assert_eq!(settings_from_json(Some(&map)), NeurolateSettings::default(), "{params}");
        }
        let lower = json!({ "neurolate": { "language": "ja", "type": "WEBTOON" } });
        let parsed = settings_from_json(lower.as_object());
        assert_eq!(parsed.language, NeurolateLanguageChoice::Fixed(NeurolateLanguage::Ja));
        assert_eq!(parsed.comic_type, NeurolateTypeChoice::Webtoon);
    }
}
