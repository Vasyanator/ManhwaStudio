/*
File: panel/font_coverage.rs

Purpose:
Classify how well a font covers the writing system and language-specific
characters of the program's current UI language. Used to highlight fonts in the
create/edit font dropdowns (red = wrong script, yellow = missing some chars).

Main responsibilities:
- define the per-language character requirements (script alphabet + extra
  language/typography chars);
- classify a font (from its raw bytes + face index) into
  Full / Partial / Unsupported via the swash glyph charmap.

Notes:
Pure logic (no egui): the combobox picks colors/tooltips from the result. Only
Russian exists today; English/French/Spanish are planned once the UI is
localized (see `current_program_language`). Coverage is computed at font-load
time on a worker thread and cached on `FontEntry`.
*/

use swash::FontRef;

/// Level of support a font provides for the program's current language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FontLanguageSupport {
    /// Covers the writing system and every required language/typography char.
    Full,
    /// Covers the writing system but is missing some required characters.
    Partial,
    /// Does not cover the writing system at all (a wrong-script font).
    Unsupported,
}

/// Per-font coverage result for the current program language.
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

/// The program UI language whose writing system a font is checked against.
/// Only Russian exists today; English/French/Spanish are planned once the UI
/// gains localization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProgramLanguage {
    Russian,
}

/// Current program UI language. Hardcoded to Russian until UI localization
/// exists; this is the single seam to read the selected locale in the future.
pub(super) fn current_program_language() -> ProgramLanguage {
    ProgramLanguage::Russian
}

/// Character requirements for one program language.
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

/// Basic Russian/Cyrillic alphabet (32 letters, both cases), excluding `ё`
/// which is treated as a language-specific extra. Defines "supports Cyrillic".
const RUSSIAN_SCRIPT_CHARS: &[char] = &[
    'А', 'Б', 'В', 'Г', 'Д', 'Е', 'Ж', 'З', 'И', 'Й', 'К', 'Л', 'М', 'Н', 'О', 'П', 'Р', 'С', 'Т',
    'У', 'Ф', 'Х', 'Ц', 'Ч', 'Ш', 'Щ', 'Ъ', 'Ы', 'Ь', 'Э', 'Ю', 'Я', 'а', 'б', 'в', 'г', 'д', 'е',
    'ж', 'з', 'и', 'й', 'к', 'л', 'м', 'н', 'о', 'п', 'р', 'с', 'т', 'у', 'ф', 'х', 'ц', 'ч', 'ш',
    'щ', 'ъ', 'ы', 'ь', 'э', 'ю', 'я',
];

/// Russian-specific letters plus common Russian typography punctuation. Missing
/// any of these (with the script present) marks the font as partially supported.
const RUSSIAN_EXTRA_CHARS: &[char] = &['Ё', 'ё', '«', '»', '—', '–', '…', '№'];

/// Character requirements for `lang`.
fn language_spec(lang: ProgramLanguage) -> LanguageSpec {
    match lang {
        ProgramLanguage::Russian => LanguageSpec {
            script_chars: RUSSIAN_SCRIPT_CHARS,
            extra_chars: RUSSIAN_EXTRA_CHARS,
        },
    }
}

/// Classify a font's coverage of the current program language from raw font
/// bytes and the face index to inspect. Returns `Default` (treated as full
/// support) when the bytes cannot be parsed, so an unreadable font never shows a
/// false warning.
pub(super) fn classify_font_bytes(bytes: &[u8], face_index: usize) -> FontLanguageCoverage {
    classify_font_bytes_for(bytes, face_index, current_program_language())
}

/// Like [`classify_font_bytes`] but against an explicit program language.
pub(super) fn classify_font_bytes_for(
    bytes: &[u8],
    face_index: usize,
    lang: ProgramLanguage,
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
fn classify_with(is_covered: impl Fn(char) -> bool, lang: ProgramLanguage) -> FontLanguageCoverage {
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
        let cov = classify_with(covers(&latin), ProgramLanguage::Russian);
        assert_eq!(cov.support, FontLanguageSupport::Unsupported);
    }

    #[test]
    fn full_cyrillic_plus_extras_is_full() {
        let mut all: Vec<char> = RUSSIAN_SCRIPT_CHARS.to_vec();
        all.extend_from_slice(RUSSIAN_EXTRA_CHARS);
        let cov = classify_with(covers(&all), ProgramLanguage::Russian);
        assert_eq!(cov.support, FontLanguageSupport::Full);
        assert!(cov.missing.is_empty());
    }

    #[test]
    fn cyrillic_missing_only_extras_is_partial() {
        let script: Vec<char> = RUSSIAN_SCRIPT_CHARS.to_vec();
        let cov = classify_with(covers(&script), ProgramLanguage::Russian);
        assert_eq!(cov.support, FontLanguageSupport::Partial);
        // Every extra char is reported missing.
        assert_eq!(cov.missing.len(), RUSSIAN_EXTRA_CHARS.len());
        assert!(cov.missing.contains(&'ё'));
    }

    #[test]
    fn cyrillic_missing_a_few_letters_is_partial() {
        // Full alphabet + extras except drop one letter and one extra.
        let mut set: Vec<char> = RUSSIAN_SCRIPT_CHARS.to_vec();
        set.extend_from_slice(RUSSIAN_EXTRA_CHARS);
        set.retain(|&c| c != 'ъ' && c != '№');
        let cov = classify_with(covers(&set), ProgramLanguage::Russian);
        assert_eq!(cov.support, FontLanguageSupport::Partial);
        assert!(cov.missing.contains(&'ъ'));
        assert!(cov.missing.contains(&'№'));
    }
}
