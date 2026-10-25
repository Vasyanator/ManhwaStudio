/*
File: crates/ms-text-render/src/pipeline/text_prep.rs

Purpose:
Source-text preparation before wrapping and the per-line alignment helpers shared
with the formula / custom-line path.

Key functions:
- prepare_source_text(): ellipsis expansion, uppercase, trimming, sentence newlines
  (in that order)
- compute_inline_line_aligns(), apply_line_aligns_to_buffer(), horizontal_line_offset()
- compute_layout_line_offsets(): byte offset of every layout line
*/

use crate::inline_styles::InlineStyleSpan;
use crate::types::{HorizontalAlign, TextRenderParams};
use cosmic_text::{Align, Buffer};
use super::inline_lookup::inline_style_at_offset;

pub(super) fn prepare_source_text(source_text: &str, params: &TextRenderParams) -> String {
    // Ellipsis expansion runs FIRST, so every later step (sentence detection,
    // wrapping, hanging punctuation) sees the same `...` the reader will see.
    let source_text = if params.replace_ellipsis_with_dots {
        replace_ellipsis_with_dots(source_text)
    } else {
        source_text.to_string()
    };
    let source_text = if params.uppercase_text {
        source_text.to_uppercase()
    } else {
        source_text
    };
    let source_text = if params.trim_extra_spaces {
        trim_extra_spaces(source_text.as_str())
    } else {
        source_text
    };
    let source_text = if params.new_line_after_sentence {
        apply_sentence_newlines(source_text.as_str())
    } else {
        source_text
    };
    if source_text.is_empty() {
        " ".to_string()
    } else {
        source_text
    }
}

/// Expands every `…` (U+2026 HORIZONTAL ELLIPSIS) into three ASCII periods.
///
/// Only U+2026 is expanded; other leader/ellipsis characters (`‥`, `⋯`, `︙`) are
/// left alone because they do not stand for exactly three dots. Returns the text
/// unchanged when it contains no `…`.
pub(super) fn replace_ellipsis_with_dots(text: &str) -> String {
    // Cheap early-out: the common case is text without a single `…`, and
    // `replace` would still allocate a full copy.
    if !text.contains('…') {
        return text.to_string();
    }
    text.replace('…', "...")
}

pub(super) fn trim_extra_spaces(text: &str) -> String {
    fn is_trimmable_space(ch: char) -> bool {
        matches!(ch, ' ' | '\t' | '\r')
    }

    text.trim_matches(is_trimmable_space)
        .split('\n')
        .map(|line| line.trim_matches(is_trimmable_space))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn justify_alignment_option(align: HorizontalAlign) -> Option<Align> {
    if align.justify {
        Some(Align::Justified)
    } else {
        None
    }
}

fn apply_sentence_newlines(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut after_sentence_end = false;
    let mut pending_spaces = String::new();

    for ch in text.chars() {
        if matches!(ch, '.' | '?' | '!') {
            result.push_str(&pending_spaces);
            pending_spaces.clear();
            result.push(ch);
            after_sentence_end = true;
        } else if after_sentence_end {
            if ch.is_alphabetic() {
                pending_spaces.clear();
                result.push('\n');
                result.push(ch);
                after_sentence_end = false;
            } else if ch == '\n' {
                pending_spaces.clear();
                result.push(ch);
                after_sentence_end = false;
            } else if ch == ' ' || ch == '\t' {
                pending_spaces.push(ch);
            } else {
                result.push_str(&pending_spaces);
                pending_spaces.clear();
                result.push(ch);
                after_sentence_end = false;
            }
        } else {
            result.push(ch);
        }
    }
    result.push_str(&pending_spaces);
    result
}

/// Resolve per-line alignment from inline style spans (the span active at each
/// line's first byte), falling back to the block alignment `base_align`. One entry
/// per layout line, at least one.
pub(crate) fn compute_inline_line_aligns(
    base_align: HorizontalAlign,
    layout_line_offsets: &[usize],
    inline_style_spans: Option<&[InlineStyleSpan]>,
) -> Vec<HorizontalAlign> {
    let Some(spans) = inline_style_spans else {
        return vec![base_align; layout_line_offsets.len().max(1)];
    };
    layout_line_offsets
        .iter()
        .map(|offset| {
            inline_style_at_offset(spans, *offset)
                .and_then(|span| span.align)
                .unwrap_or(base_align)
        })
        .collect()
}

pub(super) fn apply_line_aligns_to_buffer(
    buffer: &mut Buffer,
    line_aligns: &[HorizontalAlign],
) {
    for (idx, line) in buffer.lines.iter_mut().enumerate() {
        let cosmic_align = line_aligns
            .get(idx)
            .and_then(|align| justify_alignment_option(*align));
        line.set_align(cosmic_align);
    }
}

pub(crate) fn horizontal_line_offset(
    width_px: u32,
    line_width: f32,
    align: HorizontalAlign,
) -> i32 {
    // Свободное выравнивание растягивает строки до полной ширины, поэтому начинаем
    // от левого края (смещение 0); прочие случаи позиционируются по `bias`.
    if align.justify {
        return 0;
    }
    let free = width_px as f32 - line_width;
    (free * align.offset_fraction()).round() as i32
}

pub(crate) fn compute_layout_line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            offsets.push(idx + ch.len_utf8());
        }
    }
    offsets
}
