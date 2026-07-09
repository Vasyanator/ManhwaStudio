/*
File: crates/ms-text-util/src/segmentation/latin_common.rs

Purpose:
Shared Latin-script segmentation helpers reused by the Romance, English, and
Latin-Slavic groups. These languages share the same low-level break policy: rely
on the embedded TeX dictionary for syllable points, then enforce only script-
neutral typographic guards (one-letter rule; never break adjacent to a
non-letter such as an apostrophe or opening punctuation). Language-specific
concerns (French `l'homme`, Spanish `¿ ¡`) are covered by the "break only
between two letters" rule below and need no per-language boundary code.

Key functions (all `pub(crate)`, consumed by the group modules and
`dictionaries`/`rules`):
- `sanitize_breaks`         — filter/trim raw dictionary offsets.
- `dictionary_split_is_valid` / `emergency_boundary_is_safe` / `avoid_emergency_split`
- `maybe_soft_hyphenate_word` — dictionary soft hyphenation with acronym/digit guards.
- `hyphen_cost`             — break-quality cost tiers (shared with the panel sort).
*/

use super::base::SOFT_HYPHEN;
use super::dictionaries::HyphenationDictionaries;

/// Minimum letters that may be left at a line end / carried over (one-letter
/// rule). The embedded dictionaries usually already enforce a >= 2 minimum, but
/// this guards fallback dictionaries and keeps the rule explicit.
const MIN_EDGE_LETTERS: usize = 2;

/// Alphabetic character count, excluding soft hyphens.
pub(crate) fn count_alpha_chars(text: &str) -> usize {
    text.chars()
        .filter(|ch| ch.is_alphabetic() && *ch != SOFT_HYPHEN)
        .count()
}

/// Whether byte offset `idx` splits `text` strictly between two alphabetic
/// characters. This is what keeps a break away from an apostrophe (`l'homme`)
/// or opening punctuation (`¿`, `¡`) — the non-letter is never a break side.
fn splits_between_letters(text: &str, idx: usize) -> bool {
    if idx == 0 || idx >= text.len() || !text.is_char_boundary(idx) {
        return false;
    }
    let left = text[..idx].chars().next_back();
    let right = text[idx..].chars().next();
    matches!((left, right), (Some(l), Some(r)) if l.is_alphabetic() && r.is_alphabetic())
}

/// Sanitizes raw dictionary break offsets for a Latin-script word: keep only
/// letter-to-letter boundaries, then trim edges shorter than the one-letter
/// rule. Interior breaks need no further check (heads/tails grow inward).
pub(crate) fn sanitize_breaks(word: &str, mut breaks: Vec<usize>) -> Vec<usize> {
    breaks.retain(|&idx| splits_between_letters(word, idx));
    breaks.sort_unstable();
    breaks.dedup();

    while let Some(&first) = breaks.first() {
        if count_alpha_chars(&word[..first]) >= MIN_EDGE_LETTERS {
            break;
        }
        breaks.remove(0);
    }
    while let Some(&last) = breaks.last() {
        if count_alpha_chars(&word[last..]) >= MIN_EDGE_LETTERS {
            break;
        }
        breaks.pop();
    }

    breaks
}

/// Whether a dictionary break of `word` at byte offset `boundary` keeps >= 2
/// letters on each side. Runtime horizontal wrap re-checks each dictionary break.
pub(crate) fn dictionary_split_is_valid(word: &str, boundary: usize) -> bool {
    count_alpha_chars(&word[..boundary]) >= MIN_EDGE_LETTERS
        && count_alpha_chars(&word[boundary..]) >= MIN_EDGE_LETTERS
}

/// Whether an emergency (non-dictionary) break right before byte `boundary` in
/// `text` is allowed: it must fall between two letters and keep >= 2 letters on
/// each side.
pub(crate) fn emergency_boundary_is_safe(text: &str, boundary: usize) -> bool {
    splits_between_letters(text, boundary)
        && count_alpha_chars(&text[..boundary]) >= MIN_EDGE_LETTERS
        && count_alpha_chars(&text[boundary..]) >= MIN_EDGE_LETTERS
}

/// Blocks/words that must not be split by an emergency hyphen.
pub(crate) fn avoid_emergency_split(text: &str) -> bool {
    let normalized = text.replace(SOFT_HYPHEN, "");
    if normalized.is_empty() {
        return true;
    }
    // A block with whitespace already has a normal wrap point.
    if normalized.chars().any(char::is_whitespace) {
        return true;
    }
    if normalized.contains("://") || normalized.contains('@') {
        return true;
    }
    // Mixed digits+letters (e.g. "covid19") have no reliable break rule.
    if normalized.chars().any(|ch| ch.is_ascii_digit())
        && normalized.chars().any(char::is_alphabetic)
    {
        return true;
    }
    if is_acronym_like(normalized.as_str()) {
        return true;
    }
    normalized.contains('.')
}

/// Inserts dictionary soft hyphens into `word`, or `None` when it should not be
/// hyphenated (too short, URL/e-mail, already hyphenated, has digits, all-caps
/// acronym, or no dictionary breaks).
pub(crate) fn maybe_soft_hyphenate_word(
    word: &str,
    dicts: &HyphenationDictionaries,
) -> Option<String> {
    if word.chars().count() < 4 {
        return None;
    }
    if word.contains("://") || word.contains('@') || word.contains('-') {
        return None;
    }
    if word.contains(SOFT_HYPHEN) {
        return None;
    }
    if word.chars().any(|ch| ch.is_ascii_digit()) {
        return None;
    }
    if is_acronym_like(word) {
        return None;
    }

    let breaks = dicts.breaks_for_word(word);
    if breaks.is_empty() {
        return None;
    }
    Some(insert_soft_hyphens(word, breaks.as_slice()))
}

/// Break-quality cost by the number of letters on each side of the split (same
/// tiers as the Cyrillic path: good/medium/unpleasant -> 2/3/4).
pub(crate) fn hyphen_cost(head_word: &str, tail_word: &str) -> u32 {
    let head = count_alpha_chars(head_word);
    let tail = count_alpha_chars(tail_word);
    let min_side = head.min(tail);
    let total = head + tail;
    if min_side >= 3 {
        2
    } else if min_side >= 2 && total >= 6 {
        3
    } else {
        4
    }
}

/// A word entirely of capital letters (at least two).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_breaks_adjacent_to_apostrophe() {
        // A dictionary that returned a break right at/after the apostrophe would
        // be filtered: the break side must be a letter.
        let word = "l'homme";
        let apostrophe = word.find('\'').unwrap_or(0);
        let after = apostrophe + '\''.len_utf8();
        assert!(!splits_between_letters(word, apostrophe));
        assert!(!splits_between_letters(word, after));
        assert!(!emergency_boundary_is_safe(word, apostrophe));
        assert!(sanitize_breaks(word, vec![apostrophe, after]).is_empty());
    }

    #[test]
    fn one_letter_rule_trims_edges() {
        // "a|bcd" head has 1 letter -> dropped; "ab|cd" kept.
        assert_eq!(sanitize_breaks("abcd", vec![1, 2]), vec![2]);
    }

    #[test]
    fn avoids_emergency_split_for_urls_acronyms_and_dotted() {
        assert!(avoid_emergency_split("http://x"));
        assert!(avoid_emergency_split("HTML"));
        assert!(avoid_emergency_split("e.g"));
        assert!(!avoid_emergency_split("hyphenation"));
    }
}
