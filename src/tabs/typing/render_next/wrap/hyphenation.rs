/*
File: src/tabs/typing/render_next/wrap/hyphenation.rs

Purpose:
Словарный и аварийный перенос слов для нового horizontal wrap-ядра typing.

Main responsibilities:
- держать hyphenation dictionaries и safe-boundary проверки отдельно от DP-обвязки;
- готовить soft hyphen для длинных слов и предлагать dictionary/emergency split точки;
- делить общие языковые эвристики между free и shape horizontal wrap.

Source:
- `HyphenationDictionaries`
- `soft_hyphenate_overlong`
- `sanitize_breaks`
- `is_safe_*`
- dictionary/emergency split helper-ы из старого `src/tabs/typing/render.rs`
*/

use super::horizontal::{WrapScoringContext, count_layout_units};
use super::{SOFT_HYPHEN, is_hanging_punctuation};
use hyphenation::{Hyphenator, Language, Load, Standard};

#[derive(Debug)]
pub(crate) struct HyphenationDictionaries {
    russian: Option<Standard>,
    english_us: Option<Standard>,
}

impl HyphenationDictionaries {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            russian: Standard::from_embedded(Language::Russian).ok(),
            english_us: Standard::from_embedded(Language::EnglishUS).ok(),
        }
    }

    #[must_use]
    pub(crate) fn breaks_for_word(&self, word: &str) -> Vec<usize> {
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

#[must_use]
pub(crate) fn soft_hyphenate_overlong(text: &str, dicts: &HyphenationDictionaries) -> String {
    let ranges = find_word_ranges(text);
    if ranges.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut tail_start = 0usize;
    for (start, end) in ranges {
        out.push_str(&text[tail_start..start]);
        let word = &text[start..end];
        let replacement =
            maybe_soft_hyphenate_word(word, dicts).unwrap_or_else(|| word.to_string());
        out.push_str(replacement.as_str());
        tail_start = end;
    }
    out.push_str(&text[tail_start..]);
    out
}

pub(super) fn find_dictionary_split_index(
    text: &str,
    max_units: usize,
    target_width_px: f32,
    hanging_punctuation: bool,
    dicts: &HyphenationDictionaries,
    scoring: &mut WrapScoringContext<'_, '_>,
) -> Option<usize> {
    let mut best_fit: Option<(usize, f32, f32)> = None;
    let mut best_overflow: Option<(usize, f32, f32)> = None;
    let text_is_cyrillic = contains_cyrillic(text);
    for idx in dicts.breaks_for_word(text) {
        if count_alpha_chars(&text[..idx]) < 2 || count_alpha_chars(&text[idx..]) < 2 {
            continue;
        }
        if text_is_cyrillic
            && (count_vowels_visible(&text[..idx]) < 1 || count_vowels_visible(&text[idx..]) < 1)
        {
            continue;
        }
        let line_units = count_layout_units(&text[..idx], hanging_punctuation);
        if line_units > max_units {
            break;
        }
        let line_text = append_wrapped_hyphen(&text[..idx]);
        let line_width_px = scoring.measure_line_width_px(line_text.as_str(), line_units);
        let slack_width_px = (target_width_px - line_width_px).max(0.0);
        let overflow_width_px = (line_width_px - target_width_px).max(0.0);
        if overflow_width_px == 0.0 {
            let is_better = match best_fit {
                Some((best_idx, best_slack, _)) => {
                    slack_width_px < best_slack || (slack_width_px == best_slack && idx > best_idx)
                }
                None => true,
            };
            if is_better {
                best_fit = Some((idx, slack_width_px, line_width_px));
            }
            continue;
        }

        let is_better = best_overflow.is_none_or(|(_, best_overflow_px, best_width_px)| {
            overflow_width_px < best_overflow_px
                || (overflow_width_px == best_overflow_px && line_width_px < best_width_px)
        });
        if is_better {
            best_overflow = Some((idx, overflow_width_px, line_width_px));
        }
    }
    best_fit
        .map(|(idx, _, _)| idx)
        .or_else(|| best_overflow.map(|(idx, _, _)| idx))
}

pub(super) fn find_emergency_split_index(
    text: &str,
    max_units: usize,
    hanging_punctuation: bool,
) -> Option<usize> {
    if should_avoid_emergency_split(text) {
        return None;
    }
    let mut units = 0usize;
    let mut split_at = None;
    let text_is_cyrillic = contains_cyrillic(text);
    for (idx, ch) in text.char_indices() {
        if ch != SOFT_HYPHEN && (!hanging_punctuation || !is_hanging_punctuation(ch)) {
            units = units.saturating_add(1);
        }
        let next_idx = idx + ch.len_utf8();
        if units > max_units {
            break;
        }
        if next_idx < text.len()
            && is_safe_hyphen_boundary_at(text, next_idx)
            && count_alpha_chars(&text[..next_idx]) >= 2
            && count_alpha_chars(&text[next_idx..]) >= 2
            && (!text_is_cyrillic
                || (count_vowels_visible(&text[..next_idx]) >= 1
                    && count_vowels_visible(&text[next_idx..]) >= 1))
            && count_vowels_visible(&text[next_idx..]) >= 1
        {
            split_at = Some(next_idx);
        }
    }
    split_at
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

    let breaks = dicts.breaks_for_word(word);
    if breaks.is_empty() {
        return None;
    }

    Some(insert_soft_hyphens(word, breaks.as_slice()))
}

fn find_word_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::<(usize, usize)>::new();
    let mut run_start: Option<usize> = None;
    let mut run_len_chars = 0usize;

    for (idx, ch) in text.char_indices() {
        if is_word_char(ch) {
            if run_start.is_none() {
                run_start = Some(idx);
                run_len_chars = 0;
            }
            run_len_chars += 1;
            continue;
        }

        if let Some(start) = run_start.take()
            && run_len_chars >= 4
        {
            ranges.push((start, idx));
        }
        run_len_chars = 0;
    }

    if let Some(start) = run_start
        && run_len_chars >= 4
    {
        ranges.push((start, text.len()));
    }

    ranges
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn count_alpha_chars(text: &str) -> usize {
    text.chars()
        .filter(|ch| ch.is_alphabetic() && *ch != SOFT_HYPHEN)
        .count()
}

fn count_vowels_visible(text: &str) -> usize {
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

fn contains_cyrillic(word: &str) -> bool {
    word.chars().any(|ch| {
        let cp = ch as u32;
        matches!(cp, 0x0400..=0x052F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F)
    })
}

fn contains_latin(word: &str) -> bool {
    word.chars().any(|ch| ch.is_ascii_alphabetic())
}

pub(super) fn append_wrapped_hyphen(head: &str) -> String {
    if head
        .chars()
        .next_back()
        .is_some_and(is_hard_hyphen_like_char)
    {
        head.to_string()
    } else {
        format!("{head}-")
    }
}

fn is_hard_hyphen_like_char(ch: char) -> bool {
    matches!(
        ch,
        '-' | '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2212}'
    )
}

fn should_avoid_emergency_split(text: &str) -> bool {
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

pub(super) fn sanitize_breaks(word: &str, mut breaks: Vec<usize>) -> Vec<usize> {
    breaks.retain(|&idx| {
        idx > 0
            && idx < word.len()
            && word.is_char_boundary(idx)
            && is_safe_boundary_for_dictionary_at(word, idx)
    });
    breaks.sort_unstable();
    breaks.dedup();
    breaks
}

pub(super) fn is_safe_hyphen_boundary_at(word: &str, idx: usize) -> bool {
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
    if matches!(left, 'ь' | 'Ь' | 'ъ' | 'Ъ' | 'й' | 'Й')
        || matches!(right, 'ь' | 'Ь' | 'ъ' | 'Ъ' | 'й' | 'Й')
    {
        return false;
    }
    if is_cyrillic_consonant(left) && is_cyrillic_vowel(right) {
        return false;
    }
    true
}

fn is_safe_boundary_for_dictionary(left: Option<char>, right: Option<char>) -> bool {
    let (Some(left), Some(right)) = (left, right) else {
        return false;
    };
    if matches!(left, 'ь' | 'Ь' | 'ъ' | 'Ъ') || matches!(right, 'ь' | 'Ь' | 'ъ' | 'Ъ') {
        return false;
    }
    true
}

fn is_safe_boundary_for_dictionary_at(word: &str, idx: usize) -> bool {
    if idx == 0 || idx >= word.len() || !word.is_char_boundary(idx) {
        return false;
    }
    let left = word[..idx].chars().next_back();
    let right = word[idx..].chars().next();
    is_safe_boundary_for_dictionary(left, right)
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

#[cfg(test)]
mod tests {
    use super::{
        append_wrapped_hyphen, find_emergency_split_index, is_safe_hyphen_boundary_at,
        sanitize_breaks,
    };

    #[test]
    fn sanitize_breaks_drops_soft_sign_boundary() {
        let word = "пугаешься";
        let soft_sign_idx = word.find('ь').unwrap_or(0);
        let safe_idx = word.find('г').map(|idx| idx + 'г'.len_utf8()).unwrap_or(0);

        let breaks = sanitize_breaks(word, vec![safe_idx, soft_sign_idx]);

        assert_eq!(breaks, vec![safe_idx]);
    }

    #[test]
    fn safe_hyphen_boundary_rejects_hard_sign_split() {
        let word = "подъезд";
        let hard_sign_idx = word.find('ъ').unwrap_or(0);

        assert!(!is_safe_hyphen_boundary_at(word, hard_sign_idx));
    }

    #[test]
    fn safe_hyphen_boundary_rejects_split_near_short_i() {
        let word = "майка";
        let after_short_i = word.char_indices().nth(2).map(|(idx, _)| idx).unwrap_or(0);

        assert!(!is_safe_hyphen_boundary_at(word, after_short_i));
    }

    #[test]
    fn emergency_split_skips_space_separated_block() {
        assert!(find_emergency_split_index("да хоть", 2, false).is_none());
    }

    #[test]
    fn append_wrapped_hyphen_does_not_duplicate_existing_hard_hyphen() {
        assert_eq!(append_wrapped_hyphen("поверит-"), "поверит-");
        assert_eq!(append_wrapped_hyphen("поверит"), "поверит-");
    }
}
