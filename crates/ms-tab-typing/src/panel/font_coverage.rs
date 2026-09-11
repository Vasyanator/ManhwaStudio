/*
File: panel/font_coverage.rs

Purpose:
Classify how well a font covers the writing system and language-specific
characters of the currently selected TYPESETTING language
(`ms_text_util::language::text_language()`), which is independent of the UI
language. Used to highlight fonts in the create/edit font dropdowns
(red = wrong script, yellow = missing some chars).

Main responsibilities:
- map a `TextLanguage` to its character requirements: a script base alphabet
  (from the language's `ScriptGroup`) plus language-specific letters/typography;
- classify a font (from its raw bytes + face index) into
  Full / Partial / Unsupported via the swash glyph charmap.

Key types:
- `FontLanguageSupport` — Full / Partial / Unsupported result level.
- `FontLanguageCoverage` — result level plus the missing-character list.
- `LanguageSpec` — the (script base, language extras) requirement pair.

Key functions:
- `classify_font_bytes` — classify against the current typesetting language.
- `classify_font_bytes_for` — classify against an explicit `TextLanguage`.
- `classify_with` — the core, font-file-free decision logic (tested directly).

Notes:
Pure logic (no egui): the combobox picks colors/tooltips from the result. The
selected typesetting language is a process-global seeded at startup and (once a
settings selector exists) changed at runtime; because coverage is cached on
`FontEntry.coverage` at font-load time, a language change requires reloading the
font list to recompute it. `TypingTopPanelState` detects the change and triggers
that reload (see `panel/facade.rs`).

Relation to the renderer's FACTUAL diagnostic (`FontFallbackReport`, shown next to
the preview): the two answer DIFFERENT questions and both stay.
- This file is STATIC and PREDICTIVE: "could this FONT serve the selected
  typesetting LANGUAGE at all?" It measures one font file against a fixed alphabet,
  knows nothing about the text being typed, and runs for every font in the list
  off the GUI thread so the combo can rank the options BEFORE anything is typed.
- `FontFallbackReport` is FACTUAL and RETROSPECTIVE: "in this render of this text,
  which characters did another font draw, and which drew nothing?" It sees the real
  text and the real fallback chain, but only after a render and only for the one
  font that was used.
Note also that `Unsupported` here no longer means "the text will not appear":
since `dev-docs/unicode_base_font_plan.md` phase 4 the renderer always has a
deterministic fallback chain, so a wrong-script font yields text drawn in ANOTHER
font (which the factual report names), not blank space.
*/

use ms_text_util::language::{ScriptGroup, TextLanguage, text_language};
use swash::FontRef;

/// Level of support a font provides for the selected typesetting language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FontLanguageSupport {
    /// Covers the writing system and every required language/typography char.
    Full,
    /// Covers the writing system but is missing some required characters.
    Partial,
    /// Does not cover the writing system at all (a wrong-script font).
    Unsupported,
}

/// Per-font coverage result for the selected typesetting language.
#[derive(Clone, Debug)]
pub(super) struct FontLanguageCoverage {
    pub(super) support: FontLanguageSupport,
    /// Required characters the font is missing. Meaningful only for `Partial`
    /// (used to build the hover tooltip); empty for `Full`, and left unused for
    /// `Unsupported` (a wrong-script font is missing essentially everything).
    pub(super) missing: Vec<char>,
}

impl Default for FontLanguageCoverage {
    /// Unknown/unparseable fonts are treated as fully supported so the UI never
    /// shows a false warning.
    fn default() -> Self {
        Self {
            support: FontLanguageSupport::Full,
            missing: Vec::new(),
        }
    }
}

/// Character requirements for one typesetting language.
///
/// `script_chars` comes from the language's `ScriptGroup` (the writing-system
/// base alphabet, shared by every language in that group); `extra_chars` comes
/// from the concrete `TextLanguage` (its own letters plus its typography).
#[derive(Debug)]
struct LanguageSpec {
    /// Core alphabet of the writing system ("the script in general"). If a font
    /// covers fewer than half of these it is considered to lack the writing
    /// system entirely (`Unsupported`).
    script_chars: &'static [char],
    /// Additional characters required for this specific language and its common
    /// typography. Missing some of these (while the script is present) yields
    /// `Partial`.
    extra_chars: &'static [char],
}

/// Cyrillic script base: the Russian alphabet (32 letters, both cases), excluding
/// `ё` which is a language-specific extra. Defines "supports Cyrillic" for the
/// whole `CyrillicSlavic` group.
///
/// Design note: this base intentionally KEEPS the Russian/Belarusian-flavored
/// letters `ъ`/`ы`/`э`. Making it a strict common-Slavic-Cyrillic base (dropping
/// them) would require moving `ъ`/`ы`/`э` into Russian's extras, but Russian's
/// extras are frozen to preserve its historical classification result exactly, so
/// those letters must stay in the shared base. The accepted trade-off: a
/// Ukrainian/Belarusian/Serbian font that lacks `ъ`/`ы`/`э` is reported as
/// `Partial` for those languages even though they do not use those letters. This
/// is over-strict but never wrong about the writing system, and keeps Russian
/// byte-identical to the previous behavior.
const CYRILLIC_SCRIPT_CHARS: &[char] = &[
    'А', 'Б', 'В', 'Г', 'Д', 'Е', 'Ж', 'З', 'И', 'Й', 'К', 'Л', 'М', 'Н', 'О', 'П', 'Р', 'С', 'Т',
    'У', 'Ф', 'Х', 'Ц', 'Ч', 'Ш', 'Щ', 'Ъ', 'Ы', 'Ь', 'Э', 'Ю', 'Я', 'а', 'б', 'в', 'г', 'д', 'е',
    'ж', 'з', 'и', 'й', 'к', 'л', 'м', 'н', 'о', 'п', 'р', 'с', 'т', 'у', 'ф', 'х', 'ц', 'ч', 'ш',
    'щ', 'ъ', 'ы', 'ь', 'э', 'ю', 'я',
];

/// Latin script base: the basic ISO Latin alphabet (A-Z, a-z). Defines "supports
/// Latin" for the `LatinSlavic`, `Romance`, and `English` groups. Language-specific
/// diacritics live in each language's extras.
const LATIN_SCRIPT_CHARS: &[char] = &[
    'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S',
    'T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l',
    'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z',
];

/// Russian-specific letters plus common Russian typography punctuation. Frozen to
/// preserve the historical Russian classification result exactly.
const RU_EXTRA_CHARS: &[char] = &['Ё', 'ё', '«', '»', '—', '–', '…', '№'];

/// Ukrainian-specific letters (`і ї є ґ`, both cases). Ukrainian does NOT use
/// `ё`/`ъ`/`ы`/`э`; those remain Russian-flavored members of the shared Cyrillic
/// base (see `CYRILLIC_SCRIPT_CHARS`).
const UK_EXTRA_CHARS: &[char] = &['І', 'і', 'Ї', 'ї', 'Є', 'є', 'Ґ', 'ґ'];

/// Belarusian-specific letters (`і ў`, both cases).
const BE_EXTRA_CHARS: &[char] = &['І', 'і', 'Ў', 'ў'];

/// Serbian Cyrillic-specific letters (`ђ ј љ њ ћ џ`, both cases).
const SR_EXTRA_CHARS: &[char] = &[
    'Ђ', 'ђ', 'Ј', 'ј', 'Љ', 'љ', 'Њ', 'њ', 'Ћ', 'ћ', 'Џ', 'џ',
];

/// Polish-specific letters (`ą ć ę ł ń ó ś ź ż`, both cases).
const PL_EXTRA_CHARS: &[char] = &[
    'Ą', 'ą', 'Ć', 'ć', 'Ę', 'ę', 'Ł', 'ł', 'Ń', 'ń', 'Ó', 'ó', 'Ś', 'ś', 'Ź', 'ź', 'Ż', 'ż',
];

/// Czech-specific letters (`á č ď é ě í ň ó ř š ť ú ů ý ž`, both cases).
const CS_EXTRA_CHARS: &[char] = &[
    'Á', 'á', 'Č', 'č', 'Ď', 'ď', 'É', 'é', 'Ě', 'ě', 'Í', 'í', 'Ň', 'ň', 'Ó', 'ó', 'Ř', 'ř', 'Š',
    'š', 'Ť', 'ť', 'Ú', 'ú', 'Ů', 'ů', 'Ý', 'ý', 'Ž', 'ž',
];

/// Slovak-specific letters (`á ä č ď é í ĺ ľ ň ó ô ŕ š ť ú ý ž`, both cases).
const SK_EXTRA_CHARS: &[char] = &[
    'Á', 'á', 'Ä', 'ä', 'Č', 'č', 'Ď', 'ď', 'É', 'é', 'Í', 'í', 'Ĺ', 'ĺ', 'Ľ', 'ľ', 'Ň', 'ň', 'Ó',
    'ó', 'Ô', 'ô', 'Ŕ', 'ŕ', 'Š', 'š', 'Ť', 'ť', 'Ú', 'ú', 'Ý', 'ý', 'Ž', 'ž',
];

/// Slovenian-specific letters (`č š ž`, both cases). The standard Slovenian
/// alphabet adds only these three carons to the Latin base.
const SL_EXTRA_CHARS: &[char] = &['Č', 'č', 'Š', 'š', 'Ž', 'ž'];

/// Croatian-specific letters (`č ć đ š ž`, both cases). The digraphs `dž`/`lj`/`nj`
/// are composed from base Latin letters already in the script base, so only the
/// single-character letters are listed here.
const HR_EXTRA_CHARS: &[char] = &['Č', 'č', 'Ć', 'ć', 'Đ', 'đ', 'Š', 'š', 'Ž', 'ž'];

/// Spanish-specific letters and punctuation (`á é í ó ú ü ñ`, both cases, plus the
/// inverted marks `¿ ¡`).
const ES_EXTRA_CHARS: &[char] = &[
    'Á', 'á', 'É', 'é', 'Í', 'í', 'Ó', 'ó', 'Ú', 'ú', 'Ü', 'ü', 'Ñ', 'ñ', '¿', '¡',
];

/// French-specific letters (`à â ç é è ê ë î ï ô ù û ü ÿ œ`, both cases) plus the
/// French guillemets `« »`.
const FR_EXTRA_CHARS: &[char] = &[
    'À', 'à', 'Â', 'â', 'Ç', 'ç', 'É', 'é', 'È', 'è', 'Ê', 'ê', 'Ë', 'ë', 'Î', 'î', 'Ï', 'ï', 'Ô',
    'ô', 'Ù', 'ù', 'Û', 'û', 'Ü', 'ü', 'Ÿ', 'ÿ', 'Œ', 'œ', '«', '»',
];

/// Portuguese-specific letters (`ã õ á â à é ê í ó ô ú ç`, both cases).
const PT_EXTRA_CHARS: &[char] = &[
    'Ã', 'ã', 'Õ', 'õ', 'Á', 'á', 'Â', 'â', 'À', 'à', 'É', 'é', 'Ê', 'ê', 'Í', 'í', 'Ó', 'ó', 'Ô',
    'ô', 'Ú', 'ú', 'Ç', 'ç',
];

/// English typography only (em dash, en dash, ellipsis, curly double and single
/// quotes). English adds no letters beyond the Latin base.
const EN_EXTRA_CHARS: &[char] = &['—', '–', '…', '\u{201C}', '\u{201D}', '\u{2018}', '\u{2019}'];

/// Script base alphabet for a `ScriptGroup`. Every group maps to exactly one
/// writing-system base; a new group must be classified here explicitly.
fn script_chars_for_group(group: ScriptGroup) -> &'static [char] {
    match group {
        ScriptGroup::CyrillicSlavic => CYRILLIC_SCRIPT_CHARS,
        ScriptGroup::LatinSlavic | ScriptGroup::Romance | ScriptGroup::English => LATIN_SCRIPT_CHARS,
    }
}

/// Language-specific extra characters (own letters plus typography) for a
/// `TextLanguage`. A new language must add its set here explicitly.
fn extra_chars_for_language(lang: TextLanguage) -> &'static [char] {
    match lang {
        TextLanguage::Ru => RU_EXTRA_CHARS,
        TextLanguage::Uk => UK_EXTRA_CHARS,
        TextLanguage::Be => BE_EXTRA_CHARS,
        TextLanguage::Sr => SR_EXTRA_CHARS,
        TextLanguage::Pl => PL_EXTRA_CHARS,
        TextLanguage::Cs => CS_EXTRA_CHARS,
        TextLanguage::Sk => SK_EXTRA_CHARS,
        TextLanguage::Sl => SL_EXTRA_CHARS,
        TextLanguage::Hr => HR_EXTRA_CHARS,
        TextLanguage::Es => ES_EXTRA_CHARS,
        TextLanguage::Fr => FR_EXTRA_CHARS,
        TextLanguage::Pt => PT_EXTRA_CHARS,
        TextLanguage::En => EN_EXTRA_CHARS,
    }
}

/// Character requirements for `lang`: the group's script base plus the language's
/// own extras.
fn language_spec(lang: TextLanguage) -> LanguageSpec {
    LanguageSpec {
        script_chars: script_chars_for_group(lang.group()),
        extra_chars: extra_chars_for_language(lang),
    }
}

/// Classify a font's coverage of the current typesetting language from raw font
/// bytes and the face index to inspect. Returns `Default` (treated as full
/// support) when the bytes cannot be parsed, so an unreadable font never shows a
/// false warning.
pub(super) fn classify_font_bytes(bytes: &[u8], face_index: usize) -> FontLanguageCoverage {
    classify_font_bytes_for(bytes, face_index, text_language())
}

/// Like [`classify_font_bytes`] but against an explicit typesetting language.
pub(super) fn classify_font_bytes_for(
    bytes: &[u8],
    face_index: usize,
    lang: TextLanguage,
) -> FontLanguageCoverage {
    let Some(font) = FontRef::from_index(bytes, face_index) else {
        return FontLanguageCoverage::default();
    };
    let charmap = font.charmap();
    // Glyph id 0 is `.notdef`, i.e. the char is not covered by the font.
    classify_with(|ch| charmap.map(ch) != 0, lang)
}

/// Core classification over a coverage predicate. Split out so the decision
/// logic is testable without a real font file.
fn classify_with(is_covered: impl Fn(char) -> bool, lang: TextLanguage) -> FontLanguageCoverage {
    let spec = language_spec(lang);

    let mut script_covered = 0usize;
    let mut missing: Vec<char> = Vec::new();
    for &ch in spec.script_chars {
        if is_covered(ch) {
            script_covered += 1;
        } else {
            missing.push(ch);
        }
    }

    // A font that covers fewer than half of the core alphabet does not have this
    // writing system at all (e.g. a Latin-only font covers 0/64) -> Unsupported.
    let has_script = !spec.script_chars.is_empty() && script_covered * 2 >= spec.script_chars.len();
    if !has_script {
        return FontLanguageCoverage {
            support: FontLanguageSupport::Unsupported,
            missing: Vec::new(),
        };
    }

    for &ch in spec.extra_chars {
        if !is_covered(ch) {
            missing.push(ch);
        }
    }

    let support = if missing.is_empty() {
        FontLanguageSupport::Full
    } else {
        FontLanguageSupport::Partial
    };
    FontLanguageCoverage { support, missing }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn covers(set: &[char]) -> impl Fn(char) -> bool + '_ {
        move |ch| set.contains(&ch)
    }

    #[test]
    fn latin_only_font_is_unsupported() {
        let latin: Vec<char> = ('A'..='Z').chain('a'..='z').collect();
        let cov = classify_with(covers(&latin), TextLanguage::Ru);
        assert_eq!(cov.support, FontLanguageSupport::Unsupported);
    }

    #[test]
    fn full_cyrillic_plus_extras_is_full() {
        let mut all: Vec<char> = CYRILLIC_SCRIPT_CHARS.to_vec();
        all.extend_from_slice(RU_EXTRA_CHARS);
        let cov = classify_with(covers(&all), TextLanguage::Ru);
        assert_eq!(cov.support, FontLanguageSupport::Full);
        assert!(cov.missing.is_empty());
    }

    #[test]
    fn cyrillic_missing_only_extras_is_partial() {
        let script: Vec<char> = CYRILLIC_SCRIPT_CHARS.to_vec();
        let cov = classify_with(covers(&script), TextLanguage::Ru);
        assert_eq!(cov.support, FontLanguageSupport::Partial);
        // Every extra char is reported missing.
        assert_eq!(cov.missing.len(), RU_EXTRA_CHARS.len());
        assert!(cov.missing.contains(&'ё'));
    }

    #[test]
    fn cyrillic_missing_a_few_letters_is_partial() {
        // Full alphabet + extras except drop one letter and one extra.
        let mut set: Vec<char> = CYRILLIC_SCRIPT_CHARS.to_vec();
        set.extend_from_slice(RU_EXTRA_CHARS);
        set.retain(|&c| c != 'ъ' && c != '№');
        let cov = classify_with(covers(&set), TextLanguage::Ru);
        assert_eq!(cov.support, FontLanguageSupport::Partial);
        assert!(cov.missing.contains(&'ъ'));
        assert!(cov.missing.contains(&'№'));
    }

    #[test]
    fn cyrillic_only_font_is_unsupported_for_latin_language() {
        // Mirror of `latin_only_font_is_unsupported`: a Cyrillic font has no Latin
        // writing system, so a Latin-group language rejects it.
        let cyrillic: Vec<char> = CYRILLIC_SCRIPT_CHARS.to_vec();
        for lang in [TextLanguage::En, TextLanguage::Pl, TextLanguage::Fr] {
            let cov = classify_with(covers(&cyrillic), lang);
            assert_eq!(
                cov.support,
                FontLanguageSupport::Unsupported,
                "Cyrillic-only font must be Unsupported for {lang:?}"
            );
        }
    }

    #[test]
    fn latin_missing_polish_l_is_partial_for_pl_but_full_for_en() {
        // Latin base + every Polish extra EXCEPT ł/Ł + English typography.
        let mut set: Vec<char> = LATIN_SCRIPT_CHARS.to_vec();
        set.extend(PL_EXTRA_CHARS.iter().filter(|&&c| c != 'ł' && c != 'Ł'));
        set.extend_from_slice(EN_EXTRA_CHARS);

        let pl = classify_with(covers(&set), TextLanguage::Pl);
        assert_eq!(pl.support, FontLanguageSupport::Partial);
        assert!(pl.missing.contains(&'ł'));
        assert!(pl.missing.contains(&'Ł'));

        let en = classify_with(covers(&set), TextLanguage::En);
        assert_eq!(en.support, FontLanguageSupport::Full);
        assert!(en.missing.is_empty());
    }

    #[test]
    fn latin_plus_french_diacritics_is_full_for_fr_partial_for_es() {
        // Latin base + every French extra. French is complete; Spanish still needs
        // ñ and the inverted marks, which the French set does not provide.
        let mut set: Vec<char> = LATIN_SCRIPT_CHARS.to_vec();
        set.extend_from_slice(FR_EXTRA_CHARS);

        let fr = classify_with(covers(&set), TextLanguage::Fr);
        assert_eq!(fr.support, FontLanguageSupport::Full);
        assert!(fr.missing.is_empty());

        let es = classify_with(covers(&set), TextLanguage::Es);
        assert_eq!(es.support, FontLanguageSupport::Partial);
        assert!(es.missing.contains(&'ñ'));
        assert!(es.missing.contains(&'¿'));
        assert!(es.missing.contains(&'¡'));
    }

    #[test]
    fn every_language_has_a_non_empty_script_and_extra_set() {
        // Table-completeness guard: adding a `TextLanguage` variant without wiring
        // its requirements here fails the exhaustive matches at compile time; this
        // guards the values themselves at run time.
        for lang in TextLanguage::all() {
            let spec = language_spec(lang);
            assert!(
                !spec.script_chars.is_empty(),
                "empty script base for {lang:?}"
            );
            assert!(
                !spec.extra_chars.is_empty(),
                "empty extra set for {lang:?}"
            );
        }
    }
}
