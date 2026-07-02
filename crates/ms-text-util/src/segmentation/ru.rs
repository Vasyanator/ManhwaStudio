/*
File: src/tabs/typing/segmentation/ru.rs

Purpose:
Русская (и латиница как запасная) реализация сегментатора текста: правила
связывания слов, словарный перенос и безопасные границы переноса.

Словарь (TeX-паттерны через крейт `hyphenation`) уже расставляет переносы по
слогам и знает приставки/удвоенные согласные. Здесь поверх него навешиваются
типографские правила и исключения, которых словарь не гарантирует:

  • Правило одной буквы. Нельзя оставлять в конце строки или переносить на новую
    одну букву: «о-кно», «акаци-я» недопустимы. → у головы перед первым переносом
    и у хвоста после последнего должно быть ≥2 букв.
  • Правило слога. Часть слова без гласной не образует слога: «ст-рах»,
    «отъ-езд» по согласным недопустимы. → и голова, и хвост обязаны содержать
    гласную.
  • ь/ъ/й нельзя оставлять в НАЧАЛЕ новой строки (т.е. справа от точки переноса),
    но переносить ПОСЛЕ них можно — «силь-нее», «подъ-езд», «май-ка».
  • Односложные слова (одна гласная) не переносятся: «стол», «край».
  • Аббревиатуры из заглавных букв («СССР», «HTML») и слова с цифрами не
    переносятся вовсе.

Достаточно проверять только первый и последний словарные переносы: голова и
хвост лишь накапливают буквы/гласные, поэтому если крайние переносы валидны, то
валидны и все промежуточные.
*/

use hyphenation::{Hyphenator, Language, Load, Standard};

use super::base::{Conservatism, SOFT_HYPHEN, Segmenter};

/// Русский сегментатор. Держит словари переноса (рус + англ как fallback).
#[derive(Debug)]
pub struct RussianSegmenter {
    dicts: HyphenationDictionaries,
}

impl RussianSegmenter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            dicts: HyphenationDictionaries::new(),
        }
    }
}

impl Default for RussianSegmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Segmenter for RussianSegmenter {
    fn binding_conservatism(&self, left_token: &str, right_token: &str) -> Conservatism {
        binding_conservatism(left_token, right_token)
    }

    fn hyphenate_word(&self, word: &str) -> Option<String> {
        maybe_soft_hyphenate_word(word, &self.dicts)
    }

    fn hyphen_cost(&self, head_word: &str, tail_word: &str) -> u32 {
        classify_hyphen(head_word, tail_word).cost()
    }
}

// --- Словари переноса -------------------------------------------------------

#[derive(Debug)]
pub struct HyphenationDictionaries {
    russian: Option<Standard>,
    english_us: Option<Standard>,
}

impl Default for HyphenationDictionaries {
    fn default() -> Self {
        Self::new()
    }
}

impl HyphenationDictionaries {
    #[must_use]
    pub fn new() -> Self {
        Self {
            russian: Standard::from_embedded(Language::Russian).ok(),
            english_us: Standard::from_embedded(Language::EnglishUS).ok(),
        }
    }

    /// Безопасные словарные точки переноса слова (рус приоритетнее для кириллицы,
    /// англ — для латиницы; второй словарь как fallback).
    #[must_use]
    pub fn breaks_for_word(&self, word: &str) -> Vec<usize> {
        let has_cyrillic = contains_cyrillic(word);
        let mut out = Vec::<usize>::new();

        if has_cyrillic {
            if let Some(dic) = self.russian.as_ref() {
                out = sanitize_breaks(word, dic.hyphenate(word).breaks);
            }
            if out.is_empty()
                && let Some(dic) = self.english_us.as_ref()
            {
                out = sanitize_breaks(word, dic.hyphenate(word).breaks);
            }
        } else {
            if let Some(dic) = self.english_us.as_ref() {
                out = sanitize_breaks(word, dic.hyphenate(word).breaks);
            }
            if out.is_empty()
                && let Some(dic) = self.russian.as_ref()
            {
                out = sanitize_breaks(word, dic.hyphenate(word).breaks);
            }
        }

        out
    }
}

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
    // Слова с цифрами («covid19», «3д») не переносим — нет надёжных правил.
    if word.chars().any(|ch| ch.is_ascii_digit()) {
        return None;
    }
    // Аббревиатуры из заглавных букв («СССР», «HTML») не переносятся.
    if is_acronym_like(word) {
        return None;
    }
    // Односложные слова (одна гласная) переносить некуда.
    if count_vowels_visible(word) < 2 {
        return None;
    }

    let breaks = dicts.breaks_for_word(word);
    if breaks.is_empty() {
        return None;
    }

    Some(insert_soft_hyphens(word, breaks.as_slice()))
}

/// Слово целиком из заглавных букв (аббревиатура): минимум две буквы и среди
/// буквенных символов нет строчных.
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

// --- Качество переноса ------------------------------------------------------

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

/// Типографско-лингвистическая оценка качества словарного переноса по числу букв
/// слова с каждой стороны разрыва. Эвристика; легко подкрутить.
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

// --- Безопасные границы переноса --------------------------------------------

/// Минимум букв, который можно оставить в конце строки / перенести (правило одной
/// буквы: «одну букву» оставлять нельзя, значит порог — две).
const MIN_EDGE_LETTERS: usize = 2;

pub fn sanitize_breaks(word: &str, mut breaks: Vec<usize>) -> Vec<usize> {
    // Покоординатное правило: справа от переноса не должно стоять ь/ъ/й.
    breaks.retain(|&idx| is_safe_boundary_for_dictionary_at(word, idx));
    breaks.sort_unstable();
    breaks.dedup();

    // Правило одной буквы и правило слога. Достаточно проверить голову перед
    // ПЕРВЫМ переносом и хвост после ПОСЛЕДНЕГО: при движении внутрь слова голова
    // и хвост лишь набирают буквы и гласные, поэтому промежуточные переносы
    // автоматически валидны. Слишком короткие/бесгласные края срезаем.
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

/// Край слова (голова перед первым переносом или хвост после последнего) можно
/// оставлять на строке: в нём ≥2 букв и есть гласная (он образует слог).
fn is_breakable_edge(edge: &str) -> bool {
    count_alpha_chars(edge) >= MIN_EDGE_LETTERS && count_vowels_visible(edge) > 0
}

pub fn is_safe_hyphen_boundary_at(word: &str, idx: usize) -> bool {
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
    // ь/ъ/й нельзя оставлять в начале новой строки; после них рвать можно.
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
    // ь/ъ/й нельзя оставлять в начале новой строки; после них рвать можно.
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

// --- Подсчёт символов / классы кириллицы ------------------------------------

pub fn count_alpha_chars(text: &str) -> usize {
    text.chars()
        .filter(|ch| ch.is_alphabetic() && *ch != SOFT_HYPHEN)
        .count()
}

pub fn count_vowels_visible(text: &str) -> usize {
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

pub fn contains_cyrillic(word: &str) -> bool {
    word.chars().any(|ch| {
        let cp = ch as u32;
        matches!(cp, 0x0400..=0x052F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F)
    })
}

fn contains_latin(word: &str) -> bool {
    word.chars().any(|ch| ch.is_ascii_alphabetic())
}

/// Слова/блоки, которые лучше не рвать аварийным переносом.
pub fn should_avoid_emergency_split(text: &str) -> bool {
    let normalized = text.replace(SOFT_HYPHEN, "");
    if normalized.is_empty() {
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

// --- Правила связывания слов ------------------------------------------------

/// Категория консервативности переноса между двумя токенами. `Safe` — обычный
/// пробел; выше — служебная связь, отрыв которой тем рискованнее, чем выше класс.
/// Множество «связей выше Safe» совпадает с прежним `should_keep_words_together`,
/// поэтому в режиме склейки (`BindingMode::Glue`) поведение врапера не меняется.
fn binding_conservatism(left_token: &str, right_token: &str) -> Conservatism {
    // «Число + единица» («5 кг») — рвать рискованнее всего. Считаем по СЫРЫМ
    // токенам: нормализация ниже выбросила бы цифры, обнулив левый токен.
    if is_numeric_measure_pair(left_token, right_token) {
        return Conservatism::Reckless;
    }

    let left = normalize_binding_token(left_token);
    let right = normalize_binding_token(right_token);
    if left.is_empty() || right.is_empty() {
        return Conservatism::Safe;
    }

    // Однобуквенный предлог/союз («в дом», «к нам») отрывать тоже рискованно.
    let left_is_single = left.chars().count() == 1 && left.chars().all(char::is_alphabetic);
    if left_is_single {
        return Conservatism::Reckless;
    }
    // Предлоги/союзы из словаря: короткие (2 буквы) рвать смелее, длинные — мягче.
    if is_nonbreaking_prefix_word(left.as_str()) {
        return if left.chars().count() <= 2 {
            Conservatism::Bold
        } else {
            Conservatism::Relaxed
        };
    }
    // Частица справа («же», «ли», «бы», «ка») липнет к предыдущему слову.
    if is_nonbreaking_suffix_particle(right.as_str()) {
        return Conservatism::Bold;
    }
    // Сокращение с точкой («стр.», «ул. Ленина»).
    if is_nonbreaking_abbreviation(left_token) {
        return Conservatism::Relaxed;
    }
    Conservatism::Safe
}

fn normalize_binding_token(token: &str) -> String {
    token
        .trim_matches(|ch: char| !ch.is_alphabetic() && ch != SOFT_HYPHEN)
        .to_lowercase()
}

fn is_nonbreaking_prefix_word(word: &str) -> bool {
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

fn is_nonbreaking_suffix_particle(word: &str) -> bool {
    matches!(word, "же" | "ли" | "ль" | "бы" | "б" | "ка" | "де" | "то")
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

fn is_numeric_measure_pair(left_token: &str, right_token: &str) -> bool {
    let left = left_token
        .trim_matches(|ch: char| ch.is_whitespace() || matches!(ch, '(' | '[' | '{' | '"' | '\''));
    let right = right_token
        .trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '№')
        .to_lowercase();
    (is_numeric_token(left) || left == "№") && is_measure_or_unit_token(right.as_str())
}

fn is_numeric_token(token: &str) -> bool {
    let compact = token
        .trim_matches(|ch: char| !ch.is_alphanumeric())
        .replace(',', ".")
        .replace(' ', "");
    !compact.is_empty() && compact.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
}

fn is_measure_or_unit_token(token: &str) -> bool {
    matches!(
        token,
        "кг" | "г"
            | "мг"
            | "л"
            | "мл"
            | "м"
            | "см"
            | "мм"
            | "км"
            | "стр"
            | "с"
            | "мин"
            | "ч"
            | "шт"
            | "гл"
    ) || token.starts_with("стр")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_breaks_drops_soft_sign_boundary() {
        let word = "пугаешься";
        let soft_sign_idx = word.find('ь').unwrap_or(0);
        let safe_idx = word.find('г').map(|idx| idx + 'г'.len_utf8()).unwrap_or(0);

        let breaks = sanitize_breaks(word, vec![safe_idx, soft_sign_idx]);

        assert_eq!(breaks, vec![safe_idx]);
    }

    #[test]
    fn dictionary_keeps_break_after_soft_sign() {
        // «силь|нее» — ь слева, перенос допустим (раньше выбрасывался).
        let word = "сильнее";
        let after_soft_sign = word.find('ь').map(|idx| idx + 'ь'.len_utf8()).unwrap_or(0);
        assert!(is_safe_boundary_for_dictionary_at(word, after_soft_sign));
        let breaks = sanitize_breaks(word, vec![after_soft_sign]);
        assert_eq!(breaks, vec![after_soft_sign]);
    }

    #[test]
    fn safe_hyphen_boundary_rejects_hard_sign_at_line_start() {
        // «под|ъезд» — ъ начинает новую строку, перенос недопустим.
        let word = "подъезд";
        let before_hard_sign = word.find('ъ').unwrap_or(0);
        assert!(!is_safe_hyphen_boundary_at(word, before_hard_sign));
    }

    #[test]
    fn safe_hyphen_boundary_allows_break_after_short_i() {
        // «май|ка» — й остаётся с предыдущей буквой, перенос допустим.
        let word = "майка";
        let after_short_i = word.find('й').map(|idx| idx + 'й'.len_utf8()).unwrap_or(0);
        assert!(is_safe_hyphen_boundary_at(word, after_short_i));
        // «ма|йка» — й начинает строку, недопустимо.
        let before_short_i = word.find('й').unwrap_or(0);
        assert!(!is_safe_hyphen_boundary_at(word, before_short_i));
    }

    #[test]
    fn one_letter_rule_drops_short_head() {
        // «у|дача» — голову из одной буквы оставлять нельзя; «уда|ча» допустимо.
        let word = "удача";
        let after_u = "у".len();
        let after_uda = "уда".len();
        assert_eq!(sanitize_breaks(word, vec![after_u, after_uda]), vec![
            after_uda
        ]);
    }

    #[test]
    fn one_letter_rule_drops_short_tail() {
        // «арми|я» — переносить одну букву нельзя; остаётся «ар|мия».
        let word = "армия";
        let after_ar = "ар".len();
        let before_ya = "арми".len();
        assert_eq!(sanitize_breaks(word, vec![after_ar, before_ya]), vec![
            after_ar
        ]);
    }

    #[test]
    fn syllable_rule_requires_vowel_on_each_side() {
        // Голова без гласной («вз») не образует слога — перенос срезается.
        let word = "взлёт";
        let after_vz = "вз".len();
        assert!(sanitize_breaks(word, vec![after_vz]).is_empty());
    }

    #[test]
    fn monosyllable_is_not_hyphenated() {
        let dicts = HyphenationDictionaries::new();
        assert_eq!(maybe_soft_hyphenate_word("стол", &dicts), None);
        assert_eq!(maybe_soft_hyphenate_word("край", &dicts), None);
    }

    #[test]
    fn acronyms_and_digits_are_not_hyphenated() {
        let dicts = HyphenationDictionaries::new();
        assert_eq!(maybe_soft_hyphenate_word("СССР", &dicts), None);
        assert_eq!(maybe_soft_hyphenate_word("HTML", &dicts), None);
        assert_eq!(maybe_soft_hyphenate_word("covid19", &dicts), None);
    }

    #[test]
    fn multisyllable_word_is_hyphenated() {
        let dicts = HyphenationDictionaries::new();
        let hyphenated = maybe_soft_hyphenate_word("переносить", &dicts);
        assert!(hyphenated.is_some());
        let hyphenated = hyphenated.unwrap();
        assert!(hyphenated.contains(SOFT_HYPHEN));
        // Текст без мягких переносов совпадает с исходным словом.
        assert_eq!(hyphenated.replace(SOFT_HYPHEN, ""), "переносить");
    }

    #[test]
    fn binding_conservatism_categories() {
        // Однобуквенный предлог и «число + единица» — самый рискованный отрыв.
        assert_eq!(binding_conservatism("в", "дом"), Conservatism::Reckless);
        assert_eq!(binding_conservatism("5", "кг"), Conservatism::Reckless);
        // Короткий (2 буквы) предлог/союз и частица справа — «смело».
        assert_eq!(binding_conservatism("не", "вижу"), Conservatism::Bold);
        assert_eq!(binding_conservatism("по", "небу"), Conservatism::Bold);
        assert_eq!(binding_conservatism("он", "же"), Conservatism::Bold);
        // Длинный предлог и сокращение с точкой — мягкий отрыв.
        assert_eq!(binding_conservatism("через", "лес"), Conservatism::Relaxed);
        assert_eq!(binding_conservatism("ул.", "Ленина"), Conservatism::Relaxed);
        // Обычная пара слов рвётся свободно.
        assert_eq!(binding_conservatism("кошка", "спит"), Conservatism::Safe);
    }

    #[test]
    fn binding_above_safe_matches_old_keep_together_set() {
        // Множество «связей выше Safe» = прежнее «держать вместе»: предлог, частица,
        // сокращение, число+единица, однобуквенное слово.
        for (l, r, bound) in [
            ("на", "столе", true),
            ("я", "иду", true),
            ("дом", "стоит", false),
            ("очень", "рад", false),
        ] {
            assert_eq!(
                binding_conservatism(l, r) > Conservatism::Safe,
                bound,
                "{l} + {r}"
            );
        }
    }

    #[test]
    fn hyphen_quality_tiers() {
        assert!(matches!(classify_hyphen("пере", "нос"), HyphenQuality::Good));
        assert!(matches!(
            classify_hyphen("пе", "ренос"),
            HyphenQuality::Medium
        ));
        assert!(matches!(
            classify_hyphen("ст", "ол"),
            HyphenQuality::Unpleasant
        ));
    }
}
