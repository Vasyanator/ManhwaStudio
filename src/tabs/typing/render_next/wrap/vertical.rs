/*
File: src/tabs/typing/render_next/wrap/vertical.rs

Purpose:
Vertical wrap/layout preparation для staged рендера typing.

Main responsibilities:
- собирать vertical columns отдельно от raster-слоя;
- переносить paragraph wrap и split rules для vertical режима;
- переиспользовать hyphenation/shape helper'ы из общего wrap-ядра.

Source:
- `build_vertical_layout_text`
- `build_vertical_layout_columns`
- `wrap_vertical_paragraph`
- связанные split/profile helper'ы из старого `src/tabs/typing/render.rs`
*/

use super::WordBreakPolicy;
use super::hyphenation::{HyphenationDictionaries, find_emergency_split_index};
use super::shape::compute_shape_line_widths;
use crate::tabs::typing::render_next::types::{TextShape, TextWrapMode};
use std::collections::VecDeque;

const VERTICAL_HALF_SPACE: char = '\u{200A}';
const VERTICAL_MIN_CHARS_PER_COLUMN: usize = 5;

#[derive(Debug, Clone, Copy)]
pub(crate) struct VerticalWrapRequest<'a> {
    pub(crate) text: &'a str,
    pub(crate) width_px: f32,
    pub(crate) font_size_px: f32,
    pub(crate) extra_line_spacing_px: f32,
    pub(crate) wrap_mode: TextWrapMode,
    pub(crate) hyphen_dicts: Option<&'a HyphenationDictionaries>,
    pub(crate) word_break_policy: Option<WordBreakPolicy>,
    pub(crate) shape: TextShape,
    pub(crate) min_width_percent: f32,
    pub(crate) allow_moderate_trees: bool,
    pub(crate) preserve_edge_spaces: bool,
}

#[derive(Debug, Clone, Copy)]
struct VerticalParagraphWrapRequest<'a> {
    paragraph: &'a str,
    total_units: f32,
    char_step_px: f32,
    max_column_count: usize,
    hyphen_dicts: Option<&'a HyphenationDictionaries>,
    word_break_policy: Option<WordBreakPolicy>,
    shape: TextShape,
    min_width_percent: f32,
    allow_moderate_trees: bool,
    min_chars_per_column: usize,
    preserve_edge_spaces: bool,
}

#[derive(Debug, Clone, Copy)]
struct VerticalTargetWrapRequest<'a> {
    paragraph: &'a str,
    char_step_px: f32,
    targets: &'a [f32],
    allow_token_split: bool,
    hyphen_dicts: Option<&'a HyphenationDictionaries>,
    word_break_policy: Option<WordBreakPolicy>,
    shape: TextShape,
    allow_moderate_trees: bool,
    preserve_edge_spaces: bool,
}

pub(crate) fn build_vertical_layout_text(request: VerticalWrapRequest<'_>) -> String {
    let columns = build_vertical_layout_columns(request);
    if columns.is_empty() {
        " ".to_string()
    } else {
        columns.join("\n")
    }
}

fn build_vertical_layout_columns(request: VerticalWrapRequest<'_>) -> Vec<String> {
    let cleaned = request.text.replace('\r', "");
    if cleaned.is_empty() {
        return vec![" ".to_string()];
    }

    let estimated_column_step_px =
        (request.font_size_px + request.extra_line_spacing_px).max(request.font_size_px * 0.4);
    let max_columns_by_width = ((request.width_px.max(request.font_size_px)
        + estimated_column_step_px * 0.25)
        / estimated_column_step_px.max(1.0))
    .floor()
    .max(1.0) as usize;
    let mut out = Vec::<String>::new();

    for paragraph in cleaned.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            continue;
        }

        if request.wrap_mode == TextWrapMode::None {
            out.push(paragraph.to_string());
            continue;
        }

        let unit_count = vertical_measure_text_units(paragraph).max(1.0);
        let max_columns_by_min_chars = ((unit_count / VERTICAL_MIN_CHARS_PER_COLUMN as f32)
            .floor()
            .max(1.0)) as usize;
        let max_column_count = max_columns_by_width
            .min(max_columns_by_min_chars.max(1))
            .max(1);
        let mut wrapped = wrap_vertical_paragraph(VerticalParagraphWrapRequest {
            paragraph,
            total_units: unit_count,
            char_step_px: request.font_size_px,
            max_column_count,
            hyphen_dicts: request.hyphen_dicts,
            word_break_policy: request.word_break_policy,
            shape: request.shape,
            min_width_percent: request.min_width_percent,
            allow_moderate_trees: request.allow_moderate_trees,
            min_chars_per_column: VERTICAL_MIN_CHARS_PER_COLUMN,
            preserve_edge_spaces: request.preserve_edge_spaces,
        });
        out.append(&mut wrapped);
    }

    if out.is_empty() {
        out.push(" ".to_string());
    }
    out
}

fn wrap_vertical_paragraph(request: VerticalParagraphWrapRequest<'_>) -> Vec<String> {
    let max_column_count = request.max_column_count.max(1);

    for column_count in (1..=max_column_count).rev() {
        let profile = compute_vertical_column_height_profile(
            column_count,
            request.total_units,
            request.char_step_px,
            request.shape,
            request.min_width_percent,
            request.min_chars_per_column,
        );
        if let Some(columns) = try_wrap_vertical_paragraph_with_targets(VerticalTargetWrapRequest {
            paragraph: request.paragraph,
            char_step_px: request.char_step_px,
            targets: profile.as_slice(),
            allow_token_split: false,
            hyphen_dicts: request.hyphen_dicts,
            word_break_policy: request.word_break_policy,
            shape: request.shape,
            allow_moderate_trees: request.allow_moderate_trees,
            preserve_edge_spaces: request.preserve_edge_spaces,
        }) {
            return columns;
        }
    }

    let fallback_profile = compute_vertical_column_height_profile(
        max_column_count,
        request.total_units,
        request.char_step_px,
        request.shape,
        request.min_width_percent,
        request.min_chars_per_column,
    );
    try_wrap_vertical_paragraph_with_targets(VerticalTargetWrapRequest {
        paragraph: request.paragraph,
        char_step_px: request.char_step_px,
        targets: fallback_profile.as_slice(),
        allow_token_split: true,
        hyphen_dicts: request.hyphen_dicts,
        word_break_policy: request.word_break_policy,
        shape: request.shape,
        allow_moderate_trees: request.allow_moderate_trees,
        preserve_edge_spaces: request.preserve_edge_spaces,
    })
    .unwrap_or_else(|| vec![request.paragraph.trim().to_string()])
}

fn compute_vertical_column_height_profile(
    column_count: usize,
    total_units: f32,
    font_size_px: f32,
    shape: TextShape,
    min_width_percent: f32,
    min_chars_per_column: usize,
) -> Vec<f32> {
    let column_count = column_count.max(1);
    let base_unit_px = font_size_px.max(1.0);
    let unit_profile = match shape {
        TextShape::Free | TextShape::Rectangle | TextShape::SoftPeak => vec![1.0; column_count],
        TextShape::Oval | TextShape::Hexagon => {
            let min_ratio = (min_width_percent / 100.0).clamp(0.01, 1.0);
            compute_shape_line_widths(column_count, 1.0, shape, min_ratio)
        }
    };
    let total_ratio = unit_profile.iter().sum::<f32>().max(1.0);
    let base_units_per_column = (total_units / total_ratio)
        .max(min_chars_per_column as f32)
        .max(1.0);
    unit_profile
        .into_iter()
        .map(|ratio| (ratio * base_units_per_column * base_unit_px).max(base_unit_px))
        .collect()
}

fn try_wrap_vertical_paragraph_with_targets(
    request: VerticalTargetWrapRequest<'_>,
) -> Option<Vec<String>> {
    let mut tokens = tokenize_paragraph(request.paragraph);
    let mut out = Vec::<String>::new();
    let mut current_column = String::new();
    let mut column_idx = 0usize;

    while let Some(token) = tokens.pop_front() {
        let token_is_whitespace = token.chars().all(|ch| ch.is_whitespace());
        if token_is_whitespace && current_column.is_empty() && !request.preserve_edge_spaces {
            continue;
        }

        let max_height_px = request
            .targets
            .get(column_idx)
            .copied()
            .or_else(|| request.targets.last().copied())
            .unwrap_or(request.char_step_px.max(1.0))
            .max(request.char_step_px.max(1.0));
        let available_chars = (max_height_px / request.char_step_px.max(1.0))
            .floor()
            .max(1.0) as usize;

        if token_is_whitespace {
            if !current_column.is_empty()
                && vertical_measure_text_units(current_column.as_str()) < available_chars as f32
            {
                current_column.push(VERTICAL_HALF_SPACE);
            }
            continue;
        }

        if current_column.is_empty() {
            let token_len = token.chars().count();
            if token_len <= available_chars {
                current_column.push_str(token.as_str());
                continue;
            }
            if !request.allow_token_split
                || !should_split_vertical_token(
                    token.as_str(),
                    available_chars,
                    request.word_break_policy,
                    request.shape,
                    request.allow_moderate_trees,
                )
            {
                return None;
            }

            let (head, tail) =
                split_vertical_token(token.as_str(), available_chars, request.hyphen_dicts);
            if !head.is_empty() {
                out.push(head);
                column_idx += 1;
            }
            if !tail.is_empty() {
                tokens.push_front(tail);
            }
            continue;
        }

        let needs_gap = current_column
            .chars()
            .next_back()
            .map(|ch| !ch.is_whitespace())
            .unwrap_or(false);
        let token_units = vertical_measure_text_units(token.as_str());
        let candidate_units = vertical_measure_text_units(current_column.as_str())
            + token_units
            + if needs_gap { 0.5 } else { 0.0 };
        if candidate_units <= available_chars as f32 {
            if needs_gap {
                current_column.push(VERTICAL_HALF_SPACE);
            }
            current_column.push_str(token.as_str());
            continue;
        }

        let flushed = if request.preserve_edge_spaces {
            current_column.clone()
        } else {
            current_column.trim_end().to_string()
        };
        if !flushed.is_empty() {
            out.push(flushed);
            column_idx += 1;
        }
        current_column.clear();
        tokens.push_front(token);
    }

    let tail = if request.preserve_edge_spaces {
        current_column
    } else {
        current_column.trim_end().to_string()
    };
    if !tail.is_empty() || out.is_empty() {
        out.push(tail);
    }
    Some(out)
}

fn should_split_vertical_token(
    token: &str,
    available_chars: usize,
    word_break_policy: Option<WordBreakPolicy>,
    shape: TextShape,
    allow_moderate_trees: bool,
) -> bool {
    let Some(word_break_policy) = word_break_policy else {
        return false;
    };
    if !allow_moderate_trees || shape == TextShape::Free {
        return false;
    }
    let token_chars = token.chars().count();
    if token_chars <= available_chars {
        return false;
    }
    token_chars
        > match word_break_policy {
            WordBreakPolicy::Aggressive => available_chars.saturating_mul(6) / 5,
            WordBreakPolicy::Moderate => available_chars.saturating_mul(4) / 3,
            WordBreakPolicy::Minimal => available_chars.saturating_mul(3) / 2,
        }
}

fn split_vertical_token(
    token: &str,
    count: usize,
    hyphen_dicts: Option<&HyphenationDictionaries>,
) -> (String, String) {
    if let Some(split_at) = hyphen_dicts
        .and_then(|dicts| find_dictionary_split_index_by_units(token, count, dicts))
        .or_else(|| find_emergency_split_index(token, count.max(1), false))
    {
        return (token[..split_at].to_string(), token[split_at..].to_string());
    }
    split_vertical_token_at_char_count(token, count)
}

fn find_dictionary_split_index_by_units(
    token: &str,
    count: usize,
    dicts: &HyphenationDictionaries,
) -> Option<usize> {
    let mut best_split = None;
    for idx in dicts.breaks_for_word(token) {
        let units = token[..idx].chars().count();
        if units > count {
            break;
        }
        best_split = Some(idx);
    }
    best_split
}

fn split_vertical_token_at_char_count(token: &str, count: usize) -> (String, String) {
    if count == 0 {
        return (String::new(), token.to_string());
    }
    let mut split_at = token.len();
    let mut seen = 0usize;
    for (idx, ch) in token.char_indices() {
        seen += 1;
        split_at = idx + ch.len_utf8();
        if seen >= count {
            break;
        }
    }
    if split_at >= token.len() {
        return (token.to_string(), String::new());
    }
    (token[..split_at].to_string(), token[split_at..].to_string())
}

fn vertical_measure_text_units(text: &str) -> f32 {
    text.chars().fold(0.0f32, |acc, ch| match ch {
        '\n' | '\r' => acc,
        VERTICAL_HALF_SPACE => acc + 0.5,
        _ => acc + 1.0,
    })
}

fn tokenize_paragraph(paragraph: &str) -> VecDeque<String> {
    let mut tokens = VecDeque::<String>::new();
    let mut start = 0usize;
    let mut mode_ws: Option<bool> = None;

    for (idx, ch) in paragraph.char_indices() {
        let is_ws = ch.is_whitespace();
        match mode_ws {
            None => mode_ws = Some(is_ws),
            Some(prev) if prev != is_ws => {
                tokens.push_back(paragraph[start..idx].to_string());
                start = idx;
                mode_ws = Some(is_ws);
            }
            Some(_) => {}
        }
    }

    if start < paragraph.len() {
        tokens.push_back(paragraph[start..].to_string());
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::{
        VERTICAL_HALF_SPACE, VerticalParagraphWrapRequest, VerticalWrapRequest,
        build_vertical_layout_columns, wrap_vertical_paragraph,
    };
    use crate::tabs::typing::render_next::types::{TextShape, TextWrapMode};

    #[test]
    fn vertical_wrap_keeps_spaces_between_words() {
        let columns = wrap_vertical_paragraph(VerticalParagraphWrapRequest {
            paragraph: "один два три",
            total_units: 12.0,
            char_step_px: 10.0,
            max_column_count: 3,
            hyphen_dicts: None,
            word_break_policy: None,
            shape: TextShape::Free,
            min_width_percent: 100.0,
            allow_moderate_trees: false,
            min_chars_per_column: 3,
            preserve_edge_spaces: false,
        });

        assert_eq!(columns, vec!["один", "два", "три"]);
    }

    #[test]
    fn vertical_free_wrap_does_not_split_word_when_whole_word_layout_is_possible() {
        let columns = wrap_vertical_paragraph(VerticalParagraphWrapRequest {
            paragraph: "тест слово",
            total_units: 10.0,
            char_step_px: 10.0,
            max_column_count: 2,
            hyphen_dicts: None,
            word_break_policy: None,
            shape: TextShape::Free,
            min_width_percent: 100.0,
            allow_moderate_trees: false,
            min_chars_per_column: 5,
            preserve_edge_spaces: false,
        });

        assert_eq!(columns, vec!["тест", "слово"]);
    }

    #[test]
    fn vertical_no_wrap_keeps_paragraph_in_single_column() {
        let columns = build_vertical_layout_columns(VerticalWrapRequest {
            text: "тест слово",
            width_px: 10.0,
            font_size_px: 10.0,
            extra_line_spacing_px: 0.0,
            wrap_mode: TextWrapMode::None,
            hyphen_dicts: None,
            word_break_policy: None,
            shape: TextShape::Free,
            min_width_percent: 100.0,
            allow_moderate_trees: false,
            preserve_edge_spaces: false,
        });

        assert_eq!(columns, vec!["тест слово"]);
    }

    #[test]
    fn vertical_wrap_preserves_edge_spaces_when_requested() {
        let preserved = wrap_vertical_paragraph(VerticalParagraphWrapRequest {
            paragraph: "  тест  ",
            total_units: 8.0,
            char_step_px: 10.0,
            max_column_count: 1,
            hyphen_dicts: None,
            word_break_policy: None,
            shape: TextShape::Free,
            min_width_percent: 100.0,
            allow_moderate_trees: false,
            min_chars_per_column: 3,
            preserve_edge_spaces: true,
        });
        let trimmed = wrap_vertical_paragraph(VerticalParagraphWrapRequest {
            paragraph: "  тест  ",
            total_units: 8.0,
            char_step_px: 10.0,
            max_column_count: 1,
            hyphen_dicts: None,
            word_break_policy: None,
            shape: TextShape::Free,
            min_width_percent: 100.0,
            allow_moderate_trees: false,
            min_chars_per_column: 3,
            preserve_edge_spaces: false,
        });

        assert_eq!(preserved, vec!["  тест  "]);
        assert_eq!(trimmed, vec!["тест"]);
    }

    #[test]
    fn vertical_wrap_encodes_half_space_for_inter_word_gap() {
        let columns = build_vertical_layout_columns(VerticalWrapRequest {
            text: "один два",
            width_px: 40.0,
            font_size_px: 10.0,
            extra_line_spacing_px: 0.0,
            wrap_mode: TextWrapMode::WholeWords,
            hyphen_dicts: None,
            word_break_policy: None,
            shape: TextShape::Free,
            min_width_percent: 100.0,
            allow_moderate_trees: false,
            preserve_edge_spaces: false,
        });

        assert!(columns.join("\n").contains(VERTICAL_HALF_SPACE));
    }
}
