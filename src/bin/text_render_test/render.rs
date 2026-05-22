/*
FILE OVERVIEW: src/bin/text_render_test/render.rs
Рендер текста в RGBA-изображение через cosmic-text.

Ключевые структуры:
- `TextRenderParams`: входные параметры (text/font path + face index/color/size/line spacing px/%/width/align включая justify/bold+italic/rich tags/shape/min width %/strict shape/effects json).
- `RenderedTextImage`: буфер RGBA результата.
- `HorizontalAlign`: выравнивание строки (`Left/Center/Right/Justify`).
- `TextShape`: профиль переносов (`Free/Rectangle/Oval/Hexagon`).

Ключевые функции:
- `render_text_to_image`: формирует layout текста и растрирует glyph в RGBA.
- `reshape_text_for_shape`: подготавливает переносы под формы `free/rectangle/oval/hexagon`.
- `apply_effects_pipeline`: применяет эффекты из JSON по очереди (`stroke`, `shadow`, `glow_v1`, `glow_v2`, `gradient2`, `gradient4`, `reflect`, `shake`).
- `register_selected_font`: регистрирует выбранный файл шрифта и фиксирует family/style/weight/stretch.
- `soft_hyphenate_overlong`: ставит `U+00AD` в слишком широкие слова по словарю переносов.
- `sanitize_breaks`/`is_safe_hyphen_boundary`: чистят словарные точки переноса и отбрасывают плохие границы (например перед `ь/ъ` или между русской согласной и следующей гласной).
- `blend_pixel_over`: альфа-композитинг glyph-пикселя в итоговый буфер.
*/

use cosmic_text::{
    Align, Attrs, Buffer, Family, FontSystem, LayoutGlyph, Metrics, Shaping, Stretch, Style,
    SwashCache, SwashContent, Weight, fontdb,
};
use hyphenation::{Hyphenator, Language, Load, Standard};
use image::RgbaImage;
use serde_json::Value;
use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

const SOFT_HYPHEN: char = '\u{00AD}';

#[derive(Debug, Clone)]
pub struct TextRenderParams {
    pub text: String,
    pub text_color: [u8; 4],
    pub font_path: PathBuf,
    pub font_size_px: f32,
    pub line_spacing_px: f32,
    pub line_spacing_percent: f32,
    pub width_px: u32,
    pub align: HorizontalAlign,
    pub selected_face_index: usize,
    pub force_bold: bool,
    pub force_italic: bool,
    pub enable_inline_style_tags: bool,
    pub text_shape: TextShape,
    pub shape_min_width_percent: f32,
    pub shape_variant: u8,
    pub strict_shape_fit: bool,
    pub effects_json: String,
}

#[derive(Debug, Clone)]
pub struct RenderedTextImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizontalAlign {
    Left,
    Center,
    Right,
    Justify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextShape {
    Free,
    Rectangle,
    Oval,
    Hexagon,
    SoftPeak,
}

#[derive(Debug, Clone, Copy)]
struct PixelBounds {
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
    initialized: bool,
}

impl PixelBounds {
    fn empty() -> Self {
        Self {
            min_x: 0,
            min_y: 0,
            max_x: 0,
            max_y: 0,
            initialized: false,
        }
    }

    fn include_rect(&mut self, x: i32, y: i32, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }
        let rect_max_x = x.saturating_add(width);
        let rect_max_y = y.saturating_add(height);

        if !self.initialized {
            self.min_x = x;
            self.min_y = y;
            self.max_x = rect_max_x;
            self.max_y = rect_max_y;
            self.initialized = true;
            return;
        }

        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(rect_max_x);
        self.max_y = self.max_y.max(rect_max_y);
    }
}

pub fn render_text_to_image(params: &TextRenderParams) -> Result<RenderedTextImage, String> {
    let width_px = params.width_px.max(1);
    let font_size_px = params.font_size_px.max(1.0);
    let line_spacing_px = params.line_spacing_px;
    let line_spacing_percent = params.line_spacing_percent.clamp(-300.0, 300.0);
    let extra_line_spacing_px = line_spacing_px + font_size_px * (line_spacing_percent / 100.0);
    let base_line_height_px = font_size_px.max(1.0);
    let text = if params.text.is_empty() {
        " ".to_string()
    } else {
        params.text.clone()
    };

    let font_bytes = fs::read(&params.font_path).map_err(|err| {
        format!(
            "не удалось прочитать шрифт {}: {err}",
            params.font_path.display()
        )
    })?;
    let mut font_system = FontSystem::new();
    let selected_face =
        register_selected_font(&mut font_system, font_bytes, params.selected_face_index)
            .map_err(|err| format!("не удалось загрузить шрифт в fontdb: {err}"))?;

    let mut buffer = Buffer::new(
        &mut font_system,
        Metrics::new(font_size_px, base_line_height_px),
    );
    buffer.set_size(&mut font_system, Some(width_px as f32), None);
    let mut attrs = Attrs::new();
    if let Some(name) = selected_face.family_name.as_deref() {
        attrs = attrs.family(Family::Name(name));
    }
    if let Some(style) = selected_face.style {
        attrs = attrs.style(style);
    }
    if let Some(weight) = selected_face.weight {
        attrs = attrs.weight(weight);
    }
    if let Some(stretch) = selected_face.stretch {
        attrs = attrs.stretch(stretch);
    }
    if params.force_bold {
        attrs = attrs.weight(Weight::BOLD);
    }
    if params.force_italic {
        attrs = attrs.style(Style::Italic);
    }

    let parsed_inline_styles = if params.enable_inline_style_tags {
        Some(parse_inline_style_tags(text.as_str()))
    } else {
        None
    };
    let source_text = parsed_inline_styles
        .as_ref()
        .map(|parsed| parsed.plain_text.as_str())
        .unwrap_or(text.as_str());

    let hyphen_dicts = HyphenationDictionaries::new();
    let hyphenated_text = soft_hyphenate_overlong(
        source_text,
        &mut font_system,
        &attrs,
        font_size_px,
        base_line_height_px,
        width_px as f32,
        &hyphen_dicts,
    );
    let layout_text = reshape_text_for_shape(
        hyphenated_text.as_str(),
        &mut font_system,
        &attrs,
        font_size_px,
        base_line_height_px,
        width_px as f32,
        params.text_shape,
        params.shape_min_width_percent,
        params.shape_variant,
        params.strict_shape_fit,
    );
    let justify_alignment = justify_alignment_option(params.align);

    if let Some(parsed) = parsed_inline_styles.as_ref() {
        if let Some(mapped_spans) = remap_inline_style_spans(
            parsed.plain_text.as_str(),
            layout_text.as_str(),
            parsed.spans.as_slice(),
        ) {
            let spans_iter = mapped_spans.iter().filter_map(|span| {
                let text_slice = layout_text.get(span.start..span.end)?;
                let mut span_attrs = attrs.clone();
                if span.bold {
                    span_attrs = span_attrs.weight(Weight::BOLD);
                }
                if span.italic {
                    span_attrs = span_attrs.style(Style::Italic);
                }
                Some((text_slice, span_attrs))
            });
            buffer.set_rich_text(
                &mut font_system,
                spans_iter,
                &attrs,
                Shaping::Advanced,
                justify_alignment,
            );
        } else {
            buffer.set_text(
                &mut font_system,
                layout_text.as_str(),
                &attrs,
                Shaping::Advanced,
            );
            if let Some(alignment) = justify_alignment {
                for line in buffer.lines.iter_mut() {
                    line.set_align(Some(alignment));
                }
                buffer.shape_until_scroll(&mut font_system, false);
            }
        }
    } else {
        buffer.set_text(
            &mut font_system,
            layout_text.as_str(),
            &attrs,
            Shaping::Advanced,
        );
        if let Some(alignment) = justify_alignment {
            for line in buffer.lines.iter_mut() {
                line.set_align(Some(alignment));
            }
            buffer.shape_until_scroll(&mut font_system, false);
        }
    }

    let mut cache = SwashCache::new();
    let hard_hyphen_glyph =
        build_hard_hyphen_glyph(&mut font_system, &attrs, font_size_px, base_line_height_px);
    let mut bounds = PixelBounds::empty();

    let mut line_idx = 0usize;
    let mut runs = buffer.layout_runs().peekable();
    while let Some(run) = runs.next() {
        let line_offset_x = horizontal_line_offset(width_px, run.line_w, params.align);
        let line_offset_y = line_idx as f32 * extra_line_spacing_px;

        for glyph in run.glyphs.iter() {
            let physical = glyph.physical((line_offset_x as f32, run.line_y + line_offset_y), 1.0);
            let Some(image) = cache.get_image(&mut font_system, physical.cache_key) else {
                continue;
            };
            let x = physical.x + image.placement.left;
            let y = physical.y - image.placement.top;
            bounds.include_rect(
                x,
                y,
                image.placement.width as i32,
                image.placement.height as i32,
            );
        }

        if run_wraps_at_soft_hyphen(&run, runs.peek())
            && let Some(hyphen_glyph) = hard_hyphen_glyph.as_ref()
        {
            let hyphen_offset_x = line_offset_x as f32 + trailing_hyphen_x(&run);
            let hyphen_physical =
                hyphen_glyph.physical((hyphen_offset_x, run.line_y + line_offset_y), 1.0);
            if let Some(image) = cache.get_image(&mut font_system, hyphen_physical.cache_key) {
                let x = hyphen_physical.x + image.placement.left;
                let y = hyphen_physical.y - image.placement.top;
                bounds.include_rect(
                    x,
                    y,
                    image.placement.width as i32,
                    image.placement.height as i32,
                );
            }
        }

        line_idx += 1;
    }

    if !bounds.initialized {
        let height = base_line_height_px.ceil().max(1.0) as u32;
        return Ok(RenderedTextImage {
            width: width_px,
            height,
            rgba: vec![0; width_px as usize * height as usize * 4],
        });
    }

    let left_overhang = (-bounds.min_x).max(0) as u32;
    let right_overhang = (bounds.max_x - width_px as i32).max(0) as u32;
    let horizontal_pad = 2u32;
    let vertical_pad = 2u32;
    let side_safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;
    let top_safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;
    let bottom_safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;

    let out_width = width_px
        .saturating_add(left_overhang)
        .saturating_add(right_overhang)
        .saturating_add(horizontal_pad * 2)
        .saturating_add(side_safety_pad * 2);
    let content_height = (bounds.max_y - bounds.min_y).max(1) as u32;
    let min_height = base_line_height_px.ceil().max(1.0) as u32;
    let out_height = content_height
        .max(min_height)
        .saturating_add(vertical_pad * 2)
        .saturating_add(top_safety_pad)
        .saturating_add(bottom_safety_pad);

    let x_offset = left_overhang as i32 + horizontal_pad as i32 + side_safety_pad as i32;
    let y_offset = (-bounds.min_y).max(0) + vertical_pad as i32 + top_safety_pad as i32;
    let mut rgba = vec![0u8; out_width as usize * out_height as usize * 4];

    let mut line_idx = 0usize;
    let mut runs = buffer.layout_runs().peekable();
    while let Some(run) = runs.next() {
        let line_offset_x = horizontal_line_offset(width_px, run.line_w, params.align);
        let line_offset_y = line_idx as f32 * extra_line_spacing_px;

        for glyph in run.glyphs.iter() {
            let physical = glyph.physical((line_offset_x as f32, run.line_y + line_offset_y), 1.0);
            let Some(image) = cache.get_image(&mut font_system, physical.cache_key) else {
                continue;
            };

            let draw_x = physical.x + image.placement.left + x_offset;
            let draw_y = physical.y - image.placement.top + y_offset;
            let glyph_w = image.placement.width as usize;
            let glyph_h = image.placement.height as usize;

            if glyph_w == 0 || glyph_h == 0 {
                continue;
            }

            for gy in 0..glyph_h {
                for gx in 0..glyph_w {
                    let dst_x = draw_x + gx as i32;
                    let dst_y = draw_y + gy as i32;
                    if dst_x < 0
                        || dst_y < 0
                        || dst_x >= out_width as i32
                        || dst_y >= out_height as i32
                    {
                        continue;
                    }

                    let (src_r, src_g, src_b, src_a) = sample_swash_pixel(
                        &image.content,
                        image.data.as_slice(),
                        glyph_w,
                        gx,
                        gy,
                        params.text_color,
                    );
                    if src_a == 0 {
                        continue;
                    }

                    let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
                    blend_pixel_over(&mut rgba[dst_idx..dst_idx + 4], src_r, src_g, src_b, src_a);
                }
            }
        }

        if run_wraps_at_soft_hyphen(&run, runs.peek())
            && let Some(hyphen_glyph) = hard_hyphen_glyph.as_ref()
        {
            let hyphen_offset_x = line_offset_x as f32 + trailing_hyphen_x(&run);
            let hyphen_physical =
                hyphen_glyph.physical((hyphen_offset_x, run.line_y + line_offset_y), 1.0);
            if let Some(image) = cache.get_image(&mut font_system, hyphen_physical.cache_key) {
                let draw_x = hyphen_physical.x + image.placement.left + x_offset;
                let draw_y = hyphen_physical.y - image.placement.top + y_offset;
                let glyph_w = image.placement.width as usize;
                let glyph_h = image.placement.height as usize;

                for gy in 0..glyph_h {
                    for gx in 0..glyph_w {
                        let dst_x = draw_x + gx as i32;
                        let dst_y = draw_y + gy as i32;
                        if dst_x < 0
                            || dst_y < 0
                            || dst_x >= out_width as i32
                            || dst_y >= out_height as i32
                        {
                            continue;
                        }
                        let (src_r, src_g, src_b, src_a) = sample_swash_pixel(
                            &image.content,
                            image.data.as_slice(),
                            glyph_w,
                            gx,
                            gy,
                            params.text_color,
                        );
                        if src_a == 0 {
                            continue;
                        }
                        let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
                        blend_pixel_over(
                            &mut rgba[dst_idx..dst_idx + 4],
                            src_r,
                            src_g,
                            src_b,
                            src_a,
                        );
                    }
                }
            }
        }

        line_idx += 1;
    }

    let mut rendered = RenderedTextImage {
        width: out_width,
        height: out_height,
        rgba,
    };
    apply_effects_pipeline(&mut rendered, params.effects_json.as_str())?;
    Ok(rendered)
}

struct SelectedFaceAttrs {
    family_name: Option<String>,
    style: Option<Style>,
    weight: Option<Weight>,
    stretch: Option<Stretch>,
}

struct HyphenationDictionaries {
    russian: Option<Standard>,
    english_us: Option<Standard>,
}

impl HyphenationDictionaries {
    fn new() -> Self {
        Self {
            russian: Standard::from_embedded(Language::Russian).ok(),
            english_us: Standard::from_embedded(Language::EnglishUS).ok(),
        }
    }

    fn breaks_for_word(&self, word: &str) -> Vec<usize> {
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

fn register_selected_font(
    font_system: &mut FontSystem,
    font_bytes: Vec<u8>,
    selected_face_index: usize,
) -> Result<SelectedFaceAttrs, String> {
    let source = fontdb::Source::Binary(Arc::new(font_bytes));
    let loaded_ids = font_system.db_mut().load_font_source(source);
    if loaded_ids.is_empty() {
        return Err("fontdb не смог распарсить файл шрифта".to_string());
    }

    let mut selected = SelectedFaceAttrs {
        family_name: None,
        style: None,
        weight: None,
        stretch: None,
    };

    let face_id = loaded_ids
        .get(selected_face_index)
        .copied()
        .unwrap_or(loaded_ids[0]);
    if let Some(face) = font_system.db().face(face_id) {
        selected.family_name = face
            .families
            .first()
            .map(|(name, _)| name.clone())
            .or_else(|| {
                if face.post_script_name.is_empty() {
                    None
                } else {
                    Some(face.post_script_name.clone())
                }
            });
        selected.style = Some(face.style);
        selected.weight = Some(face.weight);
        selected.stretch = Some(face.stretch);
    }

    if let Some(family) = selected.family_name.as_ref() {
        let db = font_system.db_mut();
        db.set_sans_serif_family(family.clone());
        db.set_serif_family(family.clone());
        db.set_monospace_family(family.clone());
        db.set_cursive_family(family.clone());
        db.set_fantasy_family(family.clone());
    }

    Ok(selected)
}

#[derive(Clone, Copy)]
struct InlineStyleSpan {
    start: usize,
    end: usize,
    bold: bool,
    italic: bool,
}

struct ParsedInlineStyles {
    plain_text: String,
    spans: Vec<InlineStyleSpan>,
}

fn parse_inline_style_tags(text: &str) -> ParsedInlineStyles {
    let mut plain_text = String::with_capacity(text.len());
    let mut spans = Vec::<InlineStyleSpan>::new();

    let mut bold_depth = 0usize;
    let mut italic_depth = 0usize;
    let mut span_start = 0usize;
    let mut span_bold = false;
    let mut span_italic = false;

    let flush = |plain: &String,
                 spans: &mut Vec<InlineStyleSpan>,
                 start: &mut usize,
                 bold: bool,
                 italic: bool| {
        let end = plain.len();
        if end > *start {
            spans.push(InlineStyleSpan {
                start: *start,
                end,
                bold,
                italic,
            });
            *start = end;
        }
    };

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
                .filter(|c| !c.is_ascii_whitespace())
                .collect::<String>()
                .to_ascii_lowercase();

            match compact.as_str() {
                "b" | "strong" => {
                    flush(
                        &plain_text,
                        &mut spans,
                        &mut span_start,
                        span_bold,
                        span_italic,
                    );
                    bold_depth = bold_depth.saturating_add(1);
                    span_bold = bold_depth > 0;
                    span_italic = italic_depth > 0;
                    i = end + 1;
                    continue;
                }
                "/b" | "/strong" => {
                    flush(
                        &plain_text,
                        &mut spans,
                        &mut span_start,
                        span_bold,
                        span_italic,
                    );
                    bold_depth = bold_depth.saturating_sub(1);
                    span_bold = bold_depth > 0;
                    span_italic = italic_depth > 0;
                    i = end + 1;
                    continue;
                }
                "i" | "em" => {
                    flush(
                        &plain_text,
                        &mut spans,
                        &mut span_start,
                        span_bold,
                        span_italic,
                    );
                    italic_depth = italic_depth.saturating_add(1);
                    span_bold = bold_depth > 0;
                    span_italic = italic_depth > 0;
                    i = end + 1;
                    continue;
                }
                "/i" | "/em" => {
                    flush(
                        &plain_text,
                        &mut spans,
                        &mut span_start,
                        span_bold,
                        span_italic,
                    );
                    italic_depth = italic_depth.saturating_sub(1);
                    span_bold = bold_depth > 0;
                    span_italic = italic_depth > 0;
                    i = end + 1;
                    continue;
                }
                "br" | "br/" | "/br" => {
                    plain_text.push('\n');
                    i = end + 1;
                    continue;
                }
                _ => {}
            }
        }

        plain_text.push(ch);
        i += ch.len_utf8();
    }

    let end = plain_text.len();
    if end > span_start {
        spans.push(InlineStyleSpan {
            start: span_start,
            end,
            bold: span_bold,
            italic: span_italic,
        });
    }

    ParsedInlineStyles { plain_text, spans }
}

fn remap_inline_style_spans(
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

    let source_bytes = source_text.as_bytes();
    let target_bytes = target_text.as_bytes();
    let mut map = vec![0usize; source_bytes.len() + 1];

    let mut target_i = 0usize;
    map[0] = 0;
    for (source_i, source_byte) in source_bytes.iter().copied().enumerate() {
        while target_i < target_bytes.len() && target_bytes[target_i] != source_byte {
            target_i += 1;
        }
        if target_i >= target_bytes.len() {
            return None;
        }
        target_i += 1;
        map[source_i + 1] = target_i;
    }

    let mut mapped = Vec::<InlineStyleSpan>::with_capacity(source_spans.len());
    for span in source_spans.iter().copied() {
        if span.end <= span.start || span.end > map.len() - 1 {
            continue;
        }
        let start = map[span.start];
        let end = map[span.end];
        if end <= start {
            continue;
        }
        target_text.get(start..end)?;
        mapped.push(InlineStyleSpan {
            start,
            end,
            bold: span.bold,
            italic: span.italic,
        });
    }

    Some(mapped)
}

fn justify_alignment_option(align: HorizontalAlign) -> Option<Align> {
    match align {
        HorizontalAlign::Justify => Some(Align::Justified),
        _ => None,
    }
}

// Text layout parameters are distinct rendering properties; grouping them would obscure intent.
#[allow(clippy::too_many_arguments)]
fn reshape_text_for_shape(
    text: &str,
    font_system: &mut FontSystem,
    attrs: &Attrs,
    font_size_px: f32,
    line_height_px: f32,
    base_width_px: f32,
    shape: TextShape,
    min_width_percent: f32,
    shape_variant: u8,
    strict_shape_fit: bool,
) -> String {
    let base_width_px = base_width_px.max(1.0);
    let shape_variant = shape_variant.clamp(1, 9);
    if shape == TextShape::Free {
        return text.to_string();
    }

    if matches!(shape, TextShape::Rectangle | TextShape::SoftPeak) {
        let base_lines = wrap_text_with_targets(
            text,
            font_system,
            attrs,
            font_size_px,
            line_height_px,
            base_width_px,
            None,
            strict_shape_fit,
        );
        let target_width = rectangle_target_width(
            base_lines.as_slice(),
            font_system,
            attrs,
            font_size_px,
            line_height_px,
            base_width_px,
        );
        let target_width = if shape == TextShape::SoftPeak {
            target_width * (0.9 + f32::from(shape_variant) / 45.0)
        } else {
            target_width
        };
        let profile = vec![target_width; base_lines.len().max(1)];
        let balanced = wrap_text_with_targets(
            text,
            font_system,
            attrs,
            font_size_px,
            line_height_px,
            base_width_px,
            Some(profile.as_slice()),
            strict_shape_fit,
        );
        return balanced.join("\n");
    }

    let mut prev_profile: Option<Vec<f32>> = None;
    let mut lines = wrap_text_with_targets(
        text,
        font_system,
        attrs,
        font_size_px,
        line_height_px,
        base_width_px,
        None,
        strict_shape_fit,
    );

    const MAX_PASSES: usize = 3;
    let min_ratio = (min_width_percent / 100.0).clamp(0.01, 1.0);
    for _ in 0..MAX_PASSES {
        let profile = compute_shape_line_widths(lines.len(), base_width_px, shape, min_ratio);
        if prev_profile.as_ref() == Some(&profile) {
            break;
        }
        prev_profile = Some(profile.clone());
        lines = wrap_text_with_targets(
            text,
            font_system,
            attrs,
            font_size_px,
            line_height_px,
            base_width_px,
            Some(profile.as_slice()),
            strict_shape_fit,
        );
    }

    lines.join("\n")
}

fn rectangle_target_width(
    lines: &[String],
    font_system: &mut FontSystem,
    attrs: &Attrs,
    font_size_px: f32,
    line_height_px: f32,
    base_width_px: f32,
) -> f32 {
    let mut total = 0.0f32;
    let mut count = 0usize;
    for line in lines.iter() {
        let sample = line.trim();
        if sample.is_empty() {
            continue;
        }
        total += measure_word_width_px(sample, font_system, attrs, font_size_px, line_height_px);
        count += 1;
    }

    if count <= 1 {
        return base_width_px;
    }

    let avg_width = total / count as f32;
    (avg_width * 1.08).clamp(base_width_px * 0.5, base_width_px)
}

fn compute_shape_line_widths(
    line_count: usize,
    base_width_px: f32,
    shape: TextShape,
    min_ratio: f32,
) -> Vec<f32> {
    if line_count == 0 {
        return Vec::new();
    }
    if line_count == 1 {
        return vec![base_width_px.max(1.0)];
    }
    if !matches!(shape, TextShape::Oval | TextShape::Hexagon) {
        return vec![base_width_px.max(1.0); line_count];
    }

    let half = (line_count - 1) as f32 / 2.0;
    let mut widths = Vec::with_capacity(line_count);
    for i in 0..line_count {
        let u = if half > 0.0 {
            ((i as f32 - half).abs() / half).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let ratio = match shape {
            TextShape::Hexagon => 1.0 - (1.0 - min_ratio) * u,
            // Эллиптический профиль под форму "овала" из Python-реализации.
            TextShape::Oval => min_ratio + (1.0 - min_ratio) * (1.0 - u * u).sqrt(),
            _ => 1.0,
        };
        widths.push((base_width_px * ratio).max(1.0));
    }
    widths
}

// Text layout parameters are distinct rendering properties; grouping them would obscure intent.
#[allow(clippy::too_many_arguments)]
fn wrap_text_with_targets(
    text: &str,
    font_system: &mut FontSystem,
    attrs: &Attrs,
    font_size_px: f32,
    line_height_px: f32,
    base_width_px: f32,
    line_width_targets: Option<&[f32]>,
    strict_shape_fit: bool,
) -> Vec<String> {
    let mut out = Vec::<String>::new();
    let mut global_line_idx = 0usize;

    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            global_line_idx += 1;
            continue;
        }

        let mut wrapped = wrap_paragraph_with_targets(
            paragraph,
            font_system,
            attrs,
            font_size_px,
            line_height_px,
            base_width_px,
            line_width_targets,
            strict_shape_fit,
            &mut global_line_idx,
        );
        out.append(&mut wrapped);
    }

    if out.is_empty() {
        out.push(String::new());
    }
    out
}

// Text layout parameters are distinct rendering properties; grouping them would obscure intent.
#[allow(clippy::too_many_arguments)]
fn wrap_paragraph_with_targets(
    paragraph: &str,
    font_system: &mut FontSystem,
    attrs: &Attrs,
    font_size_px: f32,
    line_height_px: f32,
    base_width_px: f32,
    line_width_targets: Option<&[f32]>,
    strict_shape_fit: bool,
    global_line_idx: &mut usize,
) -> Vec<String> {
    let mut tokens = tokenize_paragraph(paragraph);
    let mut out = Vec::<String>::new();
    let mut current_line = String::new();

    while let Some(mut token) = tokens.pop_front() {
        if token.chars().all(|ch| ch.is_whitespace()) && current_line.is_empty() {
            continue;
        }

        loop {
            let max_width = line_target_width(line_width_targets, base_width_px, *global_line_idx);

            if token.chars().all(|ch| ch.is_whitespace()) {
                current_line.push_str(token.as_str());
                break;
            }

            let mut candidate = String::with_capacity(current_line.len() + token.len());
            candidate.push_str(current_line.as_str());
            candidate.push_str(token.as_str());
            let candidate_for_measure = candidate.trim_end_matches(|ch: char| ch.is_whitespace());
            let candidate_width = if candidate_for_measure.is_empty() {
                0.0
            } else {
                measure_word_width_px(
                    candidate_for_measure,
                    font_system,
                    attrs,
                    font_size_px,
                    line_height_px,
                )
            };

            if candidate_width <= max_width + 0.25 {
                current_line.push_str(token.as_str());
                break;
            }

            if strict_shape_fit
                && !current_line.trim().is_empty()
                && is_strict_splittable_token(token.as_str())
            {
                let (head, tail) = split_word_for_prefixed_width(
                    current_line.as_str(),
                    token.as_str(),
                    max_width,
                    font_system,
                    attrs,
                    font_size_px,
                    line_height_px,
                );

                if !head.is_empty() && !tail.is_empty() {
                    current_line.push_str(head.as_str());
                    let flushed = trim_line_end(current_line.as_str());
                    if !flushed.is_empty() {
                        out.push(flushed);
                        *global_line_idx += 1;
                    }
                    current_line.clear();
                    token = tail;
                    continue;
                }
            }

            let flushed = trim_line_end(current_line.as_str());
            if !flushed.is_empty() {
                out.push(flushed);
                *global_line_idx += 1;
                current_line.clear();
                continue;
            }

            let (head, tail) = split_word_for_width(
                token.as_str(),
                max_width,
                font_system,
                attrs,
                font_size_px,
                line_height_px,
            );

            if head.is_empty() {
                current_line.push_str(token.as_str());
                break;
            }

            out.push(head);
            *global_line_idx += 1;
            current_line.clear();

            if tail.is_empty() {
                break;
            }
            token = tail;
        }
    }

    let tail = trim_line_end(current_line.as_str());
    if !tail.is_empty() || out.is_empty() {
        out.push(tail);
        *global_line_idx += 1;
    }

    out
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
            _ => {}
        }
    }

    if start < paragraph.len() {
        tokens.push_back(paragraph[start..].to_string());
    }

    tokens
}

fn line_target_width(targets: Option<&[f32]>, base_width_px: f32, line_idx: usize) -> f32 {
    match targets {
        Some(vals) if !vals.is_empty() => {
            // Если в текущем проходе строк стало больше, чем было в профиле,
            // продолжаем использовать крайнее значение профиля (а не full width),
            // чтобы хвост не "взрывался" по ширине.
            vals.get(line_idx)
                .copied()
                .or_else(|| vals.last().copied())
                .unwrap_or(base_width_px)
                .max(1.0)
        }
        _ => base_width_px.max(1.0),
    }
}

fn split_word_for_prefixed_width(
    prefix: &str,
    word: &str,
    max_width_px: f32,
    font_system: &mut FontSystem,
    attrs: &Attrs,
    font_size_px: f32,
    line_height_px: f32,
) -> (String, String) {
    let prefix_trimmed = prefix.trim_end_matches(|ch: char| ch.is_whitespace());
    let total_chars = word.chars().count();
    if total_chars < 4 {
        return (String::new(), word.to_string());
    }

    let mut best_soft_break: Option<usize> = None;

    for (idx, ch) in word.char_indices() {
        if ch != SOFT_HYPHEN || idx == 0 {
            continue;
        }
        if !is_soft_hyphen_break_allowed(word, idx) {
            continue;
        }
        let left_chars = word[..idx].chars().count();
        let right_start = idx + SOFT_HYPHEN.len_utf8();
        let right_chars = if right_start < word.len() {
            word[right_start..].chars().count()
        } else {
            0
        };
        if left_chars < 2 || right_chars < 2 {
            continue;
        }

        let mut probe = String::with_capacity(prefix_trimmed.len() + idx + 1);
        probe.push_str(prefix_trimmed);
        probe.push_str(&word[..idx]);
        probe.push('-');
        let width = measure_word_width_px(
            probe.as_str(),
            font_system,
            attrs,
            font_size_px,
            line_height_px,
        );
        if width <= max_width_px + 0.25 {
            best_soft_break = Some(idx);
        } else {
            break;
        }
    }

    if let Some(idx) = best_soft_break {
        let mut head = String::with_capacity(idx + 1);
        head.push_str(&word[..idx]);
        head.push('-');
        let rest_start = idx + SOFT_HYPHEN.len_utf8();
        let tail = if rest_start < word.len() {
            word[rest_start..].to_string()
        } else {
            String::new()
        };
        return (head, tail);
    }

    let mut best_end = 0usize;
    let mut chars_seen = 0usize;
    for (idx, ch) in word.char_indices() {
        let end = idx + ch.len_utf8();
        chars_seen += 1;
        let right_chars = total_chars.saturating_sub(chars_seen);
        if chars_seen < 2 || right_chars < 2 {
            continue;
        }
        if !is_safe_hyphen_boundary_at(word, end) {
            continue;
        }
        let mut probe = String::with_capacity(prefix_trimmed.len() + end);
        probe.push_str(prefix_trimmed);
        probe.push_str(&word[..end]);
        probe.push('-');
        let width = measure_word_width_px(
            probe.as_str(),
            font_system,
            attrs,
            font_size_px,
            line_height_px,
        );
        if width <= max_width_px + 0.25 {
            best_end = end;
        } else {
            break;
        }
    }

    if best_end == 0 || best_end >= word.len() {
        return (String::new(), word.to_string());
    }

    let mut head = String::with_capacity(best_end + 1);
    head.push_str(&word[..best_end]);
    head.push('-');
    (head, word[best_end..].to_string())
}

fn is_strict_splittable_token(token: &str) -> bool {
    if token.chars().count() < 4 {
        return false;
    }
    token
        .chars()
        .all(|ch| ch.is_alphabetic() || ch == SOFT_HYPHEN)
}

fn trim_line_end(line: &str) -> String {
    line.trim_end_matches(|ch: char| ch.is_whitespace())
        .to_string()
}

fn split_word_for_width(
    word: &str,
    max_width_px: f32,
    font_system: &mut FontSystem,
    attrs: &Attrs,
    font_size_px: f32,
    line_height_px: f32,
) -> (String, String) {
    let mut best_soft_break: Option<usize> = None;
    for (idx, ch) in word.char_indices() {
        if ch != SOFT_HYPHEN || idx == 0 {
            continue;
        }
        if !is_soft_hyphen_break_allowed(word, idx) {
            continue;
        }
        let mut probe = String::with_capacity(idx + 1);
        probe.push_str(&word[..idx]);
        probe.push('-');
        let width = measure_word_width_px(
            probe.as_str(),
            font_system,
            attrs,
            font_size_px,
            line_height_px,
        );
        if width <= max_width_px + 0.25 {
            best_soft_break = Some(idx);
        } else {
            break;
        }
    }

    if let Some(idx) = best_soft_break {
        let mut head = String::with_capacity(idx + 1);
        head.push_str(&word[..idx]);
        head.push('-');
        let rest_start = idx + SOFT_HYPHEN.len_utf8();
        let tail = if rest_start < word.len() {
            word[rest_start..].to_string()
        } else {
            String::new()
        };
        return (head, tail);
    }

    let mut best_end = 0usize;
    for (idx, ch) in word.char_indices() {
        let end = idx + ch.len_utf8();
        if !is_safe_hyphen_boundary_at(word, end) {
            continue;
        }
        let width = measure_word_width_px(
            &word[..end],
            font_system,
            attrs,
            font_size_px,
            line_height_px,
        );
        if width <= max_width_px + 0.25 {
            best_end = end;
        } else {
            break;
        }
    }

    if best_end == 0
        && let Some((idx, ch)) = word.char_indices().next()
    {
        best_end = idx + ch.len_utf8();
    }

    if best_end >= word.len() {
        return (word.to_string(), String::new());
    }

    (word[..best_end].to_string(), word[best_end..].to_string())
}

fn soft_hyphenate_overlong(
    text: &str,
    font_system: &mut FontSystem,
    attrs: &Attrs,
    font_size_px: f32,
    line_height_px: f32,
    max_width_px: f32,
    dicts: &HyphenationDictionaries,
) -> String {
    let ranges = find_word_ranges(text);
    if ranges.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut tail_start = 0usize;
    for (start, end) in ranges {
        out.push_str(&text[tail_start..start]);
        let word = &text[start..end];
        let replacement = maybe_soft_hyphenate_word(
            word,
            font_system,
            attrs,
            font_size_px,
            line_height_px,
            max_width_px,
            dicts,
        )
        .unwrap_or_else(|| word.to_string());
        out.push_str(replacement.as_str());
        tail_start = end;
    }
    out.push_str(&text[tail_start..]);
    out
}

fn maybe_soft_hyphenate_word(
    word: &str,
    font_system: &mut FontSystem,
    attrs: &Attrs,
    font_size_px: f32,
    line_height_px: f32,
    max_width_px: f32,
    dicts: &HyphenationDictionaries,
) -> Option<String> {
    if word.chars().count() < 4 {
        return None;
    }
    if word.contains("://") || word.contains('@') || word.contains('-') {
        return None;
    }

    let measured_width =
        measure_word_width_px(word, font_system, attrs, font_size_px, line_height_px);
    if measured_width <= max_width_px {
        return None;
    }

    let breaks = dicts.breaks_for_word(word);
    if breaks.is_empty() {
        return None;
    }

    Some(insert_soft_hyphens(word, breaks.as_slice()))
}

fn measure_word_width_px(
    word: &str,
    font_system: &mut FontSystem,
    attrs: &Attrs,
    font_size_px: f32,
    line_height_px: f32,
) -> f32 {
    let mut measure = Buffer::new(
        font_system,
        Metrics::new(font_size_px.max(1.0), line_height_px.max(1.0)),
    );
    measure.set_size(font_system, None, None);
    measure.set_text(font_system, word, attrs, Shaping::Advanced);
    measure.shape_until_scroll(font_system, false);
    measure
        .layout_runs()
        .fold(0.0f32, |max_w, run| max_w.max(run.line_w))
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

fn contains_cyrillic(word: &str) -> bool {
    word.chars().any(|ch| {
        let cp = ch as u32;
        matches!(cp, 0x0400..=0x052F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F)
    })
}

fn sanitize_breaks(word: &str, mut breaks: Vec<usize>) -> Vec<usize> {
    breaks.retain(|&idx| {
        idx > 0
            && idx < word.len()
            && word.is_char_boundary(idx)
            && is_safe_hyphen_boundary_at(word, idx)
    });
    breaks.sort_unstable();
    breaks.dedup();
    breaks
}

fn is_soft_hyphen_break_allowed(word: &str, soft_hyphen_idx: usize) -> bool {
    if !word
        .get(soft_hyphen_idx..)
        .is_some_and(|tail| tail.starts_with(SOFT_HYPHEN))
    {
        return false;
    }
    let left = word[..soft_hyphen_idx].chars().next_back();
    let right = word[soft_hyphen_idx + SOFT_HYPHEN.len_utf8()..]
        .chars()
        .next();
    is_safe_hyphen_boundary(left, right)
}

fn is_safe_hyphen_boundary_at(word: &str, idx: usize) -> bool {
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
    if matches!(left, 'ь' | 'Ь' | 'ъ' | 'Ъ') || matches!(right, 'ь' | 'Ь' | 'ъ' | 'Ъ') {
        return false;
    }
    if is_cyrillic_consonant(left) && is_cyrillic_vowel(right) {
        return false;
    }
    true
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

fn build_hard_hyphen_glyph(
    font_system: &mut FontSystem,
    attrs: &Attrs,
    font_size_px: f32,
    line_height_px: f32,
) -> Option<LayoutGlyph> {
    let mut buffer = Buffer::new(
        font_system,
        Metrics::new(font_size_px.max(1.0), line_height_px.max(1.0)),
    );
    buffer.set_size(font_system, None, None);
    buffer.set_text(font_system, "-", attrs, Shaping::Advanced);
    buffer.shape_until_scroll(font_system, false);
    buffer
        .layout_runs()
        .next()
        .and_then(|run| run.glyphs.first().cloned())
}

fn run_wraps_at_soft_hyphen(
    run: &cosmic_text::LayoutRun<'_>,
    next: Option<&cosmic_text::LayoutRun<'_>>,
) -> bool {
    let Some(next_run) = next else {
        return false;
    };
    if next_run.line_i != run.line_i {
        return false;
    }

    let Some(last_glyph) = run.glyphs.last() else {
        return false;
    };
    let Some(next_first_glyph) = next_run.glyphs.first() else {
        return false;
    };

    let end = last_glyph.end.min(run.text.len());
    let next_start = next_first_glyph.start.min(run.text.len());

    if next_start >= end {
        if let Some(slice) = run.text.get(end..next_start)
            && slice.contains(SOFT_HYPHEN)
        {
            return true;
        }
        if run.text[..end].ends_with(SOFT_HYPHEN) {
            return true;
        }
    }

    false
}

fn trailing_hyphen_x(run: &cosmic_text::LayoutRun<'_>) -> f32 {
    let mut right = run.line_w;
    for glyph in run.glyphs.iter() {
        right = right.max(glyph.x + glyph.w);
    }
    right
}

fn horizontal_line_offset(width_px: u32, line_width: f32, align: HorizontalAlign) -> i32 {
    let free = (width_px as f32 - line_width).max(0.0);
    match align {
        HorizontalAlign::Left => 0,
        HorizontalAlign::Center => (free * 0.5).round() as i32,
        HorizontalAlign::Right => free.round() as i32,
        HorizontalAlign::Justify => 0,
    }
}

fn sample_swash_pixel(
    content: &SwashContent,
    data: &[u8],
    glyph_width: usize,
    x: usize,
    y: usize,
    text_color: [u8; 4],
) -> (u8, u8, u8, u8) {
    let pixel_idx = y.saturating_mul(glyph_width).saturating_add(x);
    let tint_alpha = text_color[3] as f32 / 255.0;
    match content {
        SwashContent::Mask => {
            let alpha = data.get(pixel_idx).copied().unwrap_or(0);
            let out_a = ((alpha as f32) * tint_alpha).round().clamp(0.0, 255.0) as u8;
            (text_color[0], text_color[1], text_color[2], out_a)
        }
        SwashContent::SubpixelMask => {
            let base = pixel_idx.saturating_mul(3);
            let r = data.get(base).copied().unwrap_or(0);
            let g = data.get(base + 1).copied().unwrap_or(0);
            let b = data.get(base + 2).copied().unwrap_or(0);
            let alpha = r.max(g).max(b);
            let out_a = ((alpha as f32) * tint_alpha).round().clamp(0.0, 255.0) as u8;
            (text_color[0], text_color[1], text_color[2], out_a)
        }
        SwashContent::Color => {
            let base = pixel_idx.saturating_mul(4);
            let r = data.get(base).copied().unwrap_or(255);
            let g = data.get(base + 1).copied().unwrap_or(255);
            let b = data.get(base + 2).copied().unwrap_or(255);
            let a = data.get(base + 3).copied().unwrap_or(255);
            let tint_r = ((r as u16 * text_color[0] as u16) / 255) as u8;
            let tint_g = ((g as u16 * text_color[1] as u16) / 255) as u8;
            let tint_b = ((b as u16 * text_color[2] as u16) / 255) as u8;
            let out_a = ((a as f32) * tint_alpha).round().clamp(0.0, 255.0) as u8;
            (tint_r, tint_g, tint_b, out_a)
        }
    }
}

fn blend_pixel_over(dst: &mut [u8], src_r: u8, src_g: u8, src_b: u8, src_a: u8) {
    let src_a_f = src_a as f32 / 255.0;
    if src_a_f <= f32::EPSILON {
        return;
    }

    let dst_r_f = dst[0] as f32 / 255.0;
    let dst_g_f = dst[1] as f32 / 255.0;
    let dst_b_f = dst[2] as f32 / 255.0;
    let dst_a_f = dst[3] as f32 / 255.0;

    let src_r_f = src_r as f32 / 255.0;
    let src_g_f = src_g as f32 / 255.0;
    let src_b_f = src_b as f32 / 255.0;

    let out_a = src_a_f + dst_a_f * (1.0 - src_a_f);
    if out_a <= f32::EPSILON {
        return;
    }

    let out_r = (src_r_f * src_a_f + dst_r_f * dst_a_f * (1.0 - src_a_f)) / out_a;
    let out_g = (src_g_f * src_a_f + dst_g_f * dst_a_f * (1.0 - src_a_f)) / out_a;
    let out_b = (src_b_f * src_a_f + dst_b_f * dst_a_f * (1.0 - src_a_f)) / out_a;

    dst[0] = (out_r * 255.0).round().clamp(0.0, 255.0) as u8;
    dst[1] = (out_g * 255.0).round().clamp(0.0, 255.0) as u8;
    dst[2] = (out_b * 255.0).round().clamp(0.0, 255.0) as u8;
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

fn apply_effects_pipeline(image: &mut RenderedTextImage, effects_json: &str) -> Result<(), String> {
    let effects_json = effects_json.trim();
    if effects_json.is_empty() {
        return Ok(());
    }

    let root: Value = serde_json::from_str(effects_json)
        .map_err(|err| format!("ошибка парсинга effects json: {err}"))?;
    let effects = root
        .as_array()
        .ok_or_else(|| "effects json должен быть массивом".to_string())?;

    for (idx, effect) in effects.iter().enumerate() {
        let Some(obj) = effect.as_object() else {
            return Err(format!("effects[{idx}] должен быть объектом"));
        };
        let enabled = obj.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        if !enabled {
            continue;
        }

        let effect_name = obj
            .get("effect")
            .or_else(|| obj.get("type"))
            .and_then(Value::as_str)
            .map(|v| v.trim().to_lowercase())
            .ok_or_else(|| format!("effects[{idx}] должен содержать поле effect/type"))?;

        match effect_name.as_str() {
            "stroke" => {
                let width_px = obj
                    .get("width")
                    .or_else(|| obj.get("width_px"))
                    .and_then(Value::as_f64)
                    .unwrap_or(2.0)
                    .clamp(0.0, 64.0) as f32;
                let color = parse_effect_color(obj.get("color"))?;
                let opacity_mode = parse_stroke_opacity_mode(obj)?;
                let transparency_percent = parse_stroke_transparency_percent(obj)?;
                apply_stroke_effect(image, width_px, color, opacity_mode, transparency_percent);
            }
            "shadow" => {
                let offset_x = parse_effect_i32(
                    obj.get("offset_x")
                        .or_else(|| obj.get("x"))
                        .or_else(|| obj.get("dx")),
                    "offset_x",
                    4,
                    -8192,
                    8192,
                )?;
                let offset_y = parse_effect_i32(
                    obj.get("offset_y")
                        .or_else(|| obj.get("y"))
                        .or_else(|| obj.get("dy")),
                    "offset_y",
                    4,
                    -8192,
                    8192,
                )?;
                let transparency_percent = parse_shadow_transparency_percent(obj)?;
                let blur_radius_px = parse_shadow_blur_radius_px(obj)?;
                let use_source_color = parse_shadow_use_source_color(obj);
                let color = parse_effect_color(obj.get("color"))?;
                apply_shadow_effect(
                    image,
                    offset_x,
                    offset_y,
                    transparency_percent,
                    blur_radius_px,
                    use_source_color,
                    color,
                );
            }
            "glow_v1" => {
                let glow = parse_glow_effect_params(obj)?;
                apply_glow_effect_v1(image, &glow);
            }
            "glow_v2" | "glow" => {
                let glow = parse_glow_effect_params(obj)?;
                apply_glow_effect_v2(image, &glow);
            }
            "gradient2" => {
                let gradient = parse_gradient2_effect_params(obj)?;
                apply_gradient2_effect(image, &gradient);
            }
            "gradient4" => {
                let gradient = parse_gradient4_effect_params(obj)?;
                apply_gradient4_effect(image, &gradient);
            }
            "reflect" | "mirror" | "flip" => {
                let axis = parse_reflect_axis(obj)?;
                apply_reflect_effect(image, axis);
            }
            "shake" => {
                let shake = parse_shake_effect_params(obj)?;
                apply_shake_effect(image, &shake);
            }
            other => {
                return Err(format!(
                    "effects[{idx}]: эффект '{other}' пока не поддержан"
                ));
            }
        }
    }

    Ok(())
}

fn parse_effect_color(value: Option<&Value>) -> Result<[u8; 4], String> {
    let Some(color) = value else {
        return Ok([0, 0, 0, 255]);
    };

    if let Some(arr) = color.as_array() {
        if arr.len() == 3 || arr.len() == 4 {
            let r = value_to_u8(&arr[0], "color[0]")?;
            let g = value_to_u8(&arr[1], "color[1]")?;
            let b = value_to_u8(&arr[2], "color[2]")?;
            let a = if arr.len() == 4 {
                value_to_u8(&arr[3], "color[3]")?
            } else {
                255
            };
            return Ok([r, g, b, a]);
        }
        return Err("color-массив должен быть [r,g,b] или [r,g,b,a]".to_string());
    }

    if let Some(obj) = color.as_object() {
        let r = obj
            .get("r")
            .ok_or_else(|| "color.r отсутствует".to_string())
            .and_then(|v| value_to_u8(v, "color.r"))?;
        let g = obj
            .get("g")
            .ok_or_else(|| "color.g отсутствует".to_string())
            .and_then(|v| value_to_u8(v, "color.g"))?;
        let b = obj
            .get("b")
            .ok_or_else(|| "color.b отсутствует".to_string())
            .and_then(|v| value_to_u8(v, "color.b"))?;
        let a = obj
            .get("a")
            .map(|v| value_to_u8(v, "color.a"))
            .transpose()?
            .unwrap_or(255);
        return Ok([r, g, b, a]);
    }

    Err("color должен быть объектом или массивом".to_string())
}

fn value_to_u8(value: &Value, label: &str) -> Result<u8, String> {
    let Some(raw) = value.as_f64() else {
        return Err(format!("{label} должен быть числом"));
    };
    if !(0.0..=255.0).contains(&raw) {
        return Err(format!("{label} должен быть в диапазоне 0..255"));
    }
    Ok(raw.round() as u8)
}

fn parse_effect_i32(
    value: Option<&Value>,
    label: &str,
    default: i32,
    min: i32,
    max: i32,
) -> Result<i32, String> {
    let Some(value) = value else {
        return Ok(default.clamp(min, max));
    };
    let Some(raw) = value.as_f64() else {
        return Err(format!("{label} должен быть числом"));
    };
    if raw < min as f64 || raw > max as f64 {
        return Err(format!("{label} должен быть в диапазоне {min}..{max}"));
    }
    Ok((raw.round() as i32).clamp(min, max))
}

fn parse_effect_f32_range(
    value: Option<&Value>,
    label: &str,
    default: f32,
    min: f32,
    max: f32,
) -> Result<f32, String> {
    let Some(value) = value else {
        return Ok(default.clamp(min, max));
    };
    let Some(raw) = value.as_f64() else {
        return Err(format!("{label} должен быть числом"));
    };
    if raw < min as f64 || raw > max as f64 {
        return Err(format!("{label} должен быть в диапазоне {min}..{max}"));
    }
    Ok((raw as f32).clamp(min, max))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReflectAxis {
    X,
    Y,
}

fn parse_reflect_axis(obj: &serde_json::Map<String, Value>) -> Result<ReflectAxis, String> {
    let Some(axis) = obj.get("axis").and_then(Value::as_str) else {
        return Ok(ReflectAxis::Y);
    };

    match axis.trim().to_ascii_lowercase().as_str() {
        "x" | "horizontal_axis" => Ok(ReflectAxis::X),
        "y" | "vertical_axis" => Ok(ReflectAxis::Y),
        other => Err(format!(
            "reflect.axis: неизвестное значение '{other}', ожидалось x/y"
        )),
    }
}

struct ShakeEffectParams {
    angle_deg: f32,
    up_px: f32,
    down_px: f32,
    steps: u32,
    base_fade: f32,
    decay: f32,
    blur_px: u32,
    autogrow: bool,
    grow_margin_px: u32,
}

fn parse_shake_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<ShakeEffectParams, String> {
    let angle_deg = parse_effect_f32_range(
        obj.get("angle_deg")
            .or_else(|| obj.get("angle"))
            .or_else(|| obj.get("direction_deg")),
        "shake.angle_deg",
        90.0,
        -3600.0,
        3600.0,
    )?;
    let up_px = parse_effect_f32_range(
        obj.get("up").or_else(|| obj.get("up_px")),
        "shake.up",
        0.0,
        0.0,
        8192.0,
    )?;
    let down_px = parse_effect_f32_range(
        obj.get("down").or_else(|| obj.get("down_px")),
        "shake.down",
        0.0,
        0.0,
        8192.0,
    )?;
    let steps = parse_effect_i32(obj.get("steps"), "shake.steps", 10, 0, 2048)? as u32;
    let base_fade = parse_effect_f32_range(
        obj.get("base_fade").or_else(|| obj.get("fade")),
        "shake.base_fade",
        0.30,
        0.0,
        1.0,
    )?;
    let decay = parse_effect_f32_range(obj.get("decay"), "shake.decay", 0.15, 0.0, 1.0)?;
    let blur_px = parse_effect_i32(
        obj.get("blur")
            .or_else(|| obj.get("blur_radius"))
            .or_else(|| obj.get("blur_px")),
        "shake.blur",
        0,
        0,
        1024,
    )? as u32;
    let autogrow = obj
        .get("autogrow")
        .or_else(|| obj.get("auto_grow"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let grow_margin_px = parse_effect_i32(
        obj.get("grow_margin")
            .or_else(|| obj.get("grow_margin_px"))
            .or_else(|| obj.get("extra_margin")),
        "shake.grow_margin",
        0,
        0,
        8192,
    )? as u32;

    Ok(ShakeEffectParams {
        angle_deg,
        up_px,
        down_px,
        steps,
        base_fade,
        decay,
        blur_px,
        autogrow,
        grow_margin_px,
    })
}

struct GlowEffectParams {
    radius_px: f32,
    color: [u8; 4],
    opacity_mode: StrokeOpacityMode,
    transparency_percent: f32,
    fade_strength: f32,
    fade_shift: f32,
}

fn parse_glow_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<GlowEffectParams, String> {
    let radius_px = parse_effect_f32_range(
        obj.get("radius")
            .or_else(|| obj.get("radius_px"))
            .or_else(|| obj.get("width")),
        "glow.radius",
        16.0,
        0.0,
        1024.0,
    )?;
    let color = parse_effect_color(obj.get("color"))?;
    let opacity_mode = parse_stroke_opacity_mode(obj)?;
    let transparency_percent = parse_stroke_transparency_percent(obj)?;
    let fade_strength = parse_effect_f32_range(
        obj.get("fade_strength")
            .or_else(|| obj.get("decay_strength"))
            .or_else(|| obj.get("falloff_strength")),
        "glow.fade_strength",
        0.0,
        -100.0,
        100.0,
    )?;
    let fade_shift = parse_effect_f32_range(
        obj.get("fade_shift")
            .or_else(|| obj.get("decay_shift"))
            .or_else(|| obj.get("falloff_shift")),
        "glow.fade_shift",
        0.0,
        -100.0,
        100.0,
    )?;
    Ok(GlowEffectParams {
        radius_px,
        color,
        opacity_mode,
        transparency_percent,
        fade_strength,
        fade_shift,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gradient2FillMode {
    AllOpaque,
    SpecificColor,
}

struct Gradient2EffectParams {
    color1: [u8; 4],
    color2: [u8; 4],
    angle_deg: f32,
    respect_source_alpha: bool,
    fill_mode: Gradient2FillMode,
    target_color: [u8; 4],
}

fn parse_gradient2_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<Gradient2EffectParams, String> {
    let color1 = if let Some(value) = obj
        .get("color1")
        .or_else(|| obj.get("start_color"))
        .or_else(|| obj.get("from_color"))
    {
        parse_effect_color(Some(value))?
    } else {
        [255, 255, 255, 255]
    };
    let color2 = if let Some(value) = obj
        .get("color2")
        .or_else(|| obj.get("end_color"))
        .or_else(|| obj.get("to_color"))
    {
        parse_effect_color(Some(value))?
    } else {
        [0, 0, 0, 255]
    };
    let angle_deg = parse_effect_f32_range(
        obj.get("angle_deg")
            .or_else(|| obj.get("angle"))
            .or_else(|| obj.get("rotation")),
        "gradient2.angle_deg",
        90.0,
        -3600.0,
        3600.0,
    )?;
    let respect_source_alpha = obj
        .get("respect_source_alpha")
        .or_else(|| obj.get("consider_alpha"))
        .or_else(|| obj.get("use_alpha"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let fill_mode = parse_gradient2_fill_mode(obj)?;
    let target_color = if let Some(value) = obj
        .get("target_color")
        .or_else(|| obj.get("source_color"))
        .or_else(|| obj.get("mask_color"))
    {
        parse_effect_color(Some(value))?
    } else {
        [255, 255, 255, 255]
    };

    Ok(Gradient2EffectParams {
        color1,
        color2,
        angle_deg,
        respect_source_alpha,
        fill_mode,
        target_color,
    })
}

fn parse_gradient2_fill_mode(
    obj: &serde_json::Map<String, Value>,
) -> Result<Gradient2FillMode, String> {
    let Some(mode) = obj
        .get("fill_mode")
        .or_else(|| obj.get("mode"))
        .or_else(|| obj.get("fill"))
        .and_then(Value::as_str)
    else {
        return Ok(Gradient2FillMode::AllOpaque);
    };

    match mode.trim().to_ascii_lowercase().as_str() {
        "all_opaque" | "all_non_transparent" | "all" | "opaque" => Ok(Gradient2FillMode::AllOpaque),
        "specific_color" | "color" | "target_color" => Ok(Gradient2FillMode::SpecificColor),
        other => Err(format!(
            "gradient2.fill_mode: неизвестное значение '{other}', ожидалось all_opaque/specific_color"
        )),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gradient4FillMode {
    AllOpaque,
    SpecificColor,
}

struct Gradient4EffectParams {
    color_top_left: [u8; 4],
    color_top_right: [u8; 4],
    color_bottom_left: [u8; 4],
    color_bottom_right: [u8; 4],
    respect_source_alpha: bool,
    fill_mode: Gradient4FillMode,
    target_color: [u8; 4],
}

fn parse_gradient4_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<Gradient4EffectParams, String> {
    let color_top_left = if let Some(value) = obj
        .get("color_top_left")
        .or_else(|| obj.get("color_tl"))
        .or_else(|| obj.get("top_left_color"))
    {
        parse_effect_color(Some(value))?
    } else {
        [255, 255, 255, 255]
    };
    let color_top_right = if let Some(value) = obj
        .get("color_top_right")
        .or_else(|| obj.get("color_tr"))
        .or_else(|| obj.get("top_right_color"))
    {
        parse_effect_color(Some(value))?
    } else {
        [255, 255, 255, 255]
    };
    let color_bottom_left = if let Some(value) = obj
        .get("color_bottom_left")
        .or_else(|| obj.get("color_bl"))
        .or_else(|| obj.get("bottom_left_color"))
    {
        parse_effect_color(Some(value))?
    } else {
        [0, 0, 0, 255]
    };
    let color_bottom_right = if let Some(value) = obj
        .get("color_bottom_right")
        .or_else(|| obj.get("color_br"))
        .or_else(|| obj.get("bottom_right_color"))
    {
        parse_effect_color(Some(value))?
    } else {
        [0, 0, 0, 255]
    };
    let respect_source_alpha = obj
        .get("respect_source_alpha")
        .or_else(|| obj.get("consider_alpha"))
        .or_else(|| obj.get("use_alpha"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let fill_mode = parse_gradient4_fill_mode(obj)?;
    let target_color = if let Some(value) = obj
        .get("target_color")
        .or_else(|| obj.get("source_color"))
        .or_else(|| obj.get("mask_color"))
    {
        parse_effect_color(Some(value))?
    } else {
        [255, 255, 255, 255]
    };

    Ok(Gradient4EffectParams {
        color_top_left,
        color_top_right,
        color_bottom_left,
        color_bottom_right,
        respect_source_alpha,
        fill_mode,
        target_color,
    })
}

fn parse_gradient4_fill_mode(
    obj: &serde_json::Map<String, Value>,
) -> Result<Gradient4FillMode, String> {
    let Some(mode) = obj
        .get("fill_mode")
        .or_else(|| obj.get("mode"))
        .or_else(|| obj.get("fill"))
        .and_then(Value::as_str)
    else {
        return Ok(Gradient4FillMode::AllOpaque);
    };

    match mode.trim().to_ascii_lowercase().as_str() {
        "all_opaque" | "all_non_transparent" | "all" | "opaque" => Ok(Gradient4FillMode::AllOpaque),
        "specific_color" | "color" | "target_color" => Ok(Gradient4FillMode::SpecificColor),
        other => Err(format!(
            "gradient4.fill_mode: неизвестное значение '{other}', ожидалось all_opaque/specific_color"
        )),
    }
}

fn parse_shadow_use_source_color(obj: &serde_json::Map<String, Value>) -> bool {
    if let Some(value) = obj.get("use_source_color").and_then(Value::as_bool) {
        return value;
    }

    let Some(mode) = obj.get("mode").and_then(Value::as_str) else {
        return false;
    };

    matches!(
        mode.trim().to_ascii_lowercase().as_str(),
        "source" | "source_colors" | "original" | "original_colors"
    )
}

fn parse_shadow_transparency_percent(obj: &serde_json::Map<String, Value>) -> Result<f32, String> {
    if let Some(value) = obj.get("transparency") {
        let Some(raw) = value.as_f64() else {
            return Err("shadow.transparency должен быть числом".to_string());
        };
        if !(0.0..=100.0).contains(&raw) {
            return Err("shadow.transparency должен быть в диапазоне 0..100".to_string());
        }
        return Ok(raw as f32);
    }

    if let Some(value) = obj.get("opacity") {
        let Some(raw) = value.as_f64() else {
            return Err("shadow.opacity должен быть числом".to_string());
        };
        if !(0.0..=100.0).contains(&raw) {
            return Err("shadow.opacity должен быть в диапазоне 0..100".to_string());
        }
        return Ok((100.0 - raw as f32).clamp(0.0, 100.0));
    }

    Ok(40.0)
}

fn parse_shadow_blur_radius_px(obj: &serde_json::Map<String, Value>) -> Result<f32, String> {
    parse_effect_f32_range(
        obj.get("blur")
            .or_else(|| obj.get("blur_radius"))
            .or_else(|| obj.get("blur_px")),
        "shadow.blur",
        0.0,
        0.0,
        256.0,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StrokeOpacityMode {
    Static,
    FromContour,
}

fn parse_stroke_opacity_mode(
    obj: &serde_json::Map<String, Value>,
) -> Result<StrokeOpacityMode, String> {
    let Some(mode) = obj
        .get("opacity_mode")
        .or_else(|| obj.get("alpha_mode"))
        .and_then(Value::as_str)
    else {
        return Ok(StrokeOpacityMode::FromContour);
    };

    match mode.trim().to_ascii_lowercase().as_str() {
        "from_contour" | "contour" | "auto" => Ok(StrokeOpacityMode::FromContour),
        "static" | "fixed" => Ok(StrokeOpacityMode::Static),
        other => Err(format!(
            "stroke.opacity_mode: неизвестное значение '{other}', ожидалось static/from_contour"
        )),
    }
}

fn parse_stroke_transparency_percent(obj: &serde_json::Map<String, Value>) -> Result<f32, String> {
    if let Some(value) = obj.get("transparency") {
        let Some(raw) = value.as_f64() else {
            return Err("stroke.transparency должен быть числом".to_string());
        };
        if !(0.0..=100.0).contains(&raw) {
            return Err("stroke.transparency должен быть в диапазоне 0..100".to_string());
        }
        return Ok(raw as f32);
    }

    if let Some(value) = obj.get("opacity") {
        let Some(raw) = value.as_f64() else {
            return Err("stroke.opacity должен быть числом".to_string());
        };
        if !(0.0..=100.0).contains(&raw) {
            return Err("stroke.opacity должен быть в диапазоне 0..100".to_string());
        }
        return Ok((100.0 - raw as f32).clamp(0.0, 100.0));
    }

    Ok(0.0)
}

fn apply_stroke_effect(
    image: &mut RenderedTextImage,
    width_px: f32,
    color: [u8; 4],
    opacity_mode: StrokeOpacityMode,
    transparency_percent: f32,
) {
    if width_px <= 0.0 {
        return;
    }
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let radius = width_px.ceil().max(1.0) as i32;
    let mut stroke_alpha = vec![0u8; width * height];
    let source = image.rgba.clone();
    let static_opacity = (1.0 - transparency_percent.clamp(0.0, 100.0) / 100.0).clamp(0.0, 1.0);
    let static_alpha = (static_opacity * 255.0).round().clamp(0.0, 255.0) as u8;
    let static_tinted_alpha = ((static_alpha as u16 * color[3] as u16) / 255) as u8;

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }

            for oy in -radius..=radius {
                for ox in -radius..=radius {
                    if ox * ox + oy * oy > radius * radius {
                        continue;
                    }
                    let tx = x as i32 + ox;
                    let ty = y as i32 + oy;
                    if tx < 0 || ty < 0 || tx >= width as i32 || ty >= height as i32 {
                        continue;
                    }
                    let tidx = ty as usize * width + tx as usize;
                    stroke_alpha[tidx] = stroke_alpha[tidx].max(src_a);
                }
            }
        }
    }

    let mut out = vec![0u8; source.len()];
    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            let rgba_idx = idx * 4;
            let src_a = source[rgba_idx + 3];
            let stroke_only_a = stroke_alpha[idx].saturating_sub(src_a);
            let stroke_out_a = match opacity_mode {
                StrokeOpacityMode::FromContour => {
                    ((stroke_only_a as u16 * color[3] as u16) / 255) as u8
                }
                StrokeOpacityMode::Static => {
                    if stroke_only_a > 0 {
                        static_tinted_alpha
                    } else {
                        0
                    }
                }
            };
            if stroke_out_a > 0 {
                blend_pixel_over(
                    &mut out[rgba_idx..rgba_idx + 4],
                    color[0],
                    color[1],
                    color[2],
                    stroke_out_a,
                );
            }
            blend_pixel_over(
                &mut out[rgba_idx..rgba_idx + 4],
                source[rgba_idx],
                source[rgba_idx + 1],
                source[rgba_idx + 2],
                source[rgba_idx + 3],
            );
        }
    }

    image.rgba = out;
}

fn apply_glow_effect_v1(image: &mut RenderedTextImage, glow: &GlowEffectParams) {
    let radius = glow.radius_px.max(0.0);
    if radius <= f32::EPSILON {
        return;
    }

    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let pad = radius.ceil().max(1.0) as u32;
    let out_width = image.width.saturating_add(pad.saturating_mul(2));
    let out_height = image.height.saturating_add(pad.saturating_mul(2));
    if out_width == 0 || out_height == 0 {
        return;
    }

    let static_opacity =
        (1.0 - glow.transparency_percent.clamp(0.0, 100.0) / 100.0).clamp(0.0, 1.0);
    let color_alpha_factor = glow.color[3] as f32 / 255.0;
    if color_alpha_factor <= f32::EPSILON {
        return;
    }

    let mut offsets = Vec::<(i32, i32, f32)>::new();
    let radius_i = radius.ceil() as i32;
    for oy in -radius_i..=radius_i {
        for ox in -radius_i..=radius_i {
            let dist = ((ox * ox + oy * oy) as f32).sqrt();
            if dist > radius {
                continue;
            }
            let dist_norm = (dist / radius).clamp(0.0, 1.0);
            let falloff = glow_falloff_alpha(dist_norm, glow.fade_strength, glow.fade_shift);
            if falloff <= f32::EPSILON {
                continue;
            }
            offsets.push((ox, oy, falloff));
        }
    }
    if offsets.is_empty() {
        return;
    }

    let source = image.rgba.clone();
    let mut out = vec![0u8; out_width as usize * out_height as usize * 4];
    let mut source_alpha_expanded = vec![0u8; out_width as usize * out_height as usize];
    let mut glow_alpha = vec![0u8; out_width as usize * out_height as usize];
    let origin_x = pad as i32;
    let origin_y = pad as i32;

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }

            let base_x = origin_x + x as i32;
            let base_y = origin_y + y as i32;
            let base_idx = base_y as usize * out_width as usize + base_x as usize;
            source_alpha_expanded[base_idx] = src_a;
            let contour_alpha = src_a as f32 / 255.0;

            for (ox, oy, falloff) in offsets.iter() {
                let tx = base_x + *ox;
                let ty = base_y + *oy;
                if tx < 0 || ty < 0 || tx >= out_width as i32 || ty >= out_height as i32 {
                    continue;
                }
                let alpha_f = match glow.opacity_mode {
                    StrokeOpacityMode::FromContour => contour_alpha * *falloff,
                    StrokeOpacityMode::Static => static_opacity * *falloff,
                };
                if alpha_f <= f32::EPSILON {
                    continue;
                }

                let alpha_u8 = (alpha_f * 255.0).round().clamp(0.0, 255.0) as u8;
                let idx = ty as usize * out_width as usize + tx as usize;
                glow_alpha[idx] = glow_alpha[idx].max(alpha_u8);
            }
        }
    }

    for idx in 0..glow_alpha.len() {
        let glow_only_a = glow_alpha[idx].saturating_sub(source_alpha_expanded[idx]);
        if glow_only_a == 0 {
            continue;
        }
        let glow_a = ((glow_only_a as f32) * color_alpha_factor)
            .round()
            .clamp(0.0, 255.0) as u8;
        if glow_a == 0 {
            continue;
        }
        let rgba_idx = idx * 4;
        blend_pixel_over(
            &mut out[rgba_idx..rgba_idx + 4],
            glow.color[0],
            glow.color[1],
            glow.color[2],
            glow_a,
        );
    }

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }
            let dst_x = origin_x + x as i32;
            let dst_y = origin_y + y as i32;
            let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
            blend_pixel_over(
                &mut out[dst_idx..dst_idx + 4],
                source[src_idx],
                source[src_idx + 1],
                source[src_idx + 2],
                src_a,
            );
        }
    }

    image.width = out_width;
    image.height = out_height;
    image.rgba = out;
}

fn apply_glow_effect_v2(image: &mut RenderedTextImage, glow: &GlowEffectParams) {
    let radius = glow.radius_px.max(0.0);
    if radius <= f32::EPSILON {
        return;
    }

    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let pad = radius.ceil().max(1.0) as u32;
    let out_width = image.width.saturating_add(pad.saturating_mul(2));
    let out_height = image.height.saturating_add(pad.saturating_mul(2));
    if out_width == 0 || out_height == 0 {
        return;
    }

    let static_opacity =
        (1.0 - glow.transparency_percent.clamp(0.0, 100.0) / 100.0).clamp(0.0, 1.0);
    let color_alpha_factor = glow.color[3] as f32 / 255.0;
    if color_alpha_factor <= f32::EPSILON {
        return;
    }

    let source = image.rgba.clone();
    let mut out = vec![0u8; out_width as usize * out_height as usize * 4];
    let mut source_alpha_expanded = vec![0u8; out_width as usize * out_height as usize];
    let mut contour_mask = vec![0u8; out_width as usize * out_height as usize];
    let origin_x = pad as i32;
    let origin_y = pad as i32;
    let mut has_contour = false;

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }

            let base_x = origin_x + x as i32;
            let base_y = origin_y + y as i32;
            let base_idx = base_y as usize * out_width as usize + base_x as usize;
            source_alpha_expanded[base_idx] = src_a;
            contour_mask[base_idx] = 1;
            has_contour = true;
        }
    }
    if !has_contour {
        return;
    }

    let dist2_map = euclidean_distance_transform_to_mask(
        contour_mask.as_slice(),
        out_width as usize,
        out_height as usize,
    );
    let radius2 = radius * radius;

    for idx in 0..dist2_map.len() {
        if source_alpha_expanded[idx] > 0 {
            continue;
        }
        let dist2 = dist2_map[idx];
        if !dist2.is_finite() || dist2 > radius2 {
            continue;
        }
        let dist = dist2.sqrt();
        let falloff = glow_falloff_alpha(
            (dist / radius).clamp(0.0, 1.0),
            glow.fade_strength,
            glow.fade_shift,
        );
        if falloff <= f32::EPSILON {
            continue;
        }

        let base_opacity = match glow.opacity_mode {
            StrokeOpacityMode::FromContour => 1.0,
            StrokeOpacityMode::Static => static_opacity,
        };
        let glow_a = (base_opacity * falloff * color_alpha_factor * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8;
        if glow_a == 0 {
            continue;
        }
        let rgba_idx = idx * 4;
        blend_pixel_over(
            &mut out[rgba_idx..rgba_idx + 4],
            glow.color[0],
            glow.color[1],
            glow.color[2],
            glow_a,
        );
    }

    // Поверх свечения кладём исходный текст без изменений.
    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }
            let dst_x = origin_x + x as i32;
            let dst_y = origin_y + y as i32;
            let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
            blend_pixel_over(
                &mut out[dst_idx..dst_idx + 4],
                source[src_idx],
                source[src_idx + 1],
                source[src_idx + 2],
                src_a,
            );
        }
    }

    image.width = out_width;
    image.height = out_height;
    image.rgba = out;
}

fn glow_falloff_alpha(distance_norm: f32, fade_strength: f32, fade_shift: f32) -> f32 {
    let dist = distance_norm.clamp(0.0, 1.0);
    let shifted = bias01(dist, (0.5 - (fade_shift / 100.0) * 0.49).clamp(0.01, 0.99));
    let shaped = shape_falloff_progress(shifted, fade_strength);
    (1.0 - shaped).clamp(0.0, 1.0)
}

fn shape_falloff_progress(t: f32, fade_strength: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let s = (fade_strength / 100.0).clamp(-1.0, 1.0);
    if s.abs() <= f32::EPSILON {
        return t;
    }

    const K_MAX: f32 = 12.0;
    if s < 0.0 {
        let k = (-s) * K_MAX;
        ((1.0 + k * t).ln() / (1.0 + k).ln()).clamp(0.0, 1.0)
    } else {
        let k = s * K_MAX;
        (((1.0 + k).powf(t) - 1.0) / k).clamp(0.0, 1.0)
    }
}

fn bias01(t: f32, b: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 || t >= 1.0 {
        return t;
    }
    let b = b.clamp(0.01, 0.99);
    let k = (1.0 / b) - 2.0;
    (t / (k * (1.0 - t) + 1.0)).clamp(0.0, 1.0)
}

fn euclidean_distance_transform_to_mask(mask: &[u8], width: usize, height: usize) -> Vec<f32> {
    const INF: f32 = 1.0e15;

    let mut tmp = vec![INF; width * height];
    let mut dist2 = vec![INF; width * height];
    let mut f = vec![0.0f32; width.max(height)];
    let mut d = vec![0.0f32; width.max(height)];

    for x in 0..width {
        for (y, fi) in f[..height].iter_mut().enumerate() {
            let idx = y * width + x;
            *fi = if mask[idx] > 0 { 0.0 } else { INF };
        }
        edt_1d(f[..height].as_ref(), d[..height].as_mut(), INF);
        for y in 0..height {
            tmp[y * width + x] = d[y];
        }
    }

    for y in 0..height {
        let row = y * width;
        f[..width].copy_from_slice(&tmp[row..(width + row)]);
        edt_1d(f[..width].as_ref(), d[..width].as_mut(), INF);
        dist2[row..(width + row)].copy_from_slice(&d[..width]);
    }

    dist2
}

fn edt_1d(f: &[f32], out: &mut [f32], inf: f32) {
    let n = f.len();
    if n == 0 {
        return;
    }
    if !f.iter().any(|&v| v < inf * 0.5) {
        out.fill(inf);
        return;
    }

    let mut v = vec![0usize; n];
    let mut z = vec![0.0f32; n + 1];
    let mut k = 0usize;

    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;

    for q in 1..n {
        let mut s = edt_intersection(f, q, v[k]);
        while s <= z[k] {
            if k == 0 {
                break;
            }
            k -= 1;
            s = edt_intersection(f, q, v[k]);
        }

        if s <= z[k] && k == 0 {
            v[0] = q;
            z[0] = f32::NEG_INFINITY;
            z[1] = f32::INFINITY;
            k = 0;
            continue;
        }

        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f32::INFINITY;
    }

    let mut kk = 0usize;
    for (q, out_q) in out[..n].iter_mut().enumerate() {
        while z[kk + 1] < q as f32 {
            kk += 1;
        }
        let p = v[kk];
        let dq = q as f32 - p as f32;
        *out_q = dq * dq + f[p];
    }
}

fn edt_intersection(f: &[f32], q: usize, p: usize) -> f32 {
    let qf = q as f32;
    let pf = p as f32;
    ((f[q] + qf * qf) - (f[p] + pf * pf)) / (2.0 * (qf - pf))
}

fn apply_gradient2_effect(image: &mut RenderedTextImage, gradient: &Gradient2EffectParams) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let source = image.rgba.clone();
    let mut min_x = width as i32;
    let mut min_y = height as i32;
    let mut max_x = -1i32;
    let mut max_y = -1i32;
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            if source[idx + 3] == 0 {
                continue;
            }
            min_x = min_x.min(x as i32);
            min_y = min_y.min(y as i32);
            max_x = max_x.max(x as i32);
            max_y = max_y.max(y as i32);
        }
    }
    if max_x < min_x || max_y < min_y {
        return;
    }

    let bbox_w = (max_x - min_x + 1) as usize;
    let bbox_h = (max_y - min_y + 1) as usize;
    if bbox_w == 0 || bbox_h == 0 {
        return;
    }

    let angle_rad = gradient.angle_deg.to_radians();
    let dir_x = angle_rad.cos();
    let dir_y = angle_rad.sin();
    let center_x = (bbox_w as f32 - 1.0) * 0.5;
    let center_y = (bbox_h as f32 - 1.0) * 0.5;

    let mut min_proj = f32::INFINITY;
    let mut max_proj = f32::NEG_INFINITY;
    let corners = [
        (0.0f32, 0.0f32),
        ((bbox_w as f32 - 1.0).max(0.0), 0.0f32),
        (0.0f32, (bbox_h as f32 - 1.0).max(0.0)),
        (
            (bbox_w as f32 - 1.0).max(0.0),
            (bbox_h as f32 - 1.0).max(0.0),
        ),
    ];
    for (x, y) in corners {
        let proj = (x - center_x) * dir_x + (y - center_y) * dir_y;
        min_proj = min_proj.min(proj);
        max_proj = max_proj.max(proj);
    }
    let proj_range = (max_proj - min_proj).max(f32::EPSILON);

    let mut out = source.clone();
    for y in 0..bbox_h {
        for x in 0..bbox_w {
            let image_x = min_x + x as i32;
            let image_y = min_y + y as i32;
            let idx = ((image_y as usize * width) + image_x as usize) * 4;
            let src_a = source[idx + 3];
            if src_a == 0 {
                continue;
            }

            let should_replace = match gradient.fill_mode {
                Gradient2FillMode::AllOpaque => true,
                Gradient2FillMode::SpecificColor => {
                    source[idx] == gradient.target_color[0]
                        && source[idx + 1] == gradient.target_color[1]
                        && source[idx + 2] == gradient.target_color[2]
                }
            };
            if !should_replace {
                continue;
            }

            let proj = (x as f32 - center_x) * dir_x + (y as f32 - center_y) * dir_y;
            let t = ((proj - min_proj) / proj_range).clamp(0.0, 1.0);
            let inv_t = 1.0 - t;

            let grad_r =
                ((gradient.color1[0] as f32) * inv_t + (gradient.color2[0] as f32) * t).round();
            let grad_g =
                ((gradient.color1[1] as f32) * inv_t + (gradient.color2[1] as f32) * t).round();
            let grad_b =
                ((gradient.color1[2] as f32) * inv_t + (gradient.color2[2] as f32) * t).round();
            let grad_a =
                ((gradient.color1[3] as f32) * inv_t + (gradient.color2[3] as f32) * t).round();
            let mut out_a = grad_a.clamp(0.0, 255.0) as u8;
            if gradient.respect_source_alpha {
                out_a = ((out_a as u16 * src_a as u16) / 255) as u8;
            }

            out[idx] = grad_r.clamp(0.0, 255.0) as u8;
            out[idx + 1] = grad_g.clamp(0.0, 255.0) as u8;
            out[idx + 2] = grad_b.clamp(0.0, 255.0) as u8;
            out[idx + 3] = out_a;
        }
    }

    image.rgba = out;
}

fn apply_gradient4_effect(image: &mut RenderedTextImage, gradient: &Gradient4EffectParams) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let source = image.rgba.clone();
    let mut min_x = width as i32;
    let mut min_y = height as i32;
    let mut max_x = -1i32;
    let mut max_y = -1i32;
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            if source[idx + 3] == 0 {
                continue;
            }
            min_x = min_x.min(x as i32);
            min_y = min_y.min(y as i32);
            max_x = max_x.max(x as i32);
            max_y = max_y.max(y as i32);
        }
    }
    if max_x < min_x || max_y < min_y {
        return;
    }

    let bbox_w = (max_x - min_x + 1) as usize;
    let bbox_h = (max_y - min_y + 1) as usize;
    if bbox_w == 0 || bbox_h == 0 {
        return;
    }

    let mut out = source.clone();
    let denom_x = (bbox_w.saturating_sub(1)).max(1) as f32;
    let denom_y = (bbox_h.saturating_sub(1)).max(1) as f32;

    for y in 0..bbox_h {
        for x in 0..bbox_w {
            let image_x = min_x + x as i32;
            let image_y = min_y + y as i32;
            let idx = ((image_y as usize * width) + image_x as usize) * 4;
            let src_a = source[idx + 3];
            if src_a == 0 {
                continue;
            }

            let should_replace = match gradient.fill_mode {
                Gradient4FillMode::AllOpaque => true,
                Gradient4FillMode::SpecificColor => {
                    source[idx] == gradient.target_color[0]
                        && source[idx + 1] == gradient.target_color[1]
                        && source[idx + 2] == gradient.target_color[2]
                }
            };
            if !should_replace {
                continue;
            }

            let u = if bbox_w > 1 { x as f32 / denom_x } else { 0.0 };
            let v = if bbox_h > 1 { y as f32 / denom_y } else { 0.0 };
            let inv_u = 1.0 - u;
            let inv_v = 1.0 - v;

            let grad_r = ((gradient.color_top_left[0] as f32) * inv_u * inv_v
                + (gradient.color_top_right[0] as f32) * u * inv_v
                + (gradient.color_bottom_left[0] as f32) * inv_u * v
                + (gradient.color_bottom_right[0] as f32) * u * v)
                .round();
            let grad_g = ((gradient.color_top_left[1] as f32) * inv_u * inv_v
                + (gradient.color_top_right[1] as f32) * u * inv_v
                + (gradient.color_bottom_left[1] as f32) * inv_u * v
                + (gradient.color_bottom_right[1] as f32) * u * v)
                .round();
            let grad_b = ((gradient.color_top_left[2] as f32) * inv_u * inv_v
                + (gradient.color_top_right[2] as f32) * u * inv_v
                + (gradient.color_bottom_left[2] as f32) * inv_u * v
                + (gradient.color_bottom_right[2] as f32) * u * v)
                .round();
            let grad_a = ((gradient.color_top_left[3] as f32) * inv_u * inv_v
                + (gradient.color_top_right[3] as f32) * u * inv_v
                + (gradient.color_bottom_left[3] as f32) * inv_u * v
                + (gradient.color_bottom_right[3] as f32) * u * v)
                .round();
            let mut out_a = grad_a.clamp(0.0, 255.0) as u8;
            if gradient.respect_source_alpha {
                out_a = ((out_a as u16 * src_a as u16) / 255) as u8;
            }

            out[idx] = grad_r.clamp(0.0, 255.0) as u8;
            out[idx + 1] = grad_g.clamp(0.0, 255.0) as u8;
            out[idx + 2] = grad_b.clamp(0.0, 255.0) as u8;
            out[idx + 3] = out_a;
        }
    }

    image.rgba = out;
}

fn apply_reflect_effect(image: &mut RenderedTextImage, axis: ReflectAxis) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let source = image.rgba.clone();
    let mut out = vec![0u8; source.len()];

    for y in 0..height {
        for x in 0..width {
            let src_x = match axis {
                ReflectAxis::X => x,
                ReflectAxis::Y => width - 1 - x,
            };
            let src_y = match axis {
                ReflectAxis::X => height - 1 - y,
                ReflectAxis::Y => y,
            };
            let src_idx = (src_y * width + src_x) * 4;
            let dst_idx = (y * width + x) * 4;
            out[dst_idx..dst_idx + 4].copy_from_slice(&source[src_idx..src_idx + 4]);
        }
    }

    image.rgba = out;
}

fn apply_shake_effect(image: &mut RenderedTextImage, shake: &ShakeEffectParams) {
    let source_width = image.width as usize;
    let source_height = image.height as usize;
    if source_width == 0 || source_height == 0 {
        return;
    }
    if shake.steps == 0 || (shake.up_px <= f32::EPSILON && shake.down_px <= f32::EPSILON) {
        return;
    }

    let theta = shake.angle_deg.rem_euclid(360.0).to_radians();
    let unit_x = theta.cos();
    let unit_y = theta.sin();

    let mut offsets = Vec::<(i32, i32)>::new();
    let steps_f = shake.steps as f32;
    let mut add_series = |sign: f32, amount: f32| {
        if amount <= f32::EPSILON {
            return;
        }
        for i in 1..=shake.steps {
            let t = i as f32 / steps_f;
            let dx = (sign * unit_x * (amount * t)).round() as i32;
            let dy = (sign * unit_y * (amount * t)).round() as i32;
            offsets.push((dx, dy));
        }
    };
    add_series(1.0, shake.down_px.max(0.0));
    add_series(-1.0, shake.up_px.max(0.0));

    if offsets.is_empty() {
        return;
    }

    let mut min_dx = 0i32;
    let mut max_dx = 0i32;
    let mut min_dy = 0i32;
    let mut max_dy = 0i32;
    for (dx, dy) in offsets.iter().copied() {
        min_dx = min_dx.min(dx);
        max_dx = max_dx.max(dx);
        min_dy = min_dy.min(dy);
        max_dy = max_dy.max(dy);
    }

    let blur_pad = if shake.blur_px > 0 {
        ((shake.blur_px as f32) * 3.0).ceil() as i32
    } else {
        0
    };
    let extra_pad = blur_pad.saturating_add(shake.grow_margin_px as i32);

    let (left_pad, right_pad, top_pad, bottom_pad) = if shake.autogrow {
        (
            (-min_dx).max(0).saturating_add(extra_pad),
            max_dx.max(0).saturating_add(extra_pad),
            (-min_dy).max(0).saturating_add(extra_pad),
            max_dy.max(0).saturating_add(extra_pad),
        )
    } else {
        (0, 0, 0, 0)
    };

    let source = image.rgba.clone();
    let source_width_u32 = image.width;
    let source_height_u32 = image.height;

    if left_pad > 0 || right_pad > 0 || top_pad > 0 || bottom_pad > 0 {
        let out_width = image
            .width
            .saturating_add(left_pad as u32)
            .saturating_add(right_pad as u32);
        let out_height = image
            .height
            .saturating_add(top_pad as u32)
            .saturating_add(bottom_pad as u32);
        if out_width == 0 || out_height == 0 {
            return;
        }

        let mut base = vec![0u8; out_width as usize * out_height as usize * 4];
        draw_image_with_opacity(
            &mut base,
            out_width as usize,
            out_height as usize,
            source.as_slice(),
            source_width,
            source_height,
            left_pad,
            top_pad,
            1.0,
        );
        image.width = out_width;
        image.height = out_height;
        image.rgba = base;
    }

    let trail_width = image.width as usize;
    let trail_height = image.height as usize;
    if trail_width == 0 || trail_height == 0 {
        return;
    }
    let mut trail = vec![0u8; trail_width * trail_height * 4];

    let opacity_start = (1.0 - shake.base_fade).clamp(0.0, 1.0);
    let step_factor = (1.0 - shake.decay).clamp(0.0, 1.0);

    if shake.down_px > f32::EPSILON {
        for i in 1..=shake.steps {
            let t = i as f32 / steps_f;
            let dx = (unit_x * (shake.down_px * t)).round() as i32;
            let dy = (unit_y * (shake.down_px * t)).round() as i32;
            let opacity = (opacity_start * step_factor.powi((i - 1) as i32)).clamp(0.0, 1.0);
            draw_image_with_opacity(
                &mut trail,
                trail_width,
                trail_height,
                source.as_slice(),
                source_width_u32 as usize,
                source_height_u32 as usize,
                left_pad.saturating_add(dx),
                top_pad.saturating_add(dy),
                opacity,
            );
        }
    }

    if shake.up_px > f32::EPSILON {
        for i in 1..=shake.steps {
            let t = i as f32 / steps_f;
            let dx = (-unit_x * (shake.up_px * t)).round() as i32;
            let dy = (-unit_y * (shake.up_px * t)).round() as i32;
            let opacity = (opacity_start * step_factor.powi((i - 1) as i32)).clamp(0.0, 1.0);
            draw_image_with_opacity(
                &mut trail,
                trail_width,
                trail_height,
                source.as_slice(),
                source_width_u32 as usize,
                source_height_u32 as usize,
                left_pad.saturating_add(dx),
                top_pad.saturating_add(dy),
                opacity,
            );
        }
    }

    if shake.blur_px > 0 {
        gaussian_blur_rgba_in_place(
            &mut trail,
            trail_width as u32,
            trail_height as u32,
            shake.blur_px as f32,
        );
    }

    blend_full_image_over(&mut image.rgba, trail.as_slice());
}

// Image compositing parameters are distinct pixel-buffer properties; grouping would obscure intent.
#[allow(clippy::too_many_arguments)]
fn draw_image_with_opacity(
    dst: &mut [u8],
    dst_width: usize,
    dst_height: usize,
    src: &[u8],
    src_width: usize,
    src_height: usize,
    offset_x: i32,
    offset_y: i32,
    opacity: f32,
) {
    let opacity = opacity.clamp(0.0, 1.0);
    if opacity <= f32::EPSILON
        || dst_width == 0
        || dst_height == 0
        || src_width == 0
        || src_height == 0
    {
        return;
    }

    for sy in 0..src_height {
        let dy = offset_y + sy as i32;
        if dy < 0 || dy >= dst_height as i32 {
            continue;
        }
        for sx in 0..src_width {
            let dx = offset_x + sx as i32;
            if dx < 0 || dx >= dst_width as i32 {
                continue;
            }

            let src_idx = (sy * src_width + sx) * 4;
            let src_a = src[src_idx + 3];
            if src_a == 0 {
                continue;
            }
            let out_a = ((src_a as f32) * opacity).round().clamp(0.0, 255.0) as u8;
            if out_a == 0 {
                continue;
            }

            let dst_idx = (dy as usize * dst_width + dx as usize) * 4;
            blend_pixel_over(
                &mut dst[dst_idx..dst_idx + 4],
                src[src_idx],
                src[src_idx + 1],
                src[src_idx + 2],
                out_a,
            );
        }
    }
}

fn gaussian_blur_rgba_in_place(rgba: &mut Vec<u8>, width: u32, height: u32, sigma: f32) {
    if sigma <= f32::EPSILON || width == 0 || height == 0 {
        return;
    }
    let expected_len = width as usize * height as usize * 4;
    if rgba.len() != expected_len {
        return;
    }
    let Some(src_image) = RgbaImage::from_raw(width, height, rgba.clone()) else {
        return;
    };
    let blurred = image::imageops::blur(&src_image, sigma);
    *rgba = blurred.into_raw();
}

fn blend_full_image_over(dst: &mut [u8], src: &[u8]) {
    let pixel_count = (dst.len() / 4).min(src.len() / 4);
    for idx in 0..pixel_count {
        let base = idx * 4;
        let src_a = src[base + 3];
        if src_a == 0 {
            continue;
        }
        blend_pixel_over(
            &mut dst[base..base + 4],
            src[base],
            src[base + 1],
            src[base + 2],
            src_a,
        );
    }
}

fn apply_shadow_effect(
    image: &mut RenderedTextImage,
    offset_x: i32,
    offset_y: i32,
    transparency_percent: f32,
    blur_radius_px: f32,
    use_source_color: bool,
    color: [u8; 4],
) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let shadow_opacity = (1.0 - transparency_percent.clamp(0.0, 100.0) / 100.0).clamp(0.0, 1.0);
    if shadow_opacity <= f32::EPSILON {
        return;
    }

    let blur_pad = (blur_radius_px.max(0.0) * 3.0).ceil() as u32;
    let left_pad = ((-offset_x).max(0) as u32).saturating_add(blur_pad);
    let right_pad = (offset_x.max(0) as u32).saturating_add(blur_pad);
    let top_pad = ((-offset_y).max(0) as u32).saturating_add(blur_pad);
    let bottom_pad = (offset_y.max(0) as u32).saturating_add(blur_pad);

    let out_width = image
        .width
        .saturating_add(left_pad)
        .saturating_add(right_pad);
    let out_height = image
        .height
        .saturating_add(top_pad)
        .saturating_add(bottom_pad);
    if out_width == 0 || out_height == 0 {
        return;
    }

    let source = image.rgba.clone();
    let mut shadow_layer = vec![0u8; out_width as usize * out_height as usize * 4];
    let mut out = vec![0u8; out_width as usize * out_height as usize * 4];
    let source_origin_x = left_pad as i32;
    let source_origin_y = top_pad as i32;
    let shadow_origin_x = source_origin_x + offset_x;
    let shadow_origin_y = source_origin_y + offset_y;
    let solid_alpha_factor = color[3] as f32 / 255.0;

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }

            let dst_x = shadow_origin_x + x as i32;
            let dst_y = shadow_origin_y + y as i32;
            if dst_x < 0 || dst_y < 0 || dst_x >= out_width as i32 || dst_y >= out_height as i32 {
                continue;
            }

            let (shadow_r, shadow_g, shadow_b, color_alpha_factor) = if use_source_color {
                (
                    source[src_idx],
                    source[src_idx + 1],
                    source[src_idx + 2],
                    1.0,
                )
            } else {
                (color[0], color[1], color[2], solid_alpha_factor)
            };
            let shadow_a = ((src_a as f32) * shadow_opacity * color_alpha_factor)
                .round()
                .clamp(0.0, 255.0) as u8;
            if shadow_a == 0 {
                continue;
            }

            let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
            blend_pixel_over(
                &mut shadow_layer[dst_idx..dst_idx + 4],
                shadow_r,
                shadow_g,
                shadow_b,
                shadow_a,
            );
        }
    }

    if blur_radius_px > f32::EPSILON {
        gaussian_blur_rgba_in_place(&mut shadow_layer, out_width, out_height, blur_radius_px);
    }

    blend_full_image_over(&mut out, shadow_layer.as_slice());

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }
            let dst_x = source_origin_x + x as i32;
            let dst_y = source_origin_y + y as i32;
            if dst_x < 0 || dst_y < 0 || dst_x >= out_width as i32 || dst_y >= out_height as i32 {
                continue;
            }

            let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
            blend_pixel_over(
                &mut out[dst_idx..dst_idx + 4],
                source[src_idx],
                source[src_idx + 1],
                source[src_idx + 2],
                src_a,
            );
        }
    }

    image.width = out_width;
    image.height = out_height;
    image.rgba = out;
}
