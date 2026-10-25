/*
File: crates/ms-text-render/src/pipeline/inline_lookup.rs

Purpose:
The ONE owner of the per-glyph inline-span lookups (`inline_*_at_offset` /
`inline_*_for_glyph`): text colour, glyph offset and its span/style, glyph scale and
inline kerning. The vertical and formula / custom-line paths import these instead of
keeping copies.
*/

use crate::inline_styles::{InlineGlyphOffset, InlineStyleSpan};
use crate::pair_advance::{KerningSettings, effective_spacing_percent};
use crate::types::TextRenderParams;
use cosmic_text::LayoutGlyph;
use super::GlyphScaleSettings;

pub(crate) fn spans_have_inline_size_overrides(spans: &[InlineStyleSpan]) -> bool {
    spans.iter().any(|span| span.font_size_px.is_some())
}

pub(super) fn inline_style_at_offset(spans: &[InlineStyleSpan], offset: usize) -> Option<&InlineStyleSpan> {
    spans
        .iter()
        .find(|span| span.start <= offset && offset < span.end)
}

pub(crate) fn inline_text_color_at_offset(
    default_text_color: [u8; 4],
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> [u8; 4] {
    spans
        .and_then(|style_spans| inline_style_at_offset(style_spans, offset))
        .and_then(|style| style.text_color)
        .unwrap_or(default_text_color)
}

pub(crate) fn inline_text_color_for_glyph(
    default_text_color: [u8; 4],
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> [u8; 4] {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_text_color_at_offset(
        default_text_color,
        spans,
        line_offset + glyph.start.min(glyph.end),
    )
}

pub(crate) fn inline_glyph_offset_at_offset(spans: Option<&[InlineStyleSpan]>, offset: usize) -> [f32; 2] {
    inline_glyph_offset_style_at_offset(spans, offset).global_px
}

pub(crate) fn inline_glyph_offset_style_at_offset(
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> InlineGlyphOffset {
    spans
        .and_then(|style_spans| inline_style_at_offset(style_spans, offset))
        .and_then(|style| style.glyph_offset)
        .unwrap_or_else(|| InlineGlyphOffset::global_only([0.0, 0.0]))
}

pub(crate) fn inline_glyph_offset_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> [f32; 2] {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_glyph_offset_at_offset(spans, line_offset + glyph.start.min(glyph.end))
}

pub(crate) fn inline_glyph_offset_style_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> InlineGlyphOffset {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_glyph_offset_style_at_offset(spans, line_offset + glyph.start.min(glyph.end))
}

/// Диапазон inline-спана, задающего смещение для глифа, — ключ для группировки
/// глифов, поворачиваемых как единая группа.
pub(crate) fn inline_glyph_offset_span_at_offset(
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> Option<(usize, usize)> {
    spans
        .and_then(|style_spans| inline_style_at_offset(style_spans, offset))
        .filter(|style| style.glyph_offset.is_some())
        .map(|style| (style.start, style.end))
}

pub(crate) fn inline_glyph_offset_span_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> Option<(usize, usize)> {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_glyph_offset_span_at_offset(spans, line_offset + glyph.start.min(glyph.end))
}

/// Есть ли среди inline-спанов смещения с ненулевым поворотом (группы или символа).
pub(super) fn spans_have_inline_rotation(spans: &[InlineStyleSpan]) -> bool {
    spans.iter().any(|span| {
        span.glyph_offset.is_some_and(|offset| {
            offset.group_rotation_rad.abs() > f32::EPSILON
                || offset.glyph_rotation_rad.abs() > f32::EPSILON
        })
    })
}

pub(crate) fn inline_glyph_scale_at_offset(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> GlyphScaleSettings {
    let stretch = spans
        .and_then(|style_spans| inline_style_at_offset(style_spans, offset))
        .and_then(|style| style.glyph_stretch_percent)
        .unwrap_or([params.glyph_width_percent, params.glyph_height_percent]);
    GlyphScaleSettings {
        width_mul: (stretch[0] / 100.0).clamp(0.01, 3.0),
        height_mul: (stretch[1] / 100.0).clamp(0.01, 3.0),
    }
}

pub(crate) fn inline_glyph_scale_for_glyph(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> GlyphScaleSettings {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_glyph_scale_at_offset(params, spans, line_offset + glyph.start.min(glyph.end))
}

pub(crate) fn inline_kerning_at_offset(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> KerningSettings {
    let style = spans.and_then(|style_spans| inline_style_at_offset(style_spans, offset));
    let stretch_x_percent = style
        .and_then(|value| value.glyph_stretch_percent)
        .map(|value| value[0])
        .unwrap_or(params.glyph_width_percent);
    let kerning_percent = style
        .and_then(|value| value.kerning_percent)
        .unwrap_or(params.kerning_percent);
    KerningSettings {
        mode: params.kerning_mode,
        spacing_px: style
            .and_then(|value| value.kerning_px)
            .unwrap_or(params.kerning_px)
            .clamp(-300.0, 300.0),
        spacing_percent: effective_spacing_percent(kerning_percent, stretch_x_percent),
        // Purely style-driven; the per-FONT override flag is stamped on afterwards
        // by `with_custom_pairs`, which is the only thing that knows the face.
        custom_pairs: false,
    }
}

pub(crate) fn inline_kerning_for_glyph(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> KerningSettings {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_kerning_at_offset(params, spans, line_offset + glyph.start.min(glyph.end))
}
