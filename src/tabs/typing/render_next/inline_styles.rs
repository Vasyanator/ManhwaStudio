/*
File: src/tabs/typing/render_next/inline_styles.rs

Purpose:
Отдельная подсистема inline-style тегов для staged рендера typing.

Main responsibilities:
- парсить inline-теги и отделять plain text от span-модели;
- ремапить span-диапазоны после нормализации/переформатирования текста;
- применять attrs-совместимую часть inline-стилей к `cosmic-text::Attrs`.

Notes:
- модель span уже хранит и attrs-совместимые, и будущие raster/layout override поля;
- текущий pipeline шага 5 использует здесь только rich-text shaping через `Attrs`;
- color/kerning/stretch/offset/line-spacing пока лишь сохраняются в span-модели для следующих этапов.
*/

use super::font_registry::{InlineFontRegistry, normalize_inline_font_label};
use super::types::{HorizontalAlign, PxOrPercent, parse_machine_tag};
use cosmic_text::{Attrs, AttrsOwned, Family, Metrics, Style, Weight};

const VERTICAL_HALF_SPACE: char = '\u{200A}';
const SOFT_HYPHEN: char = '\u{00AD}';

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InlineStyleSpan {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) align: Option<HorizontalAlign>,
    pub(crate) font_label: Option<String>,
    pub(crate) font_size_px: Option<f32>,
    pub(crate) text_color: Option<[u8; 4]>,
    pub(crate) line_spacing_px: Option<f32>,
    pub(crate) line_spacing_percent: Option<f32>,
    pub(crate) kerning_px: Option<f32>,
    pub(crate) kerning_percent: Option<f32>,
    pub(crate) glyph_stretch_percent: Option<[f32; 2]>,
    pub(crate) glyph_offset: Option<InlineGlyphOffset>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct InlineGlyphOffset {
    pub(crate) global_px: [f32; 2],
    pub(crate) line_px: f32,
    pub(crate) shift_following: bool,
    pub(crate) group_rotation_rad: f32,
    pub(crate) glyph_rotation_rad: f32,
}

impl InlineGlyphOffset {
    #[must_use]
    pub(crate) fn global_only(global_px: [f32; 2]) -> Self {
        Self {
            global_px,
            line_px: 0.0,
            shift_following: false,
            group_rotation_rad: 0.0,
            glyph_rotation_rad: 0.0,
        }
    }
}

impl InlineStyleSpan {
    #[must_use]
    fn empty_range(start: usize, end: usize) -> Self {
        Self {
            start,
            end,
            bold: false,
            italic: false,
            align: None,
            font_label: None,
            font_size_px: None,
            text_color: None,
            line_spacing_px: None,
            line_spacing_percent: None,
            kerning_px: None,
            kerning_percent: None,
            glyph_stretch_percent: None,
            glyph_offset: None,
        }
    }

    #[must_use]
    pub(crate) fn has_attrs_override(&self) -> bool {
        self.bold || self.italic || self.font_label.is_some() || self.font_size_px.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedInlineStyles {
    pub(crate) plain_text: String,
    pub(crate) spans: Vec<InlineStyleSpan>,
}

#[derive(Debug, Default)]
struct InlineStyleState {
    bold_depth: usize,
    italic_depth: usize,
    align_stack: Vec<HorizontalAlign>,
    font_stack: Vec<String>,
    size_stack: Vec<f32>,
    color_stack: Vec<[u8; 4]>,
    line_spacing_stack: Vec<[f32; 2]>,
    kerning_stack: Vec<[f32; 2]>,
    stretch_stack: Vec<[f32; 2]>,
    offset_stack: Vec<InlineGlyphOffset>,
    // Какие стеки толкнул каждый открытый машиночитаемый тег `<m>` — чтобы `</m>`
    // снял ровно их.
    machine_frames: Vec<MachineFramePush>,
}

/// Отметка о том, в какие стеки сложил значения один открывающий тег `<m>`.
#[derive(Debug, Default)]
struct MachineFramePush {
    bold: bool,
    italic: bool,
    align: bool,
    font: bool,
    size: bool,
    color: bool,
    line_spacing: bool,
    kerning: bool,
    stretch: bool,
    offset: bool,
}

impl InlineStyleState {
    #[must_use]
    fn active_span(&self, start: usize, end: usize) -> InlineStyleSpan {
        InlineStyleSpan {
            start,
            end,
            bold: self.bold_depth > 0,
            italic: self.italic_depth > 0,
            align: self.align_stack.last().copied(),
            font_label: self.font_stack.last().cloned(),
            font_size_px: self.size_stack.last().copied(),
            text_color: self.color_stack.last().copied(),
            line_spacing_px: self.line_spacing_stack.last().map(|value| value[0]),
            line_spacing_percent: self.line_spacing_stack.last().map(|value| value[1]),
            kerning_px: self.kerning_stack.last().map(|value| value[0]),
            kerning_percent: self.kerning_stack.last().map(|value| value[1]),
            glyph_stretch_percent: self.stretch_stack.last().copied(),
            glyph_offset: self.offset_stack.last().copied(),
        }
    }
}

pub(crate) fn parse_inline_style_tags(text: &str, base_font_size_px: f32) -> ParsedInlineStyles {
    let mut plain_text = String::with_capacity(text.len());
    let mut spans = Vec::<InlineStyleSpan>::new();
    let mut state = InlineStyleState::default();
    let mut span_start = 0usize;
    let mut i = 0usize;

    while i < text.len() {
        let rest = &text[i..];
        let Some(ch) = rest.chars().next() else {
            break;
        };

        if ch == '<'
            && let Some(rel_end) = text[i + 1..].find('>')
        {
            let end = i + 1 + rel_end;
            let raw = text[i + 1..end].trim();
            let compact = raw
                .chars()
                .filter(|character| !character.is_ascii_whitespace())
                .collect::<String>()
                .to_ascii_lowercase();

            let handled_tag = match compact.as_str() {
                "b" | "strong" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.bold_depth = state.bold_depth.saturating_add(1);
                    true
                }
                "/b" | "/strong" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.bold_depth = state.bold_depth.saturating_sub(1);
                    true
                }
                "i" | "em" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.italic_depth = state.italic_depth.saturating_add(1);
                    true
                }
                "/i" | "/em" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.italic_depth = state.italic_depth.saturating_sub(1);
                    true
                }
                "no-break" | "nobreak" | "nobr" | "/no-break" | "/nobreak" | "/nobr" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    true
                }
                "/align" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.align_stack.pop();
                    true
                }
                "/font" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.font_stack.pop();
                    true
                }
                "/size" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.size_stack.pop();
                    true
                }
                "/color" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.color_stack.pop();
                    true
                }
                "/line-spacing" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.line_spacing_stack.pop();
                    true
                }
                "/kerning" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.kerning_stack.pop();
                    true
                }
                "/stretching" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.stretch_stack.pop();
                    true
                }
                "/offset" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.offset_stack.pop();
                    true
                }
                "/m" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    close_machine_tag(&mut state);
                    true
                }
                "br" | "br/" | "/br" => {
                    plain_text.push('\n');
                    true
                }
                _ => false,
            };
            if handled_tag {
                i = end + 1;
                continue;
            }

            if let Some(align) = parse_align_tag_value(raw) {
                flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                state.align_stack.push(align);
                i = end + 1;
                continue;
            }
            if let Some(font_label) = parse_font_tag_label(raw) {
                flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                state.font_stack.push(font_label);
                i = end + 1;
                continue;
            }
            if let Some(font_size_px) = parse_size_tag_value(raw) {
                flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                state.size_stack.push(font_size_px);
                i = end + 1;
                continue;
            }
            if let Some(text_color) = parse_color_tag_value(raw) {
                flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                state.color_stack.push(text_color);
                i = end + 1;
                continue;
            }
            if let Some(line_spacing) = parse_line_spacing_tag_value(raw) {
                flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                state.line_spacing_stack.push(line_spacing);
                i = end + 1;
                continue;
            }
            if let Some(kerning) = parse_kerning_tag_value(raw) {
                flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                state.kerning_stack.push(kerning);
                i = end + 1;
                continue;
            }
            if let Some(stretching) = parse_stretching_tag_value(raw, base_font_size_px) {
                flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                state.stretch_stack.push(stretching);
                i = end + 1;
                continue;
            }
            if let Some(glyph_offset) = parse_offset_tag_value(raw, base_font_size_px) {
                flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                state.offset_stack.push(glyph_offset);
                i = end + 1;
                continue;
            }
            if let Some(attrs) = parse_machine_tag(raw) {
                flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                apply_machine_tag(&mut state, &attrs, base_font_size_px);
                i = end + 1;
                continue;
            }
        }

        plain_text.push(ch);
        i += ch.len_utf8();
    }

    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
    ParsedInlineStyles { plain_text, spans }
}

pub(crate) fn remap_inline_style_spans(
    source_text: &str,
    target_text: &str,
    source_spans: &[InlineStyleSpan],
) -> Option<Vec<InlineStyleSpan>> {
    if source_spans.is_empty() {
        return Some(Vec::new());
    }
    if source_text == target_text {
        return Some(source_spans.to_vec());
    }

    let source_chars = source_text
        .char_indices()
        .map(|(start, ch)| (start, start + ch.len_utf8(), ch))
        .collect::<Vec<_>>();
    let target_chars = target_text
        .char_indices()
        .map(|(start, ch)| (start, start + ch.len_utf8(), ch))
        .collect::<Vec<_>>();

    let mut source_char_idx = 0usize;
    let mut mapped = Vec::<InlineStyleSpan>::new();

    for (target_start, target_end, target_ch) in target_chars {
        let consumed_soft_hyphen = consume_soft_hyphen_for_wrapped_hyphen(
            target_ch,
            &source_chars,
            &mut source_char_idx,
            source_spans,
        );
        if let Some(style) = consumed_soft_hyphen {
            push_or_extend_inline_style_span(&mut mapped, target_start, target_end, &style);
            continue;
        }
        skip_unrendered_soft_hyphens(&source_chars, &mut source_char_idx);
        let source_char = source_chars.get(source_char_idx).copied();
        let (style, consumed_source_char) = match source_char {
            Some((source_start, _source_end, source_ch)) if source_ch == target_ch => {
                (inline_style_at(source_start, source_spans), true)
            }
            Some((source_start, _source_end, source_ch))
                if matches!(target_ch, '\n' | VERTICAL_HALF_SPACE) && source_ch.is_whitespace() =>
            {
                (inline_style_at(source_start, source_spans), true)
            }
            _ => (
                inline_style_context_at(source_char_idx, &source_chars, source_spans),
                false,
            ),
        };

        push_or_extend_inline_style_span(&mut mapped, target_start, target_end, &style);
        if consumed_source_char {
            source_char_idx += 1;
        }
    }

    while let Some((_, _, ch)) = source_chars.get(source_char_idx).copied() {
        if ch.is_whitespace() || ch == SOFT_HYPHEN {
            source_char_idx += 1;
            continue;
        }
        return None;
    }

    Some(mapped)
}

fn consume_soft_hyphen_for_wrapped_hyphen(
    target_ch: char,
    source_chars: &[(usize, usize, char)],
    source_char_idx: &mut usize,
    source_spans: &[InlineStyleSpan],
) -> Option<InlineStyleSpan> {
    let (source_start, _, source_ch) = source_chars.get(*source_char_idx).copied()?;
    if source_ch == SOFT_HYPHEN && target_ch == '-' {
        *source_char_idx = (*source_char_idx).saturating_add(1);
        return Some(inline_style_at(source_start, source_spans));
    }
    None
}

fn skip_unrendered_soft_hyphens(
    source_chars: &[(usize, usize, char)],
    source_char_idx: &mut usize,
) {
    while source_chars
        .get(*source_char_idx)
        .is_some_and(|(_, _, ch)| *ch == SOFT_HYPHEN)
    {
        *source_char_idx = (*source_char_idx).saturating_add(1);
    }
}

#[must_use]
pub(crate) fn collect_requested_inline_font_labels(spans: &[InlineStyleSpan]) -> Vec<String> {
    let mut labels = spans
        .iter()
        .filter_map(|span| span.font_label.as_ref())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    labels.sort();
    labels.dedup();
    labels
}

#[must_use]
pub(crate) fn spans_have_attrs_overrides(spans: &[InlineStyleSpan]) -> bool {
    spans.iter().any(InlineStyleSpan::has_attrs_override)
}

pub(crate) fn apply_inline_style_to_attrs<'a>(
    attrs: &Attrs<'a>,
    style: &InlineStyleSpan,
    inline_font_registry: &InlineFontRegistry,
) -> AttrsOwned {
    let mut styled_attrs = AttrsOwned::new(attrs);
    if style.bold {
        styled_attrs.weight = Weight::BOLD;
    }
    if style.italic {
        styled_attrs.style = Style::Italic;
    }
    if let Some(font_label) = style.font_label.as_deref()
        && let Some(font_attrs) = inline_font_registry.get(&normalize_inline_font_label(font_label))
    {
        if let Some(family_name) = font_attrs.family_name.as_deref() {
            styled_attrs.family_owned = cosmic_text::FamilyOwned::new(Family::Name(family_name));
        }
        if let Some(style) = font_attrs.style {
            styled_attrs.style = style;
        }
        if let Some(weight) = font_attrs.weight {
            styled_attrs.weight = weight;
        }
        if let Some(stretch) = font_attrs.stretch {
            styled_attrs.stretch = stretch;
        }
    }
    if let Some(font_size_px) = style.font_size_px {
        let base_metrics = attrs
            .metrics_opt
            .map(Into::<Metrics>::into)
            .unwrap_or(Metrics::new(1.0, 1.0));
        let size_scale = if base_metrics.font_size > 0.0 {
            font_size_px / base_metrics.font_size
        } else {
            1.0
        };
        let line_height = (base_metrics.line_height * size_scale).max(font_size_px);
        styled_attrs.metrics_opt = Some(Metrics::new(font_size_px, line_height).into());
    }
    styled_attrs
}

fn flush_active_span(
    plain_text: &str,
    spans: &mut Vec<InlineStyleSpan>,
    span_start: &mut usize,
    state: &InlineStyleState,
) {
    let end = plain_text.len();
    if end <= *span_start {
        return;
    }
    spans.push(state.active_span(*span_start, end));
    *span_start = end;
}

fn inline_style_at(offset: usize, spans: &[InlineStyleSpan]) -> InlineStyleSpan {
    spans
        .iter()
        .find(|span| span.start <= offset && offset < span.end)
        .cloned()
        .unwrap_or_else(|| InlineStyleSpan::empty_range(offset, offset))
}

fn inline_style_context_at(
    source_char_idx: usize,
    source_chars: &[(usize, usize, char)],
    spans: &[InlineStyleSpan],
) -> InlineStyleSpan {
    if let Some((source_start, _, _)) = source_chars.get(source_char_idx).copied() {
        return inline_style_at(source_start, spans);
    }
    if let Some((source_start, _, source_ch)) = source_chars.last().copied() {
        let probe_offset = source_start + source_ch.len_utf8().saturating_sub(1);
        return inline_style_at(probe_offset, spans);
    }
    InlineStyleSpan::empty_range(0, 0)
}

fn push_or_extend_inline_style_span(
    spans: &mut Vec<InlineStyleSpan>,
    start: usize,
    end: usize,
    style: &InlineStyleSpan,
) {
    if end <= start {
        return;
    }

    if let Some(last) = spans.last_mut()
        && last.end == start
        && last.bold == style.bold
        && last.italic == style.italic
        && last.align == style.align
        && last.font_label == style.font_label
        && last.font_size_px == style.font_size_px
        && last.text_color == style.text_color
        && last.line_spacing_px == style.line_spacing_px
        && last.line_spacing_percent == style.line_spacing_percent
        && last.kerning_px == style.kerning_px
        && last.kerning_percent == style.kerning_percent
        && last.glyph_stretch_percent == style.glyph_stretch_percent
        && last.glyph_offset == style.glyph_offset
    {
        last.end = end;
        return;
    }

    let mut cloned = style.clone();
    cloned.start = start;
    cloned.end = end;
    spans.push(cloned);
}

fn parse_align_tag_value(raw_tag: &str) -> Option<HorizontalAlign> {
    let value = tag_value(raw_tag, "align")?;
    parse_inline_align_value(value)
}

fn parse_inline_align_value(value: &str) -> Option<HorizontalAlign> {
    let trimmed = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
        .trim();
    if trimmed.is_empty() {
        return None;
    }
    let bias = trimmed.parse::<f32>().ok();
    Some(HorizontalAlign::from_config(Some(trimmed), bias))
}

fn parse_font_tag_label(raw_tag: &str) -> Option<String> {
    let trimmed = raw_tag.trim();
    let (tag_name, value) = trimmed.split_once('=')?;
    if !tag_name.trim().eq_ignore_ascii_case("font") {
        return None;
    }

    let label = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
        .trim();
    if label.is_empty() {
        None
    } else {
        Some(label.to_string())
    }
}

fn parse_size_tag_value(raw_tag: &str) -> Option<f32> {
    let trimmed = raw_tag.trim();
    let (tag_name, value) = trimmed.split_once('=')?;
    if !tag_name.trim().eq_ignore_ascii_case("size") {
        return None;
    }

    let trimmed_value = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
        .trim();
    let numeric_value = trimmed_value
        .strip_suffix("px")
        .unwrap_or(trimmed_value)
        .trim();
    let parsed = numeric_value.parse::<f32>().ok()?;
    if parsed.is_finite() && parsed > 0.0 {
        Some(parsed)
    } else {
        None
    }
}

fn parse_color_tag_value(raw_tag: &str) -> Option<[u8; 4]> {
    let trimmed = raw_tag.trim();
    let (tag_name, value) = trimmed.split_once('=')?;
    if !tag_name.trim().eq_ignore_ascii_case("color") {
        return None;
    }
    parse_hex_color_rgba(value)
}

fn parse_offset_tag_value(raw_tag: &str, base_font_size_px: f32) -> Option<InlineGlyphOffset> {
    let trimmed = raw_tag.trim();
    let (tag_name, value) = trimmed.split_once('=')?;
    if !tag_name.trim().eq_ignore_ascii_case("offset") {
        return None;
    }

    let value = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
        .trim();
    let parts = value.split(',').map(str::trim).collect::<Vec<_>>();
    // X/Y/«по линии» поддерживают суффикс `%` (проценты от кегля); без него — пиксели.
    let x = PxOrPercent::parse(parts.first()?)?.as_px_of(base_font_size_px);
    let y = PxOrPercent::parse(parts.get(1)?)?.as_px_of(base_font_size_px);
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let line_px = parts
        .get(2)
        .and_then(|value| PxOrPercent::parse(value))
        .map(|value| value.as_px_of(base_font_size_px))
        .filter(|value| value.is_finite())
        .unwrap_or(0.0)
        .clamp(-1000.0, 1000.0);
    let shift_following = parts
        .get(3)
        .is_some_and(|value| parse_inline_bool(value).unwrap_or(false));
    let group_rotation_rad = parts
        .get(4)
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(0.0)
        .clamp(-360.0, 360.0)
        .to_radians();
    let glyph_rotation_rad = parts
        .get(5)
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(0.0)
        .clamp(-360.0, 360.0)
        .to_radians();
    Some(InlineGlyphOffset {
        global_px: [x.clamp(-100.0, 100.0), y.clamp(-100.0, 100.0)],
        line_px,
        shift_following,
        group_rotation_rad,
        glyph_rotation_rad,
    })
}

fn parse_inline_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Применить машиночитаемый тег `<m ...>`: сложить все заданные стили в их стеки и
/// запомнить кадр, чтобы `</m>` снял ровно их. См. контракт ключей в `parse_machine_tag`.
fn apply_machine_tag(
    state: &mut InlineStyleState,
    attrs: &[(char, String)],
    base_font_size_px: f32,
) {
    let mut frame = MachineFramePush::default();
    let mut stretch_w: Option<f32> = None;
    let mut stretch_h: Option<f32> = None;
    let mut offset = InlineGlyphOffset::global_only([0.0, 0.0]);
    let mut has_offset = false;

    for (key, value) in attrs {
        match key {
            'b' => {
                state.bold_depth = state.bold_depth.saturating_add(1);
                frame.bold = true;
            }
            'i' => {
                state.italic_depth = state.italic_depth.saturating_add(1);
                frame.italic = true;
            }
            'a' => {
                if let Some(align) = parse_inline_align_value(value) {
                    state.align_stack.push(align);
                    frame.align = true;
                }
            }
            'f' => {
                let label = value.trim();
                if !label.is_empty() {
                    state.font_stack.push(label.to_string());
                    frame.font = true;
                }
            }
            's' => {
                if let Ok(px) = value.trim().parse::<f32>()
                    && px.is_finite()
                    && px > 0.0
                {
                    state.size_stack.push(px);
                    frame.size = true;
                }
            }
            'c' => {
                if let Some(color) = parse_hex_color_rgba(value) {
                    state.color_stack.push(color);
                    frame.color = true;
                }
            }
            'l' => {
                if let Some(pair) = machine_pair_value(value) {
                    state.line_spacing_stack.push(pair);
                    frame.line_spacing = true;
                }
            }
            'k' => {
                if let Some(pair) = machine_pair_value(value) {
                    state.kerning_stack.push(pair);
                    frame.kerning = true;
                }
            }
            'w' => {
                stretch_w = PxOrPercent::parse(value)
                    .map(|parsed| parsed.as_percent_of(base_font_size_px).clamp(1.0, 300.0));
            }
            'h' => {
                stretch_h = PxOrPercent::parse(value)
                    .map(|parsed| parsed.as_percent_of(base_font_size_px).clamp(1.0, 300.0));
            }
            'x' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    offset.global_px[0] = parsed.as_px_of(base_font_size_px).clamp(-100.0, 100.0);
                    has_offset = true;
                }
            }
            'y' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    offset.global_px[1] = parsed.as_px_of(base_font_size_px).clamp(-100.0, 100.0);
                    has_offset = true;
                }
            }
            'n' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    offset.line_px = parsed.as_px_of(base_font_size_px).clamp(-1000.0, 1000.0);
                    has_offset = true;
                }
            }
            'g' => {
                if let Ok(deg) = value.trim().parse::<f32>()
                    && deg.is_finite()
                {
                    offset.group_rotation_rad = deg.clamp(-360.0, 360.0).to_radians();
                    has_offset = true;
                }
            }
            'r' => {
                if let Ok(deg) = value.trim().parse::<f32>()
                    && deg.is_finite()
                {
                    offset.glyph_rotation_rad = deg.clamp(-360.0, 360.0).to_radians();
                    has_offset = true;
                }
            }
            'q' => {
                offset.shift_following = true;
                has_offset = true;
            }
            _ => {}
        }
    }

    if stretch_w.is_some() || stretch_h.is_some() {
        state
            .stretch_stack
            .push([stretch_w.unwrap_or(100.0), stretch_h.unwrap_or(100.0)]);
        frame.stretch = true;
    }
    if has_offset {
        state.offset_stack.push(offset);
        frame.offset = true;
    }

    state.machine_frames.push(frame);
}

/// Снять стили, сложенные парным открывающим `<m ...>`.
fn close_machine_tag(state: &mut InlineStyleState) {
    let Some(frame) = state.machine_frames.pop() else {
        return;
    };
    if frame.bold {
        state.bold_depth = state.bold_depth.saturating_sub(1);
    }
    if frame.italic {
        state.italic_depth = state.italic_depth.saturating_sub(1);
    }
    if frame.align {
        state.align_stack.pop();
    }
    if frame.font {
        state.font_stack.pop();
    }
    if frame.size {
        state.size_stack.pop();
    }
    if frame.color {
        state.color_stack.pop();
    }
    if frame.line_spacing {
        state.line_spacing_stack.pop();
    }
    if frame.kerning {
        state.kerning_stack.pop();
    }
    if frame.stretch {
        state.stretch_stack.pop();
    }
    if frame.offset {
        state.offset_stack.pop();
    }
}

/// Значение `px-или-%` в пару `[px, percent]` (активна ровно одна компонента),
/// с клампом до ±300 — как у line-spacing/kerning.
fn machine_pair_value(value: &str) -> Option<[f32; 2]> {
    let parsed = PxOrPercent::parse(value)?;
    let (px, percent) = PxOrPercent {
        value: parsed.value.clamp(-300.0, 300.0),
        is_percent: parsed.is_percent,
    }
    .as_px_percent();
    Some([px, percent])
}

/// Извлечь значение тега `name=...`, обрезав кавычки/пробелы.
fn tag_value<'a>(raw_tag: &'a str, tag_name: &str) -> Option<&'a str> {
    let trimmed = raw_tag.trim();
    let (raw_name, value) = trimmed.split_once('=')?;
    if !raw_name.trim().eq_ignore_ascii_case(tag_name) {
        return None;
    }
    Some(
        value
            .trim()
            .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
            .trim(),
    )
}

/// Разобрать одиночное значение `px-или-%` (или устаревшую пару `px,percent`)
/// в пару `[px, percent]`, где активна ровно одна компонента (для нового формата).
fn parse_value_or_legacy_pair(raw_tag: &str, tag_name: &str, clamp_abs: f32) -> Option<[f32; 2]> {
    let value = tag_value(raw_tag, tag_name)?;
    if let Some((x_raw, y_raw)) = value.split_once(',') {
        // Устаревший формат: отдельные пиксели и проценты, складывались в рендере.
        let px = x_raw.trim().parse::<f32>().ok()?;
        let percent = y_raw.trim().parse::<f32>().ok()?;
        if !px.is_finite() || !percent.is_finite() {
            return None;
        }
        return Some([px.clamp(-clamp_abs, clamp_abs), percent.clamp(-clamp_abs, clamp_abs)]);
    }
    let parsed = PxOrPercent::parse(value)?;
    let (px, percent) = PxOrPercent {
        value: parsed.value.clamp(-clamp_abs, clamp_abs),
        is_percent: parsed.is_percent,
    }
    .as_px_percent();
    Some([px, percent])
}

fn parse_line_spacing_tag_value(raw_tag: &str) -> Option<[f32; 2]> {
    parse_value_or_legacy_pair(raw_tag, "line-spacing", 300.0)
}

fn parse_kerning_tag_value(raw_tag: &str) -> Option<[f32; 2]> {
    parse_value_or_legacy_pair(raw_tag, "kerning", 300.0)
}

/// Разобрать `stretching=ширина,высота`. Каждая компонента может иметь суффикс `%`
/// (проценты от кегля) либо быть в пикселях; результат — множители в процентах.
fn parse_stretching_tag_value(raw_tag: &str, base_font_size_px: f32) -> Option<[f32; 2]> {
    let value = tag_value(raw_tag, "stretching")?;
    let (x_raw, y_raw) = value.split_once(',')?;
    let width = PxOrPercent::parse(x_raw)?.as_percent_of(base_font_size_px);
    let height = PxOrPercent::parse(y_raw)?.as_percent_of(base_font_size_px);
    if !width.is_finite() || !height.is_finite() {
        return None;
    }
    Some([width.clamp(1.0, 300.0), height.clamp(1.0, 300.0)])
}

fn parse_hex_color_rgba(value: &str) -> Option<[u8; 4]> {
    let trimmed_value = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
        .trim();
    let hex = trimmed_value
        .strip_prefix('#')
        .unwrap_or(trimmed_value)
        .trim();
    match hex.len() {
        6 => {
            let rgb = u32::from_str_radix(hex, 16).ok()?;
            Some([
                u8::try_from((rgb >> 16) & 0xFF).ok()?,
                u8::try_from((rgb >> 8) & 0xFF).ok()?,
                u8::try_from(rgb & 0xFF).ok()?,
                255,
            ])
        }
        8 => {
            let rgba = u32::from_str_radix(hex, 16).ok()?;
            Some([
                u8::try_from((rgba >> 24) & 0xFF).ok()?,
                u8::try_from((rgba >> 16) & 0xFF).ok()?,
                u8::try_from((rgba >> 8) & 0xFF).ok()?,
                u8::try_from(rgba & 0xFF).ok()?,
            ])
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InlineGlyphOffset, InlineStyleSpan, apply_inline_style_to_attrs, parse_inline_style_tags,
        remap_inline_style_spans,
    };
    use crate::tabs::typing::render_next::font_registry::InlineFontRegistry;
    use crate::tabs::typing::render_next::types::HorizontalAlign;
    use cosmic_text::{Attrs, Metrics, Style, Weight};

    #[test]
    fn remap_inline_style_spans_keeps_style_across_inserted_newline_inside_span() {
        let source = "abc";
        let target = "a\nbc";
        let spans = vec![InlineStyleSpan {
            start: 0,
            end: source.len(),
            bold: true,
            italic: false,
            align: None,
            font_label: None,
            font_size_px: None,
            text_color: None,
            line_spacing_px: None,
            line_spacing_percent: None,
            kerning_px: None,
            kerning_percent: None,
            glyph_stretch_percent: None,
            glyph_offset: None,
        }];

        let mapped = remap_inline_style_spans(source, target, spans.as_slice()).expect("mapped");

        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].start, 0);
        assert_eq!(mapped[0].end, target.len());
        assert!(mapped[0].bold);
        assert!(!mapped[0].italic);
    }

    #[test]
    fn remap_inline_style_spans_treats_wrap_newline_as_replaced_whitespace() {
        let source = "ab cd";
        let target = "ab\ncd";
        let spans = vec![
            InlineStyleSpan {
                start: 0,
                end: 2,
                bold: false,
                italic: false,
                align: None,
                font_label: None,
                font_size_px: None,
                text_color: None,
                line_spacing_px: None,
                line_spacing_percent: None,
                kerning_px: None,
                kerning_percent: None,
                glyph_stretch_percent: None,
                glyph_offset: None,
            },
            InlineStyleSpan {
                start: 2,
                end: source.len(),
                bold: true,
                italic: false,
                align: None,
                font_label: None,
                font_size_px: None,
                text_color: None,
                line_spacing_px: None,
                line_spacing_percent: None,
                kerning_px: None,
                kerning_percent: None,
                glyph_stretch_percent: None,
                glyph_offset: None,
            },
        ];

        let mapped = remap_inline_style_spans(source, target, spans.as_slice()).expect("mapped");

        assert_eq!(mapped.len(), 2);
        assert_eq!(target.get(mapped[0].start..mapped[0].end), Some("ab"));
        assert!(!mapped[0].bold);
        assert_eq!(target.get(mapped[1].start..mapped[1].end), Some("\ncd"));
        assert!(mapped[1].bold);
    }

    #[test]
    fn remap_inline_style_spans_consumes_soft_hyphen_as_wrapped_hyphen() {
        let source = "super\u{00AD}califragilistic";
        let target = "super-\ncalifragilistic";
        let spans = vec![InlineStyleSpan {
            start: 0,
            end: source.len(),
            bold: true,
            italic: false,
            align: None,
            font_label: None,
            font_size_px: None,
            text_color: None,
            line_spacing_px: None,
            line_spacing_percent: None,
            kerning_px: None,
            kerning_percent: None,
            glyph_stretch_percent: None,
            glyph_offset: None,
        }];

        let mapped = remap_inline_style_spans(source, target, spans.as_slice()).expect("mapped");

        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].start, 0);
        assert_eq!(mapped[0].end, target.len());
        assert!(mapped[0].bold);
    }

    #[test]
    fn remap_inline_style_spans_keeps_style_across_inserted_emergency_hyphen() {
        let source = "supercalifragilistic";
        let target = "super-\ncalifragilistic";
        let spans = vec![InlineStyleSpan {
            start: 0,
            end: source.len(),
            bold: true,
            italic: false,
            align: None,
            font_label: None,
            font_size_px: None,
            text_color: None,
            line_spacing_px: None,
            line_spacing_percent: None,
            kerning_px: None,
            kerning_percent: None,
            glyph_stretch_percent: None,
            glyph_offset: None,
        }];

        let mapped = remap_inline_style_spans(source, target, spans.as_slice()).expect("mapped");

        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].start, 0);
        assert_eq!(mapped[0].end, target.len());
        assert!(mapped[0].bold);
    }

    #[test]
    fn parse_inline_style_tags_tracks_font_label() {
        let parsed = parse_inline_style_tags("a<font=My Font><b>bc</b></font>d", 24.0);

        assert_eq!(parsed.plain_text, "abcd");
        assert_eq!(parsed.spans.len(), 3);
        assert_eq!(
            parsed
                .plain_text
                .get(parsed.spans[1].start..parsed.spans[1].end),
            Some("bc")
        );
        assert!(parsed.spans[1].bold);
        assert_eq!(parsed.spans[1].font_label.as_deref(), Some("My Font"));
        assert_eq!(parsed.spans[2].font_label, None);
    }

    #[test]
    fn parse_inline_style_tags_strips_no_break_control_tag() {
        let parsed = parse_inline_style_tags("a<no-break>b c</no-break>d", 24.0);

        assert_eq!(parsed.plain_text, "ab cd");
    }

    #[test]
    fn parse_inline_style_tags_tracks_line_alignment() {
        let parsed = parse_inline_style_tags("a<align=right>bc</align>d", 24.0);

        assert_eq!(parsed.plain_text, "abcd");
        assert_eq!(parsed.spans.len(), 3);
        assert_eq!(
            parsed
                .plain_text
                .get(parsed.spans[1].start..parsed.spans[1].end),
            Some("bc")
        );
        assert_eq!(parsed.spans[1].align, Some(HorizontalAlign::RIGHT));
        assert_eq!(parsed.spans[2].align, None);
    }

    #[test]
    fn parse_inline_style_tags_tracks_font_size_and_non_attrs_overrides() {
        let parsed = parse_inline_style_tags(
            "a<size=36><color=#11223344><offset=3,-4>bc</offset></color></size>d",
            24.0,
        );

        assert_eq!(parsed.plain_text, "abcd");
        assert_eq!(parsed.spans.len(), 3);
        assert_eq!(
            parsed
                .plain_text
                .get(parsed.spans[1].start..parsed.spans[1].end),
            Some("bc")
        );
        assert_eq!(parsed.spans[1].font_size_px, Some(36.0));
        assert_eq!(parsed.spans[1].text_color, Some([0x11, 0x22, 0x33, 0x44]));
        assert_eq!(
            parsed.spans[1].glyph_offset,
            Some(InlineGlyphOffset::global_only([3.0, -4.0]))
        );
        assert_eq!(parsed.spans[2].font_size_px, None);
        assert_eq!(parsed.spans[2].text_color, None);
        assert_eq!(parsed.spans[2].glyph_offset, None);
    }

    #[test]
    fn parse_machine_tag_combines_all_inline_params() {
        // Один компактный тег `<m ...>` задаёт сразу несколько параметров.
        let parsed = parse_inline_style_tags(
            "a<m b a=right s=36 f=\"My Font\" c=11223344 l=50% k=10 w=120% h=80% x=3 n=12 q g=30>bc</m>d",
            24.0,
        );

        assert_eq!(parsed.plain_text, "abcd");
        assert_eq!(parsed.spans.len(), 3);
        let span = &parsed.spans[1];
        assert_eq!(parsed.plain_text.get(span.start..span.end), Some("bc"));
        assert!(span.bold);
        assert_eq!(span.align, Some(HorizontalAlign::RIGHT));
        assert_eq!(span.font_size_px, Some(36.0));
        assert_eq!(span.font_label.as_deref(), Some("My Font"));
        assert_eq!(span.text_color, Some([0x11, 0x22, 0x33, 0x44]));
        // l=50% → проценты, k=10 → пиксели.
        assert_eq!(span.line_spacing_px, Some(0.0));
        assert_eq!(span.line_spacing_percent, Some(50.0));
        assert_eq!(span.kerning_px, Some(10.0));
        assert_eq!(span.kerning_percent, Some(0.0));
        assert_eq!(span.glyph_stretch_percent, Some([120.0, 80.0]));
        let Some(offset) = span.glyph_offset else {
            panic!("offset keys should produce an offset");
        };
        assert_eq!(offset.global_px, [3.0, 0.0]);
        assert_eq!(offset.line_px, 12.0);
        assert!(offset.shift_following);
        assert!((offset.group_rotation_rad.to_degrees() - 30.0).abs() < 0.01);

        // После `</m>` все стили сняты.
        assert!(!parsed.spans[2].bold);
        assert_eq!(parsed.spans[2].align, None);
        assert_eq!(parsed.spans[2].font_size_px, None);
        assert_eq!(parsed.spans[2].glyph_offset, None);
    }

    #[test]
    fn parse_inline_style_tags_tracks_extended_offset_fields() {
        let parsed = parse_inline_style_tags("a<offset=3,-4,12,1,30,-15>bc</offset>d", 24.0);

        assert_eq!(parsed.plain_text, "abcd");
        assert_eq!(parsed.spans.len(), 3);
        let Some(offset) = parsed.spans[1].glyph_offset else {
            panic!("extended offset should be parsed");
        };
        assert_eq!(offset.global_px, [3.0, -4.0]);
        assert_eq!(offset.line_px, 12.0);
        assert!(offset.shift_following);
        assert!((offset.group_rotation_rad.to_degrees() - 30.0).abs() < 0.01);
        assert!((offset.glyph_rotation_rad.to_degrees() + 15.0).abs() < 0.01);
    }

    #[test]
    fn apply_inline_style_to_attrs_updates_weight_style_and_metrics() {
        let attrs = Attrs::new().metrics(Metrics::new(20.0, 24.0));
        let style = InlineStyleSpan {
            start: 0,
            end: 3,
            bold: true,
            italic: true,
            align: None,
            font_label: None,
            font_size_px: Some(30.0),
            text_color: None,
            line_spacing_px: None,
            line_spacing_percent: None,
            kerning_px: None,
            kerning_percent: None,
            glyph_stretch_percent: None,
            glyph_offset: None,
        };

        let applied = apply_inline_style_to_attrs(&attrs, &style, &InlineFontRegistry::default());

        assert_eq!(applied.weight, Weight::BOLD);
        assert_eq!(applied.style, Style::Italic);
        let metrics = applied
            .metrics_opt
            .map(Into::<Metrics>::into)
            .expect("inline font size should produce metrics");
        assert_eq!(metrics.font_size, 30.0);
        assert_eq!(metrics.line_height, 36.0);
    }
}
