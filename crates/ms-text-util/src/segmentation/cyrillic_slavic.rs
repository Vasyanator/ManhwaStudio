/*
File: crates/ms-text-util/src/segmentation/cyrillic_slavic.rs

Purpose:
Cyrillic-Slavic implementation of the text segmenter (`ScriptGroup::CyrillicSlavic`:
Russian, Ukrainian, Belarusian, Serbian-Cyrillic). Owns the historical Russian
typographic rules — this is the byte-for-byte descendant of the former `ru.rs`,
generalized in which hyphenation dictionary is loaded (per `TextLanguage`) and in
the service-word binding lists, which are now dispatched per language
(Ru/Uk/Be/Sr). The boundary/syllable rules stay Russian-oriented for the whole
group; the Russian output is unchanged from the pre-refactor renderer (the Uk/Be/Sr
preposition and particle lists are best-effort, pending native-speaker review).
Script-neutral binding primitives (`normalize_binding_token`, the single-letter
orphan rule, the number+unit pair) live in `base` and are shared with
`latin_slavic`.

The dictionary (TeX patterns via the `hyphenation` crate) places syllable breaks
and knows prefixes/doubled consonants. On top of it we enforce typographic rules
the dictionary does not guarantee:

  • One-letter rule: a break may not leave a single letter at a line end/start.
    → head before the first break and tail after the last must have >= 2 letters.
  • Syllable rule: a part without a vowel is not a syllable.
    → head and tail must both contain a vowel.
  • ь/ъ/й may not start a NEW line (right of a break), but breaking AFTER them is
    fine — "силь-нее", "подъ-езд", "май-ка".
  • Monosyllabic words (one vowel) are not hyphenated: "стол", "край".
  • All-caps acronyms ("СССР", "HTML") and words with digits are not hyphenated.

Checking only the first and last dictionary break is enough: heads/tails only
accumulate letters/vowels toward the word interior, so if the edge breaks are
valid, the interior ones are too.

Key types:
- `CyrillicSlavicSegmenter` — `impl Segmenter` for the Cyrillic-Slavic group.

Wrap-facing helpers (consumed via `segmentation::rules` by the renderer's runtime
horizontal wrap; kept here so the group owns its own boundary policy):
- `contains_cyrillic`, `sanitize_breaks`, `is_safe_hyphen_boundary_at`
- `dictionary_split_is_valid`, `emergency_boundary_is_safe`, `avoid_emergency_split`
*/

use std::rc::Rc;

use super::base::{
    Conservatism, SOFT_HYPHEN, Segmenter, is_numeric_measure_pair, is_single_letter_binding,
    normalize_binding_token,
};
use super::dictionaries::HyphenationDictionaries;
use crate::language::TextLanguage;

/// Cyrillic-Slavic segmenter. Holds the shared, thread-local-cached hyphenation
/// dictionaries for its `language` (the language selects which primary TeX
/// dictionary is loaded; the typographic rules are common to the group).
#[derive(Debug)]
pub struct CyrillicSlavicSegmenter {
    language: TextLanguage,
    dicts: Rc<HyphenationDictionaries>,
}

impl CyrillicSlavicSegmenter {
    /// Builds a segmenter for `language` (must belong to
    /// `ScriptGroup::CyrillicSlavic`). Dictionaries come from the thread-local
    /// cache, so repeated construction does not reload the TeX patterns.
    #[must_use]
    pub fn new(language: TextLanguage) -> Self {
        Self {
            language,
            dicts: HyphenationDictionaries::for_language(language),
        }
    }

    /// The language this segmenter was built for.
    #[must_use]
    pub fn language(&self) -> TextLanguage {
        self.language
    }
}

impl Segmenter for CyrillicSlavicSegmenter {
    fn binding_conservatism(&self, left_token: &str, right_token: &str) -> Conservatism {
        binding_conservatism(self.language, left_token, right_token)
    }

    fn hyphenate_word(&self, word: &str) -> Option<String> {
        maybe_soft_hyphenate_word(word, &self.dicts)
    }

    fn hyphen_cost(&self, head_word: &str, tail_word: &str) -> u32 {
        classify_hyphen(head_word, tail_word).cost()
    }
}

// --- Dictionary soft hyphenation --------------------------------------------

fn maybe_soft_hyphenate_word(word: &str, dicts: &HyphenationDictionaries) -> Option<String> {
    if word.chars().count() < 4 {
        return None;
    }
    if word.contains("://") || word.contains('@') || word.contains('-') {
        return None;
    }
    if word.contains(SOFT_HYPHEN) {
        return None;
    }
    // Words with digits ("covid19", "3д") are not hyphenated — no reliable rules.
    if word.chars().any(|ch| ch.is_ascii_digit()) {
        return None;
    }
    // All-caps acronyms ("СССР", "HTML") are not hyphenated.
    if is_acronym_like(word) {
        return None;
    }
    // Monosyllabic words (one vowel) have nowhere to break.
    if count_vowels_visible(word) < 2 {
        return None;
    }

    let breaks = dicts.breaks_for_word(word);
    if breaks.is_empty() {
        return None;
    }

    Some(insert_soft_hyphens(word, breaks.as_slice()))
}

/// A word entirely of capital letters (acronym): at least two letters and no
/// lowercase among the alphabetic characters.
fn is_acronym_like(word: &str) -> bool {
    let mut alpha = 0usize;
    for ch in word.chars() {
        if ch.is_alphabetic() {
            alpha += 1;
            if !ch.is_uppercase() {
                return false;
            }
        }
    }
    alpha >= 2
}

fn insert_soft_hyphens(word: &str, breaks: &[usize]) -> String {
    let mut out = String::with_capacity(word.len() + breaks.len() * SOFT_HYPHEN.len_utf8());
    let mut tail_start = 0usize;
    for &idx in breaks {
        if idx <= tail_start || idx >= word.len() || !word.is_char_boundary(idx) {
            continue;
        }
        out.push_str(&word[tail_start..idx]);
        out.push(SOFT_HYPHEN);
        tail_start = idx;
    }
    out.push_str(&word[tail_start..]);
    out
}

// --- Hyphenation quality ----------------------------------------------------

#[derive(Clone, Copy)]
enum HyphenQuality {
    Good,
    Medium,
    Unpleasant,
}

impl HyphenQuality {
    fn cost(self) -> u32 {
        match self {
            HyphenQuality::Good => 2,
            HyphenQuality::Medium => 3,
            HyphenQuality::Unpleasant => 4,
        }
    }
}

fn alpha_count(text: &str) -> usize {
    text.chars().filter(|ch| ch.is_alphabetic()).count()
}

/// Typographic/linguistic quality of a dictionary break by the number of word
/// letters on each side of the split. A heuristic; easy to tweak.
fn classify_hyphen(head_word: &str, tail_word: &str) -> HyphenQuality {
    let head = alpha_count(head_word);
    let tail = alpha_count(tail_word);
    let min_side = head.min(tail);
    let total = head + tail;
    if min_side >= 3 {
        HyphenQuality::Good
    } else if min_side >= 2 && total >= 6 {
        HyphenQuality::Medium
    } else {
        HyphenQuality::Unpleasant
    }
}

// --- Safe break boundaries --------------------------------------------------

/// Minimum letters that may be left at a line end / carried over (one-letter
/// rule: a single letter is not allowed, so the threshold is two).
const MIN_EDGE_LETTERS: usize = 2;

/// Sanitizes raw dictionary break offsets by the Cyrillic-Slavic rules: drop
/// breaks that would put ь/ъ/й at a line start, then trim edges that violate the
/// one-letter/syllable rules. Interior breaks need no check (heads/tails only
/// grow inward).
pub(crate) fn sanitize_breaks(word: &str, mut breaks: Vec<usize>) -> Vec<usize> {
    // Per-position rule: ь/ъ/й must not stand to the right of a break.
    breaks.retain(|&idx| is_safe_boundary_for_dictionary_at(word, idx));
    breaks.sort_unstable();
    breaks.dedup();

    while let Some(&first) = breaks.first() {
        if is_breakable_edge(&word[..first]) {
            break;
        }
        breaks.remove(0);
    }
    while let Some(&last) = breaks.last() {
        if is_breakable_edge(&word[last..]) {
            break;
        }
        breaks.pop();
    }

    breaks
}

/// A word edge (head before the first break, or tail after the last) may stay on
/// a line: it has >= 2 letters and a vowel (i.e. forms a syllable).
fn is_breakable_edge(edge: &str) -> bool {
    count_alpha_chars(edge) >= MIN_EDGE_LETTERS && count_vowels_visible(edge) > 0
}

/// Whether an emergency/dictionary break right before byte `idx` keeps ь/ъ/й off
/// the new line start and does not split a consonant from a following vowel.
pub(crate) fn is_safe_hyphen_boundary_at(word: &str, idx: usize) -> bool {
    if idx == 0 || idx >= word.len() || !word.is_char_boundary(idx) {
        return false;
    }
    let left = word[..idx].chars().next_back();
    let right = word[idx..].chars().next();
    is_safe_hyphen_boundary(left, right)
}

fn is_safe_hyphen_boundary(left: Option<char>, right: Option<char>) -> bool {
    let (Some(left), Some(right)) = (left, right) else {
        return false;
    };
    // ь/ъ/й must not start a new line; breaking after them is fine.
    if matches!(right, 'ь' | 'Ь' | 'ъ' | 'Ъ' | 'й' | 'Й') {
        return false;
    }
    if is_cyrillic_consonant(left) && is_cyrillic_vowel(right) {
        return false;
    }
    true
}

fn is_safe_boundary_for_dictionary(left: Option<char>, right: Option<char>) -> bool {
    let (Some(_left), Some(right)) = (left, right) else {
        return false;
    };
    // ь/ъ/й must not start a new line; breaking after them is fine.
    !matches!(right, 'ь' | 'Ь' | 'ъ' | 'Ъ' | 'й' | 'Й')
}

fn is_safe_boundary_for_dictionary_at(word: &str, idx: usize) -> bool {
    if idx == 0 || idx >= word.len() || !word.is_char_boundary(idx) {
        return false;
    }
    let left = word[..idx].chars().next_back();
    let right = word[idx..].chars().next();
    is_safe_boundary_for_dictionary(left, right)
}

// --- Character counting / Cyrillic classes ----------------------------------

pub(crate) fn count_alpha_chars(text: &str) -> usize {
    text.chars()
        .filter(|ch| ch.is_alphabetic() && *ch != SOFT_HYPHEN)
        .count()
}

pub(crate) fn count_vowels_visible(text: &str) -> usize {
    text.chars()
        .filter(|&ch| {
            ch != SOFT_HYPHEN
                && (is_cyrillic_vowel(ch)
                    || matches!(
                        ch,
                        'a' | 'e' | 'i' | 'o' | 'u' | 'A' | 'E' | 'I' | 'O' | 'U'
                    ))
        })
        .count()
}

/// Whether `word` contains any Cyrillic letter (used to gate the vowel rules).
pub(crate) fn contains_cyrillic(word: &str) -> bool {
    word.chars().any(|ch| {
        let cp = u32::from(ch);
        matches!(cp, 0x0400..=0x052F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F)
    })
}

fn contains_latin(word: &str) -> bool {
    word.chars().any(|ch| ch.is_ascii_alphabetic())
}

/// Blocks/words that should not be split by an emergency hyphen.
pub(crate) fn avoid_emergency_split(text: &str) -> bool {
    let normalized = text.replace(SOFT_HYPHEN, "");
    if normalized.is_empty() {
        return true;
    }
    // A block that already contains whitespace has a normal word-wrap point; it must
    // never be emergency-hyphenated (that would insert a hyphen at an existing space).
    if normalized.chars().any(char::is_whitespace) {
        return true;
    }
    if normalized.contains("://") || normalized.contains('@') {
        return true;
    }
    if contains_cyrillic(normalized.as_str()) && contains_latin(normalized.as_str()) {
        return true;
    }
    if normalized.chars().any(|ch| ch.is_ascii_digit())
        && normalized.chars().any(char::is_alphabetic)
    {
        return true;
    }
    let alpha_count = normalized.chars().filter(|ch| ch.is_alphabetic()).count();
    if alpha_count > 1
        && normalized
            .chars()
            .filter(|ch| ch.is_alphabetic())
            .all(|ch| !contains_cyrillic(ch.encode_utf8(&mut [0; 4])) && ch.is_uppercase())
    {
        return true;
    }
    normalized.contains('.')
}

/// Whether a dictionary break of `word` at byte offset `boundary` keeps enough
/// letters (and, for Cyrillic words, a vowel) on each side. Runtime horizontal
/// wrap re-checks each dictionary break with this.
pub(crate) fn dictionary_split_is_valid(word: &str, boundary: usize) -> bool {
    if count_alpha_chars(&word[..boundary]) < 2 || count_alpha_chars(&word[boundary..]) < 2 {
        return false;
    }
    if contains_cyrillic(word)
        && (count_vowels_visible(&word[..boundary]) < 1
            || count_vowels_visible(&word[boundary..]) < 1)
    {
        return false;
    }
    true
}

/// Whether an emergency (non-dictionary) break right before byte `boundary` in
/// `text` is allowed under the Cyrillic-Slavic boundary and syllable rules.
pub(crate) fn emergency_boundary_is_safe(text: &str, boundary: usize) -> bool {
    is_safe_hyphen_boundary_at(text, boundary)
        && count_alpha_chars(&text[..boundary]) >= 2
        && count_alpha_chars(&text[boundary..]) >= 2
        && (!contains_cyrillic(text)
            || (count_vowels_visible(&text[..boundary]) >= 1
                && count_vowels_visible(&text[boundary..]) >= 1))
        && count_vowels_visible(&text[boundary..]) >= 1
}

fn is_cyrillic_vowel(ch: char) -> bool {
    matches!(
        ch,
        'а' | 'е'
            | 'ё'
            | 'и'
            | 'о'
            | 'у'
            | 'ы'
            | 'э'
            | 'ю'
            | 'я'
            | 'А'
            | 'Е'
            | 'Ё'
            | 'И'
            | 'О'
            | 'У'
            | 'Ы'
            | 'Э'
            | 'Ю'
            | 'Я'
    )
}

fn is_cyrillic_consonant(ch: char) -> bool {
    contains_cyrillic(ch.encode_utf8(&mut [0; 4]))
        && ch.is_alphabetic()
        && !is_cyrillic_vowel(ch)
        && !matches!(ch, 'ь' | 'Ь' | 'ъ' | 'Ъ')
}

// --- Word binding rules -----------------------------------------------------

/// Conservatism category of a break between two tokens for `language` (one of the
/// four Cyrillic-Slavic languages). `Safe` — an ordinary space (free to break);
/// higher — a service binding whose separation is riskier the higher the class.
/// The number+unit, single-letter and abbreviation rules are common to the group;
/// only the preposition/particle lists are language-dispatched. For Russian the
/// result is byte-identical to the pre-refactor renderer (golden tests).
fn binding_conservatism(
    language: TextLanguage,
    left_token: &str,
    right_token: &str,
) -> Conservatism {
    // "Number + unit" ("5 кг") is the riskiest to break. Judged on RAW tokens:
    // normalization below would strip the digits and blank the left token.
    if is_numeric_measure_pair(left_token, right_token) {
        return Conservatism::Reckless;
    }

    let left = normalize_binding_token(left_token);
    let right = normalize_binding_token(right_token);
    if left.is_empty() || right.is_empty() {
        return Conservatism::Safe;
    }

    // A single-letter preposition/conjunction ("в дом", "к нам") is risky to strip.
    if is_single_letter_binding(left.as_str()) {
        return Conservatism::Reckless;
    }
    // Dictionary prepositions/conjunctions: short (2 letters) split more boldly.
    if is_nonbreaking_prefix_word(language, left.as_str()) {
        return if left.chars().count() <= 2 {
            Conservatism::Bold
        } else {
            Conservatism::Relaxed
        };
    }
    // Trailing particle ("же", "ли", "бы", "ка") clings to the previous word.
    if is_nonbreaking_suffix_particle(language, right.as_str()) {
        return Conservatism::Bold;
    }
    // Abbreviation with a dot ("стр.", "ул. Ленина"). Russian-oriented and applied
    // group-wide: it only raises break COST, never correctness.
    if is_nonbreaking_abbreviation(left_token) {
        return Conservatism::Relaxed;
    }
    Conservatism::Safe
}

/// Whether `word` (already normalized/lowercased) is a preposition or conjunction
/// that should not be orphaned at a line end, for the given Cyrillic-Slavic
/// `language`. Non-Cyrillic languages never construct a `CyrillicSlavicSegmenter`,
/// so they map to `false` (no Cyrillic service words) instead of panicking.
fn is_nonbreaking_prefix_word(language: TextLanguage, word: &str) -> bool {
    match language {
        TextLanguage::Ru => is_russian_prefix_word(word),
        TextLanguage::Uk => is_ukrainian_prefix_word(word),
        TextLanguage::Be => is_belarusian_prefix_word(word),
        TextLanguage::Sr => is_serbian_prefix_word(word),
        TextLanguage::Pl
        | TextLanguage::Cs
        | TextLanguage::Sk
        | TextLanguage::Sl
        | TextLanguage::Hr
        | TextLanguage::Es
        | TextLanguage::Fr
        | TextLanguage::Pt
        | TextLanguage::En => false,
    }
}

/// Whether `word` (already normalized/lowercased) is a trailing particle that
/// clings to the previous word, for the given Cyrillic-Slavic `language`. See
/// [`is_nonbreaking_prefix_word`] for the non-Cyrillic mapping.
fn is_nonbreaking_suffix_particle(language: TextLanguage, word: &str) -> bool {
    match language {
        TextLanguage::Ru => is_russian_suffix_particle(word),
        TextLanguage::Uk => is_ukrainian_suffix_particle(word),
        TextLanguage::Be => is_belarusian_suffix_particle(word),
        TextLanguage::Sr => is_serbian_suffix_particle(word),
        TextLanguage::Pl
        | TextLanguage::Cs
        | TextLanguage::Sk
        | TextLanguage::Sl
        | TextLanguage::Hr
        | TextLanguage::Es
        | TextLanguage::Fr
        | TextLanguage::Pt
        | TextLanguage::En => false,
    }
}

/// Russian prepositions/conjunctions. BYTE-IDENTICAL to the pre-refactor list —
/// guarded by `russian_segmentation_is_bit_identical`; do not edit.
fn is_russian_prefix_word(word: &str) -> bool {
    matches!(
        word,
        "не" | "ни"
            | "без"
            | "безо"
            | "для"
            | "при"
            | "про"
            | "через"
            | "перед"
            | "пред"
            | "но"
            | "да"
            | "или"
            | "либо"
            | "в"
            | "во"
            | "к"
            | "ко"
            | "с"
            | "со"
            | "у"
            | "о"
            | "об"
            | "обо"
            | "от"
            | "до"
            | "по"
            | "за"
            | "подо"
            | "из"
            | "изо"
            | "на"
            | "над"
            | "под"
    )
}

/// Russian trailing particles. BYTE-IDENTICAL to the pre-refactor list — guarded
/// by `russian_segmentation_is_bit_identical`; do not edit.
fn is_russian_suffix_particle(word: &str) -> bool {
    matches!(word, "же" | "ли" | "ль" | "бы" | "б" | "ка" | "де" | "то")
}

// Best-effort service-word list for Ukrainian: common prepositions and
// conjunctions. It only affects break COST (never correctness); a native speaker
// should review it before it is treated as authoritative.
fn is_ukrainian_prefix_word(word: &str) -> bool {
    matches!(
        word,
        "без" | "для"
            | "при"
            | "про"
            | "через"
            | "перед"
            | "від"
            | "до"
            | "за"
            | "на"
            | "над"
            | "під"
            | "по"
            | "як"
            | "що"
            | "чи"
            | "але"
            | "не"
            | "ні"
    )
}

// Best-effort trailing-particle list for Ukrainian. Cost-only; native review
// pending.
fn is_ukrainian_suffix_particle(word: &str) -> bool {
    matches!(word, "же" | "ж" | "би" | "б")
}

// Best-effort service-word list for Belarusian: common prepositions and
// conjunctions. Cost-only; native review pending.
fn is_belarusian_prefix_word(word: &str) -> bool {
    matches!(
        word,
        "без" | "для"
            | "пры"
            | "праз"
            | "перад"
            | "ад"
            | "да"
            | "за"
            | "на"
            | "над"
            | "пад"
            | "па"
            | "як"
            | "што"
            | "ці"
            | "але"
            | "не"
            | "ня"
    )
}

// Best-effort trailing-particle list for Belarusian. Cost-only; native review
// pending.
fn is_belarusian_suffix_particle(word: &str) -> bool {
    matches!(word, "ж" | "бы" | "б" | "жа")
}

// Best-effort service-word list for Serbian (Cyrillic): common prepositions and
// conjunctions. Cost-only; native review pending.
fn is_serbian_prefix_word(word: &str) -> bool {
    matches!(
        word,
        "без" | "за"
            | "на"
            | "над"
            | "под"
            | "при"
            | "пре"
            | "кроз"
            | "али"
            | "до"
            | "од"
            | "из"
            | "као"
            | "да"
            | "не"
            | "или"
    )
}

// Best-effort trailing-particle/clitic list for Serbian (Cyrillic). Cost-only;
// native review pending.
fn is_serbian_suffix_particle(word: &str) -> bool {
    matches!(word, "ли" | "се" | "би" | "ће")
}

fn is_nonbreaking_abbreviation(token: &str) -> bool {
    let trimmed = token.trim();
    if !trimmed.ends_with('.') {
        return false;
    }
    let core = trimmed
        .trim_end_matches('.')
        .trim_matches(|ch: char| !ch.is_alphabetic())
        .to_lowercase();
    matches!(
        core.as_str(),
        "г" | "стр" | "рис" | "им" | "тов" | "ул" | "д" | "кв" | "см" | "т" | "п"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segmentation::base::{BindingMode, SegmentOptions};

    fn ru() -> CyrillicSlavicSegmenter {
        CyrillicSlavicSegmenter::new(TextLanguage::Ru)
    }

    // --- Russian golden regression -----------------------------------------
    //
    // These are the exact outputs captured from the pre-refactor renderer. The
    // Cyrillic-Slavic refactor must keep them bit-identical (soft hyphens shown
    // as '·' for readability). This is the single most important test.
    #[test]
    fn russian_soft_hyphenation_is_bit_identical() {
        let seg = ru();
        let cases = [
            ("переносить", "пе·ре·но·сить"),
            ("предложение", "пред·ло·же·ние"),
            ("колокольчик", "ко·ло·коль·чик"),
            ("обстоятельство", "об·сто·я·тель·ство"),
            ("информация", "ин·фор·ма·ция"),
            ("программирование", "про·грам·ми·ро·ва·ние"),
            ("достопримечательность", "до·сто·при·ме·ча·тель·ность"),
            ("подъезд", "подъ·езд"),
            ("майка", "май·ка"),
            ("разъяснение", "разъ·яс·не·ние"),
            ("пользователь", "поль·зо·ва·тель"),
            ("необходимость", "необ·хо·ди·мость"),
            ("съешь", "съешь"),
            ("объявление", "объ·яв·ле·ние"),
            ("здравствуйте", "здрав·ствуй·те"),
            ("конституция", "кон·сти·ту·ция"),
            ("preposition", "prepo·si·tion"),
            ("hyphenation", "hyphen·a·tion"),
            ("understanding", "un·der·stand·ing"),
            ("сильнее", "силь·нее"),
            ("армия", "ар·мия"),
            ("удача", "уда·ча"),
            ("взлёт", "взлёт"),
            ("переносится", "пе·ре·но·сит·ся"),
            ("всеобъемлющий", "все·объ·ем·лю·щий"),
        ];
        for (word, expected) in cases {
            let got = seg.soft_hyphenate_overlong(word).replace(SOFT_HYPHEN, "·");
            assert_eq!(got, expected, "soft hyphenation of {word}");
        }
    }

    #[test]
    fn russian_segmentation_is_bit_identical() {
        let seg = ru();
        let text = "не знаю что 5 кг муки через лес ул. Ленина рядом сильнее подъезд";

        let glue = seg.segment(text, SegmentOptions {
            hanging_punctuation: false,
            preserve_edge_spaces: false,
            allow_hard_hyphen_breaks: true,
            binding: BindingMode::Glue,
        });
        let glue_desc: Vec<String> = glue
            .iter()
            .map(|b| format!("[{}|{:?}]", b.text, b.joint.conservatism))
            .collect();
        assert_eq!(
            glue_desc.join(""),
            "[не знаю|Safe][что|Safe][5 кг|Safe][муки|Safe][через лес|Safe]\
             [ул. Ленина|Safe][рядом|Safe][сильнее|Safe][подъезд|Safe]"
        );

        let annotate = seg.segment(text, SegmentOptions {
            hanging_punctuation: false,
            preserve_edge_spaces: false,
            allow_hard_hyphen_breaks: true,
            binding: BindingMode::Annotate,
        });
        let annotate_desc: Vec<String> = annotate
            .iter()
            .map(|b| format!("[{}|{:?}]", b.text, b.joint.conservatism))
            .collect();
        assert_eq!(
            annotate_desc.join(""),
            "[не|Bold][знаю|Safe][что|Safe][5|Reckless][кг|Safe][муки|Safe]\
             [через|Relaxed][лес|Safe][ул.|Relaxed][Ленина|Safe][рядом|Safe]\
             [сильнее|Safe][подъезд|Safe]"
        );
    }

    // --- Boundary rules (ь/ъ/й and syllable) --------------------------------
    #[test]
    fn sanitize_drops_soft_sign_boundary() {
        let word = "пугаешься";
        let soft_sign_idx = word.find('ь').unwrap_or(0);
        let safe_idx = word.find('г').map(|idx| idx + 'г'.len_utf8()).unwrap_or(0);
        assert_eq!(sanitize_breaks(word, vec![safe_idx, soft_sign_idx]), vec![safe_idx]);
    }

    #[test]
    fn dictionary_keeps_break_after_soft_sign() {
        // «силь|нее» — ь on the left, break allowed.
        let word = "сильнее";
        let after_soft_sign = word.find('ь').map(|idx| idx + 'ь'.len_utf8()).unwrap_or(0);
        assert_eq!(sanitize_breaks(word, vec![after_soft_sign]), vec![after_soft_sign]);
    }

    #[test]
    fn safe_boundary_rejects_hard_sign_at_line_start() {
        let word = "подъезд";
        let before_hard_sign = word.find('ъ').unwrap_or(0);
        assert!(!is_safe_hyphen_boundary_at(word, before_hard_sign));
    }

    #[test]
    fn safe_boundary_allows_break_after_short_i() {
        let word = "майка";
        let after_short_i = word.find('й').map(|idx| idx + 'й'.len_utf8()).unwrap_or(0);
        assert!(is_safe_hyphen_boundary_at(word, after_short_i));
        let before_short_i = word.find('й').unwrap_or(0);
        assert!(!is_safe_hyphen_boundary_at(word, before_short_i));
    }

    #[test]
    fn one_letter_and_syllable_rules() {
        // «у|дача» dropped, «уда|ча» kept.
        assert_eq!(sanitize_breaks("удача", vec!["у".len(), "уда".len()]), vec!["уда".len()]);
        // «арми|я» dropped, «ар|мия» kept.
        assert_eq!(sanitize_breaks("армия", vec!["ар".len(), "арми".len()]), vec!["ар".len()]);
        // Vowel-less head «вз» dropped.
        assert!(sanitize_breaks("взлёт", vec!["вз".len()]).is_empty());
    }

    #[test]
    fn monosyllables_acronyms_digits_are_not_hyphenated() {
        let seg = ru();
        assert_eq!(seg.hyphenate_word("стол"), None);
        assert_eq!(seg.hyphenate_word("край"), None);
        assert_eq!(seg.hyphenate_word("СССР"), None);
        assert_eq!(seg.hyphenate_word("HTML"), None);
        assert_eq!(seg.hyphenate_word("covid19"), None);
    }

    #[test]
    fn binding_conservatism_categories() {
        use TextLanguage::Ru;
        assert_eq!(binding_conservatism(Ru, "в", "дом"), Conservatism::Reckless);
        assert_eq!(binding_conservatism(Ru, "5", "кг"), Conservatism::Reckless);
        assert_eq!(binding_conservatism(Ru, "не", "вижу"), Conservatism::Bold);
        assert_eq!(binding_conservatism(Ru, "по", "небу"), Conservatism::Bold);
        assert_eq!(binding_conservatism(Ru, "он", "же"), Conservatism::Bold);
        assert_eq!(binding_conservatism(Ru, "через", "лес"), Conservatism::Relaxed);
        assert_eq!(binding_conservatism(Ru, "ул.", "Ленина"), Conservatism::Relaxed);
        assert_eq!(binding_conservatism(Ru, "кошка", "спит"), Conservatism::Safe);
    }

    /// Pins the ONE intended Russian behavior change from extracting the binding
    /// helpers into `base.rs`: the unit list became a script-agnostic superset, so a
    /// Latin unit after a number now binds inside Russian text. Before the extraction
    /// the Cyrillic-only list left "5 kg" a free break. Cyrillic units are unaffected
    /// (asserted in `binding_conservatism_categories`).
    #[test]
    fn russian_binds_a_latin_unit_after_the_shared_extraction() {
        assert_eq!(
            binding_conservatism(TextLanguage::Ru, "5", "kg"),
            Conservatism::Reckless
        );
    }

    #[test]
    fn binding_is_language_dispatched() {
        use TextLanguage::{Ru, Sr, Uk};

        // Ukrainian consults its own preposition list: "але" (but) is a Ukrainian
        // conjunction absent from the Russian list, so it binds only under Uk.
        assert_eq!(binding_conservatism(Uk, "але", "все"), Conservatism::Relaxed);
        assert_eq!(binding_conservatism(Ru, "але", "все"), Conservatism::Safe);

        // ...and vice versa: the Ukrainian 2-letter conjunction "ні" (spelled with
        // Cyrillic і) binds in Uk but is an unknown word to Russian (whose list has
        // "ни" with и instead).
        assert_eq!(binding_conservatism(Uk, "ні", "я"), Conservatism::Bold);
        assert_eq!(binding_conservatism(Ru, "ні", "я"), Conservatism::Safe);

        // Serbian clitic "се" is a trailing particle in Serbian only.
        assert_eq!(binding_conservatism(Sr, "он", "се"), Conservatism::Bold);
        assert_eq!(binding_conservatism(Ru, "он", "се"), Conservatism::Safe);

        // Serbian preposition "кроз" (through) is Serbian-specific.
        assert_eq!(binding_conservatism(Sr, "кроз", "шуму"), Conservatism::Relaxed);
        assert_eq!(binding_conservatism(Ru, "кроз", "шуму"), Conservatism::Safe);
    }

    #[test]
    fn per_language_dispatch_is_observable() {
        // The dispatch must be real, not a shared superset: at least one input must
        // classify differently across languages. "але" binds in Ukrainian but is a
        // plain word for Russian.
        assert_ne!(
            binding_conservatism(TextLanguage::Uk, "але", "все"),
            binding_conservatism(TextLanguage::Ru, "але", "все"),
        );
    }

    #[test]
    fn emergency_split_helpers_match_legacy_behavior() {
        // A space-separated block is never emergency-split.
        assert!(avoid_emergency_split("да хоть"));
        assert!(!avoid_emergency_split("переносить"));
        // Boundary before ъ is unsafe; a plain interior boundary is safe.
        let word = "подъезд";
        let before_hard = word.find('ъ').unwrap_or(0);
        assert!(!emergency_boundary_is_safe(word, before_hard));
    }
}
