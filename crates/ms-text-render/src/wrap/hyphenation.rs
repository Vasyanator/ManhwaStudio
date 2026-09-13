/*
File: src/tabs/typing/render_next/wrap/hyphenation.rs

Purpose:
Runtime-перенос строк горизонтального wrap-ядра: по ширине строки выбирает точку
словарного или аварийного разрыва длинного блока.

Языковые детали (словари, безопасные границы переноса, классы букв) вынесены за
трейт/фасад сегментатора: словарные точки берутся из `HyphenationDictionaries`
(язык выбирается процесс-глобально), а валидность разрыва — через
`ms_text_util::segmentation::rules` (диспетчеризация по группе языка). Здесь —
только выбор позиции разрыва с учётом измеренной ширины строки.

Source:
- `find_dictionary_split_index`
- `find_emergency_split_index`
- `append_wrapped_hyphen`
*/

use super::horizontal::WrapScoringContext;
use ms_text_util::segmentation::HyphenationDictionaries;
use ms_text_util::segmentation::base::SOFT_HYPHEN;
use ms_text_util::segmentation::{count_layout_units, weighted_layout_units};
use ms_text_util::segmentation::rules::{
    avoid_emergency_split, dictionary_split_is_valid, emergency_boundary_is_safe,
};
use ms_text_util::text_punctuation::is_hanging_punctuation;

/// Picks the dictionary hyphenation point of `text` (ONE wrap block) whose head line
/// best fits `target_width_px` without exceeding `max_units` layout units.
///
/// `hanging_punctuation` is the hanging strength `0.0..=1.0` the unit count is taken
/// at (see `count_layout_units`). Returns the byte offset of the split, or `None`
/// when the dictionary offers no valid point.
pub(super) fn find_dictionary_split_index(
    text: &str,
    max_units: usize,
    target_width_px: f32,
    hanging_punctuation: f32,
    dicts: &HyphenationDictionaries,
    scoring: &mut WrapScoringContext<'_, '_>,
) -> Option<usize> {
    let mut best_fit: Option<(usize, f32, f32)> = None;
    let mut best_overflow: Option<(usize, f32, f32)> = None;
    for idx in dicts.breaks_for_word(text) {
        // Language-group rule: enough letters (and, for Cyrillic, a vowel) each side.
        if !dictionary_split_is_valid(text, idx) {
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

/// Finds the last-resort mid-word split of `text` (ONE wrap block) that still fits
/// `max_units`.
///
/// Returns `None` when the block must not be emergency-split at all
/// (`avoid_emergency_split`) or when no safe boundary fits.
///
/// `hanging_punctuation` is the hanging strength `0.0..=1.0`. The two raw tallies are
/// folded by `weighted_layout_units`, the SAME rule `count_layout_units` applies to a
/// whole line, so the incremental walk here and the whole-line count can never
/// disagree about whether a head fits `max_units`.
pub(super) fn find_emergency_split_index(
    text: &str,
    max_units: usize,
    hanging_punctuation: f32,
) -> Option<usize> {
    if avoid_emergency_split(text) {
        return None;
    }
    let mut non_hanging = 0usize;
    let mut hanging = 0usize;
    let mut split_at = None;
    for (idx, ch) in text.char_indices() {
        if ch != SOFT_HYPHEN {
            if is_hanging_punctuation(ch) {
                hanging = hanging.saturating_add(1);
            } else {
                non_hanging = non_hanging.saturating_add(1);
            }
        }
        let next_idx = idx + ch.len_utf8();
        if weighted_layout_units(non_hanging, hanging, hanging_punctuation) > max_units {
            break;
        }
        // Language-group rule decides whether this boundary may carry an emergency break.
        if next_idx < text.len() && emergency_boundary_is_safe(text, next_idx) {
            split_at = Some(next_idx);
        }
    }
    split_at
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

#[cfg(test)]
mod tests {
    use super::{append_wrapped_hyphen, find_emergency_split_index};

    #[test]
    fn emergency_split_skips_space_separated_block() {
        assert!(find_emergency_split_index("да хоть", 2, 0.0).is_none());
        // A whitespace block already has a normal wrap point, in either letter case.
        assert!(find_emergency_split_index("ДА ХОТЬ", 2, 0.0).is_none());
    }

    #[test]
    fn append_wrapped_hyphen_does_not_duplicate_existing_hard_hyphen() {
        assert_eq!(append_wrapped_hyphen("поверит-"), "поверит-");
        assert_eq!(append_wrapped_hyphen("поверит"), "поверит-");
    }
}
