/*
File: crates/ms-text-util/src/language.rs

Purpose:
Typesetting language model shared by the line segmenter (this crate) and the
font-coverage checker in the application. A `TextLanguage` selects the
hyphenation/segmentation engine (via its `ScriptGroup`) and, later, the required
font charset. This module also owns the process-global "currently selected
typesetting language", mirroring the hanging-punctuation seam in
`text_punctuation`.

Key types:
- `ScriptGroup`  — the segmentation engine family a language maps to.
- `TextLanguage` — a concrete typesetting language (BCP-47-like tag).

Key functions:
- `set_text_language` / `text_language` — process-global selected language,
  backed by an `AtomicU8`. Default is `TextLanguage::Ru`, which preserves the
  historical behavior byte-for-byte.

Contract:
The crate is config-free: it never reads `user_config.json`. The application
seeds the user value at startup via `set_text_language`
(`main.rs::seed_text_language_from_config`). The u8 encoding used by the atomic
is defined by explicit `to_u8`/`from_u8` (no `as` casts), so the wire encoding
is stable and does not depend on enum layout.
*/

use std::sync::atomic::{AtomicU8, Ordering};

/// Segmentation/hyphenation engine family a `TextLanguage` maps to.
///
/// The concrete engine (Cyrillic-Slavic rules, Latin-Slavic rules, Romance TeX
/// patterns, or English) is chosen from this group. Adding a group forces every
/// dispatch site to be reconsidered (no catch-all arms in the codebase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScriptGroup {
    /// Cyrillic Slavic languages (Russian and relatives). Owns the historical
    /// Russian typographic rules (ь/ъ/й line-start rule, syllable/one-letter
    /// rules, preposition binding).
    CyrillicSlavic,
    /// Latin-script Slavic languages (Polish, Czech, and relatives).
    LatinSlavic,
    /// Romance languages (Spanish, French, Portuguese).
    Romance,
    /// English.
    English,
}

/// Languages of the Cyrillic-Slavic group, in the same stable order as
/// [`TextLanguage::all`]. The `languages`/partition contract is enforced by the
/// unit tests: every `TextLanguage` appears in exactly one group slice and each
/// slice member's [`TextLanguage::group`] equals the owning group.
const CYRILLIC_SLAVIC_LANGUAGES: [TextLanguage; 4] = [
    TextLanguage::Ru,
    TextLanguage::Uk,
    TextLanguage::Be,
    TextLanguage::Sr,
];
/// Languages of the Latin-Slavic group (see [`CYRILLIC_SLAVIC_LANGUAGES`]).
const LATIN_SLAVIC_LANGUAGES: [TextLanguage; 5] = [
    TextLanguage::Pl,
    TextLanguage::Cs,
    TextLanguage::Sk,
    TextLanguage::Sl,
    TextLanguage::Hr,
];
/// Languages of the Romance group (see [`CYRILLIC_SLAVIC_LANGUAGES`]).
const ROMANCE_LANGUAGES: [TextLanguage; 3] =
    [TextLanguage::Es, TextLanguage::Fr, TextLanguage::Pt];
/// The English group (see [`CYRILLIC_SLAVIC_LANGUAGES`]).
const ENGLISH_LANGUAGES: [TextLanguage; 1] = [TextLanguage::En];

impl ScriptGroup {
    /// All script groups, in a stable order (used by the settings selector and the
    /// partition test). Adding a variant is caught by the exhaustive matches in
    /// [`ScriptGroup::languages`] / [`ScriptGroup::first_language`].
    #[must_use]
    pub fn all() -> [ScriptGroup; 4] {
        [
            ScriptGroup::CyrillicSlavic,
            ScriptGroup::LatinSlavic,
            ScriptGroup::Romance,
            ScriptGroup::English,
        ]
    }

    /// The languages belonging to this group, in a stable order. Never empty (each
    /// group owns at least one language); the group→language partition is verified
    /// by the unit tests.
    #[must_use]
    pub fn languages(self) -> &'static [TextLanguage] {
        match self {
            ScriptGroup::CyrillicSlavic => &CYRILLIC_SLAVIC_LANGUAGES,
            ScriptGroup::LatinSlavic => &LATIN_SLAVIC_LANGUAGES,
            ScriptGroup::Romance => &ROMANCE_LANGUAGES,
            ScriptGroup::English => &ENGLISH_LANGUAGES,
        }
    }

    /// The first (default) language of this group. Selecting a group in the UI
    /// switches the active language to this one. Returned by value (never panics)
    /// via an exhaustive match rather than indexing a slice.
    #[must_use]
    pub fn first_language(self) -> TextLanguage {
        match self {
            ScriptGroup::CyrillicSlavic => TextLanguage::Ru,
            ScriptGroup::LatinSlavic => TextLanguage::Pl,
            ScriptGroup::Romance => TextLanguage::Es,
            ScriptGroup::English => TextLanguage::En,
        }
    }

    /// Human-readable group name in Russian, for the group selector. Total:
    /// every variant maps to a non-empty name.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            ScriptGroup::CyrillicSlavic => "Славянские (кириллица)",
            ScriptGroup::LatinSlavic => "Славянские (латиница)",
            ScriptGroup::Romance => "Романские",
            ScriptGroup::English => "Английский",
        }
    }

    /// Nominative Russian name of the writing system this group uses, for the
    /// font-coverage tooltip ("кириллица" / "латиница"). Total; non-empty.
    #[must_use]
    pub fn script_display_name(self) -> &'static str {
        match self {
            ScriptGroup::CyrillicSlavic => "кириллица",
            ScriptGroup::LatinSlavic | ScriptGroup::Romance | ScriptGroup::English => "латиница",
        }
    }
}

/// A concrete typesetting language. The tag is a lowercase BCP-47-style code and
/// is the stable identifier persisted in `user_config.json`
/// (`TextTab.text_language`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextLanguage {
    /// Russian.
    Ru,
    /// Ukrainian.
    Uk,
    /// Belarusian.
    Be,
    /// Serbian (Cyrillic script).
    Sr,
    /// Polish.
    Pl,
    /// Czech.
    Cs,
    /// Slovak.
    Sk,
    /// Slovenian.
    Sl,
    /// Croatian.
    Hr,
    /// Spanish.
    Es,
    /// French.
    Fr,
    /// Portuguese.
    Pt,
    /// English.
    En,
}

impl TextLanguage {
    /// All supported languages, in a stable order (used by UI enumeration and
    /// tests). Adding a variant here is required and is caught by the exhaustive
    /// `match` in [`TextLanguage::to_u8`].
    #[must_use]
    pub fn all() -> [TextLanguage; 13] {
        [
            TextLanguage::Ru,
            TextLanguage::Uk,
            TextLanguage::Be,
            TextLanguage::Sr,
            TextLanguage::Pl,
            TextLanguage::Cs,
            TextLanguage::Sk,
            TextLanguage::Sl,
            TextLanguage::Hr,
            TextLanguage::Es,
            TextLanguage::Fr,
            TextLanguage::Pt,
            TextLanguage::En,
        ]
    }

    /// Human-readable display name of this language, in Russian (the current UI
    /// language). Used by the typesetting-language selector and the font-coverage
    /// tooltip. Total: every variant maps to a non-empty name (exhaustive match).
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            TextLanguage::Ru => "Русский",
            TextLanguage::Uk => "Украинский",
            TextLanguage::Be => "Белорусский",
            TextLanguage::Sr => "Сербский",
            TextLanguage::Pl => "Польский",
            TextLanguage::Cs => "Чешский",
            TextLanguage::Sk => "Словацкий",
            TextLanguage::Sl => "Словенский",
            TextLanguage::Hr => "Хорватский",
            TextLanguage::Es => "Испанский",
            TextLanguage::Fr => "Французский",
            TextLanguage::Pt => "Португальский",
            TextLanguage::En => "Английский",
        }
    }

    /// Segmentation engine family this language belongs to.
    #[must_use]
    pub fn group(self) -> ScriptGroup {
        match self {
            TextLanguage::Ru | TextLanguage::Uk | TextLanguage::Be | TextLanguage::Sr => {
                ScriptGroup::CyrillicSlavic
            }
            TextLanguage::Pl
            | TextLanguage::Cs
            | TextLanguage::Sk
            | TextLanguage::Sl
            | TextLanguage::Hr => ScriptGroup::LatinSlavic,
            TextLanguage::Es | TextLanguage::Fr | TextLanguage::Pt => ScriptGroup::Romance,
            TextLanguage::En => ScriptGroup::English,
        }
    }

    /// Stable lowercase tag persisted in config and used by `from_tag`.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            TextLanguage::Ru => "ru",
            TextLanguage::Uk => "uk",
            TextLanguage::Be => "be",
            TextLanguage::Sr => "sr",
            TextLanguage::Pl => "pl",
            TextLanguage::Cs => "cs",
            TextLanguage::Sk => "sk",
            TextLanguage::Sl => "sl",
            TextLanguage::Hr => "hr",
            TextLanguage::Es => "es",
            TextLanguage::Fr => "fr",
            TextLanguage::Pt => "pt",
            TextLanguage::En => "en",
        }
    }

    /// Parses a config tag back into a language. Returns `None` for an unknown
    /// tag; callers must fall back explicitly (never panic).
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<TextLanguage> {
        let value = match tag {
            "ru" => TextLanguage::Ru,
            "uk" => TextLanguage::Uk,
            "be" => TextLanguage::Be,
            "sr" => TextLanguage::Sr,
            "pl" => TextLanguage::Pl,
            "cs" => TextLanguage::Cs,
            "sk" => TextLanguage::Sk,
            "sl" => TextLanguage::Sl,
            "hr" => TextLanguage::Hr,
            "es" => TextLanguage::Es,
            "fr" => TextLanguage::Fr,
            "pt" => TextLanguage::Pt,
            "en" => TextLanguage::En,
            _ => return None,
        };
        Some(value)
    }

    /// Stable u8 encoding for the process-global atomic. Explicit `match` rather
    /// than an `as` cast so the encoding never silently changes with enum layout.
    #[must_use]
    const fn to_u8(self) -> u8 {
        match self {
            TextLanguage::Ru => 0,
            TextLanguage::Uk => 1,
            TextLanguage::Be => 2,
            TextLanguage::Sr => 3,
            TextLanguage::Pl => 4,
            TextLanguage::Cs => 5,
            TextLanguage::Sk => 6,
            TextLanguage::Sl => 7,
            TextLanguage::Hr => 8,
            TextLanguage::Es => 9,
            TextLanguage::Fr => 10,
            TextLanguage::Pt => 11,
            TextLanguage::En => 12,
        }
    }

    /// Inverse of [`TextLanguage::to_u8`]. Returns `None` for an out-of-range
    /// byte so the reader can fall back to the default instead of panicking.
    #[must_use]
    const fn from_u8(raw: u8) -> Option<TextLanguage> {
        let value = match raw {
            0 => TextLanguage::Ru,
            1 => TextLanguage::Uk,
            2 => TextLanguage::Be,
            3 => TextLanguage::Sr,
            4 => TextLanguage::Pl,
            5 => TextLanguage::Cs,
            6 => TextLanguage::Sk,
            7 => TextLanguage::Sl,
            8 => TextLanguage::Hr,
            9 => TextLanguage::Es,
            10 => TextLanguage::Fr,
            11 => TextLanguage::Pt,
            12 => TextLanguage::En,
            _ => return None,
        };
        Some(value)
    }
}

/// Process-global selected typesetting language. Starts at `Ru` so that a fresh
/// process behaves exactly like the historical Russian-only segmenter until the
/// app seeds the user's choice.
static SELECTED_LANGUAGE: AtomicU8 = AtomicU8::new(TextLanguage::Ru.to_u8());

/// Sets the process-global typesetting language. Seeded by the app at startup
/// and updated when the user changes the setting. Cheap; safe from any thread.
pub fn set_text_language(language: TextLanguage) {
    SELECTED_LANGUAGE.store(language.to_u8(), Ordering::Release);
}

/// Returns the process-global typesetting language. Hot path: a single relaxed
/// atomic load. An unrecognized stored byte (impossible in practice) falls back
/// to `Ru`.
#[must_use]
pub fn text_language() -> TextLanguage {
    let raw = SELECTED_LANGUAGE.load(Ordering::Acquire);
    TextLanguage::from_u8(raw).unwrap_or(TextLanguage::Ru)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_round_trips_for_every_language() {
        for language in TextLanguage::all() {
            assert_eq!(TextLanguage::from_tag(language.tag()), Some(language));
        }
    }

    #[test]
    fn unknown_tag_is_none() {
        assert_eq!(TextLanguage::from_tag("xx"), None);
        assert_eq!(TextLanguage::from_tag(""), None);
        assert_eq!(TextLanguage::from_tag("RU"), None);
    }

    #[test]
    fn u8_encoding_round_trips_and_is_dense() {
        for language in TextLanguage::all() {
            assert_eq!(TextLanguage::from_u8(language.to_u8()), Some(language));
        }
        // Out-of-range byte falls back cleanly, never panics.
        assert_eq!(TextLanguage::from_u8(200), None);
    }

    #[test]
    fn selected_language_round_trips() {
        // Serialize against other tests that read the process-global language.
        let _guard = crate::segmentation::test_language_lock();
        let previous = text_language();
        for language in TextLanguage::all() {
            set_text_language(language);
            assert_eq!(text_language(), language);
        }
        set_text_language(previous);
    }

    #[test]
    fn every_language_is_in_exactly_one_group() {
        // Partition guard: each language must appear in exactly one group's slice,
        // and that slice's owning group must equal the language's own `group()`.
        for language in TextLanguage::all() {
            let owners: Vec<ScriptGroup> = ScriptGroup::all()
                .into_iter()
                .filter(|group| group.languages().contains(&language))
                .collect();
            assert_eq!(
                owners.len(),
                1,
                "language {language:?} must belong to exactly one group, found {owners:?}"
            );
            assert_eq!(owners[0], language.group());
        }
    }

    #[test]
    fn group_language_slices_are_non_empty_and_first_is_valid() {
        for group in ScriptGroup::all() {
            let languages = group.languages();
            assert!(!languages.is_empty(), "group {group:?} has no languages");
            // Changing to a group selects its first language, which must live in
            // that group.
            let first = group.first_language();
            assert_eq!(first, languages[0]);
            assert_eq!(first.group(), group);
        }
    }

    #[test]
    fn display_name_tables_are_total() {
        for language in TextLanguage::all() {
            assert!(!language.display_name().is_empty());
        }
        for group in ScriptGroup::all() {
            assert!(!group.display_name().is_empty());
            assert!(!group.script_display_name().is_empty());
        }
    }

    #[test]
    fn groups_partition_languages_as_documented() {
        assert_eq!(TextLanguage::Ru.group(), ScriptGroup::CyrillicSlavic);
        assert_eq!(TextLanguage::Uk.group(), ScriptGroup::CyrillicSlavic);
        assert_eq!(TextLanguage::Sr.group(), ScriptGroup::CyrillicSlavic);
        assert_eq!(TextLanguage::Pl.group(), ScriptGroup::LatinSlavic);
        assert_eq!(TextLanguage::Cs.group(), ScriptGroup::LatinSlavic);
        assert_eq!(TextLanguage::Es.group(), ScriptGroup::Romance);
        assert_eq!(TextLanguage::Fr.group(), ScriptGroup::Romance);
        assert_eq!(TextLanguage::En.group(), ScriptGroup::English);
    }
}
