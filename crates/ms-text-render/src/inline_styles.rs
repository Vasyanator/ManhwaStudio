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
- color/kerning/stretch/offset/line-spacing пока лишь сохраняются в span-модели для следующих этапов;
- parameterized `<b=...>`/`<i=...>` (and machine `b=`/`i=`) request FAUX
  bold/italic: the span is marked bold/italic but deliberately does NOT get a
  Bold/Italic weight or style in attrs — it keeps the SELECTED face's own
  weight/style (`FauxFaceBaseline`) and the style is synthesized geometrically
  at the glyph seam (`pipeline.rs`, `FauxGlyphStyle`); bare `<b>`/`<i>` keep
  the legacy real-Bold/Italic-face behavior.
*/

use super::font_registry::{InlineFontRegistry, RegisteredFontFace, normalize_inline_font_label};
use super::types::{
    FAUX_THICKEN_PERCENT_MAX, FAUX_THICKEN_PERCENT_MIN, FauxBoldParams, HorizontalAlign,
    PxOrPercent, parse_machine_tag,
};
use cosmic_text::{Attrs, AttrsOwned, Family, Metrics, Style, Weight};

const VERTICAL_HALF_SPACE: char = '\u{200A}';
const SOFT_HYPHEN: char = '\u{00AD}';

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InlineStyleSpan {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    /// Faux-bold parameters when the innermost bold tag is a parameterized
    /// `<b=...>` (or machine `b=...`). `Some` implies `bold == true` and means
    /// the span must KEEP its baseline face — the inline `<font=...>`'s own face
    /// when the span sets one, otherwise the SELECTED face (no `Weight::BOLD` in
    /// attrs) — and be thickened geometrically instead. `None` with
    /// `bold == true` = real bold.
    pub(crate) faux_bold: Option<FauxBoldParams>,
    /// Faux-italic slant (degrees, −45..45, positive = top leans right) when
    /// the innermost italic tag is a parameterized `<i=slant>`. `Some` implies
    /// `italic == true` and means the span keeps its baseline face (the inline
    /// font's own face, else the SELECTED face; no `Style::Italic` in attrs) and
    /// is sheared instead. `None` + `italic` = real italic.
    pub(crate) faux_italic_slant_deg: Option<f32>,
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
            faux_bold: None,
            faux_italic_slant_deg: None,
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
    /// One entry per open bold tag: `None` = real bold (`<b>`/`<strong>`),
    /// `Some` = faux bold with parameters (`<b=...>`). The innermost entry wins.
    bold_stack: Vec<Option<FauxBoldParams>>,
    /// One entry per open italic tag: `None` = real italic (`<i>`/`<em>`),
    /// `Some(slant_deg)` = faux italic (`<i=slant>`). The innermost entry wins.
    italic_stack: Vec<Option<f32>>,
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
            bold: !self.bold_stack.is_empty(),
            italic: !self.italic_stack.is_empty(),
            faux_bold: self.bold_stack.last().copied().flatten(),
            faux_italic_slant_deg: self.italic_stack.last().copied().flatten(),
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
                    // Bare tag = real Bold face (legacy behavior, no faux params).
                    state.bold_stack.push(None);
                    true
                }
                "/b" | "/strong" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.bold_stack.pop();
                    true
                }
                "i" | "em" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    // Bare tag = real Italic face (legacy behavior, no faux slant).
                    state.italic_stack.push(None);
                    true
                }
                "/i" | "/em" => {
                    flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                    state.italic_stack.pop();
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

            if let Some(faux_bold) = parse_faux_bold_tag(raw) {
                flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                state.bold_stack.push(Some(faux_bold));
                i = end + 1;
                continue;
            }
            if let Some(slant_deg) = parse_faux_italic_tag(raw) {
                flush_active_span(&plain_text, &mut spans, &mut span_start, &state);
                state.italic_stack.push(Some(slant_deg));
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

/// The face a FAUX inline span falls back to: the weight/style of the SELECTED
/// registered face.
///
/// Invariant — FAUX NEVER CHANGES FONT MATCHING. A faux `<b=...>`/`<i=...>` span
/// synthesizes its style geometrically, so it must resolve to exactly the same
/// face as the surrounding non-forced text. cosmic-text matches weight EXACTLY
/// and abandons a family when nothing matches, and the pooled `FontSystem`
/// carries the whole bundled `fonts/ui` base beside the selected file
/// (`font_base.rs`), so pinning anything other than the selected face's attrs can
/// silently select one of those fallback fonts. Only a REAL bold/italic request
/// may deviate from this baseline.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FauxFaceBaseline {
    /// Weight of the selected face (`Weight::NORMAL` when it declares none).
    pub(crate) weight: Weight,
    /// Style of the selected face (`Style::Normal` when it declares none).
    pub(crate) style: Style,
}

impl Default for FauxFaceBaseline {
    /// The Regular/upright baseline (weight 400, upright) used when no face
    /// metadata is available.
    fn default() -> Self {
        Self {
            weight: Weight::NORMAL,
            style: Style::Normal,
        }
    }
}

impl FauxFaceBaseline {
    /// Baseline derived from a registered face — the single place mapping face
    /// metadata to the faux fallback attrs.
    ///
    /// `RegisteredFontFace::weight`/`style` are optional; an absent value falls
    /// back to `Weight::NORMAL`/`Style::Normal`, which reproduces the historical
    /// hardcoded reset for a face that declares no weight/style.
    #[must_use]
    pub(crate) fn from_registered_face(face: &RegisteredFontFace) -> Self {
        let default = Self::default();
        Self {
            weight: face.weight.unwrap_or(default.weight),
            style: face.style.unwrap_or(default.style),
        }
    }
}

/// Merges the attrs-compatible part of an inline style span into `attrs`.
///
/// Resolution order is BASELINE FIRST, then the bold/italic decision ON TOP of
/// it:
/// 1. An inline `<font=...>` establishes the span's baseline — its family and
///    stretch always win, and its own `(weight, style)` become the span's
///    effective baseline.
/// 2. Without an inline font the effective baseline is `faux_baseline`, the
///    SELECTED face's own weight/style.
/// 3. `bold` then requests `Weight::BOLD` when it is a REAL bold (bare `<b>`,
///    i.e. `faux_bold.is_none()`) and the effective baseline's weight when it is
///    a faux `<b=...>`; `italic` does the same on the style axis. A span that
///    sets neither keeps the effective baseline as-is.
///
/// So a faux span never changes font matching (it resolves to the inline font's
/// own face, or to the selected face), while a real `<b>`/`<i>` still asks for
/// the Bold/Italic face of whatever family the span ended up on.
pub(crate) fn apply_inline_style_to_attrs<'a>(
    attrs: &Attrs<'a>,
    style: &InlineStyleSpan,
    inline_font_registry: &InlineFontRegistry,
    faux_baseline: FauxFaceBaseline,
) -> AttrsOwned {
    let mut styled_attrs = AttrsOwned::new(attrs);
    // The inline font is resolved FIRST because it establishes the span's
    // BASELINE face; the bold/italic decision below then applies on top of it.
    // Running it last instead (the historical order) unconditionally overwrote
    // `weight`/`style` and silently swallowed a real `<b>`/`<i>` set on the same
    // span — `<b><font=X>..</font></b>` rendered with no bold at all.
    let mut effective_baseline = faux_baseline;
    if let Some(font_label) = style.font_label.as_deref()
        && let Some(font_attrs) = inline_font_registry.get(&normalize_inline_font_label(font_label))
    {
        if let Some(family_name) = font_attrs.family_name.as_deref() {
            styled_attrs.family_owned = cosmic_text::FamilyOwned::new(Family::Name(family_name));
        }
        // An undeclared weight/style on the inline face leaves both the attrs and
        // the baseline untouched, so such a font behaves exactly as before.
        if let Some(font_style) = font_attrs.style {
            styled_attrs.style = font_style;
            effective_baseline.style = font_style;
        }
        if let Some(weight) = font_attrs.weight {
            styled_attrs.weight = weight;
            effective_baseline.weight = weight;
        }
        if let Some(stretch) = font_attrs.stretch {
            styled_attrs.stretch = stretch;
        }
    }
    // Faux spans EXPLICITLY pin the effective baseline's weight/style: the whole
    // point of the parameterized `<b=...>`/`<i=...>` tags is to synthesize the
    // style geometrically at the glyph seam, so they must not change font
    // MATCHING at all. Merely leaving Weight/Style untouched would let a GLOBAL
    // real bold/italic (force flag without faux) leak into the span and stack
    // faux geometry on top of the real Bold/Italic face; hardcoding 400/upright
    // instead would push the span OFF its baseline face whenever the user picked
    // a non-Regular file or an inline font (see `FauxFaceBaseline`).
    if style.bold {
        styled_attrs.weight = if style.faux_bold.is_some() {
            effective_baseline.weight
        } else {
            Weight::BOLD
        };
    }
    if style.italic {
        styled_attrs.style = if style.faux_italic_slant_deg.is_some() {
            effective_baseline.style
        } else {
            Style::Italic
        };
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
        && last.faux_bold == style.faux_bold
        && last.faux_italic_slant_deg == style.faux_italic_slant_deg
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

/// Parse a parameterized bold tag `<b=...>` / `<strong=...>` (faux bold).
///
/// Returns `None` for any other tag AND for an unreadable value — the tag is
/// then not recognized and, per the parser's convention, ends up in the plain
/// text literally.
fn parse_faux_bold_tag(raw_tag: &str) -> Option<FauxBoldParams> {
    let value = tag_value(raw_tag, "b").or_else(|| tag_value(raw_tag, "strong"))?;
    parse_faux_bold_value(value)
}

/// Parse a parameterized italic tag `<i=slant_deg>` / `<em=...>` (faux italic).
///
/// Returns the slant in degrees (clamped to -45..45). `None` for another tag
/// or an unreadable value (the tag then ends up in the text literally).
fn parse_faux_italic_tag(raw_tag: &str) -> Option<f32> {
    let value = tag_value(raw_tag, "i").or_else(|| tag_value(raw_tag, "em"))?;
    parse_faux_italic_value(value)
}

/// Faux-bold value grammar: `thicken[,sharp|round][,out|both][,expand]`.
///
/// - the first token is the `thicken_percent` number, clamped to `-5..25`
///   (% of font size); a NEGATIVE value THINS the glyphs instead of thickening
///   them, `0` leaves them unchanged;
/// - the remaining tokens come in any order: `sharp`/`round` (join style),
///   `out`/`both` (counters), and at most ONE extra number — `expand_percent`
///   (0..50, extra letter-spacing);
/// - `default` (or an empty value) = the default parameters
///   (thicken 3, expand 0, sharp, both — see `FauxBoldParams::default`).
///
/// Returns `None` for an unreadable value (unknown token, a second stray
/// number, or a non-numeric thicken).
///
/// Note that a negative EXPAND token is still clamped to `0`: only the leading
/// thicken number carries a sign.
///
/// COMPATIBILITY EXCEPTION — an inline tag carries NO persistence key, so an
/// omitted `out`/`both` token means today's `FauxBoldParams::default()`, and
/// that default flipped from `out` (counters preserved) to `both` (uniform
/// weight). Saved DOCUMENTS are unaffected: `text_params` stores
/// `faux_bold_outward_only` explicitly and its frozen schema-2 default stays
/// `true`. A saved TEXT is not: a hand-typed `<b=8>` or `<b=default>` written
/// before the flip now renders uniform instead of counter-preserving. The
/// exposure is limited to hand-typed tags — the typing panel always emits the
/// token explicitly (`src/tabs/typing/panel/inline_tags.rs`). No compatibility
/// shim exists, deliberately: a tag has no version to key one off, and guessing
/// per tag would make two identical tags in one document mean different things.
pub(crate) fn parse_faux_bold_value(value: &str) -> Option<FauxBoldParams> {
    let trimmed = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
        .trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("default") {
        return Some(FauxBoldParams::default());
    }
    let mut tokens = trimmed.split(',').map(str::trim);
    let thicken = tokens.next()?.parse::<f32>().ok()?;
    if !thicken.is_finite() {
        return None;
    }
    let mut params = FauxBoldParams {
        thicken_percent: thicken.clamp(FAUX_THICKEN_PERCENT_MIN, FAUX_THICKEN_PERCENT_MAX),
        ..FauxBoldParams::default()
    };
    let mut has_expand = false;
    for token in tokens {
        match token.to_ascii_lowercase().as_str() {
            "sharp" => params.sharp_corners = true,
            "round" => params.sharp_corners = false,
            "out" => params.outward_only = true,
            "both" => params.outward_only = false,
            other => {
                // Exactly one extra number is allowed: expand_percent.
                let expand = other.parse::<f32>().ok()?;
                if !expand.is_finite() || has_expand {
                    return None;
                }
                params.expand_percent = expand.clamp(0.0, 50.0);
                has_expand = true;
            }
        }
    }
    Some(params)
}

/// Faux-italic value: a single number — the slant in degrees, clamped to
/// -45..45. `None` for an unreadable/non-finite value.
pub(crate) fn parse_faux_italic_value(value: &str) -> Option<f32> {
    let trimmed = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
        .trim();
    let slant = trimmed.parse::<f32>().ok()?;
    slant.is_finite().then(|| slant.clamp(-45.0, 45.0))
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
                // Optional value payload = faux bold (same grammar as `<b=...>`).
                // A valueless `b` (and, defensively, an unreadable value) keeps
                // the legacy real-Bold-face flag semantics.
                let faux = if value.is_empty() {
                    None
                } else {
                    parse_faux_bold_value(value)
                };
                state.bold_stack.push(faux);
                frame.bold = true;
            }
            'i' => {
                // Optional value payload = faux italic slant in degrees; a
                // valueless `i` (or unreadable value) = real Italic face.
                let faux = if value.is_empty() {
                    None
                } else {
                    parse_faux_italic_value(value)
                };
                state.italic_stack.push(faux);
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
        state.bold_stack.pop();
    }
    if frame.italic {
        state.italic_stack.pop();
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
        FauxFaceBaseline, InlineGlyphOffset, InlineStyleSpan, apply_inline_style_to_attrs,
        parse_inline_style_tags, remap_inline_style_spans,
    };
    use crate::font_registry::{InlineFontRegistry, RegisteredFontFace};
    use crate::types::{FauxBoldParams, HorizontalAlign};
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
            faux_bold: None,
            faux_italic_slant_deg: None,
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
                faux_bold: None,
                faux_italic_slant_deg: None,
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
                faux_bold: None,
                faux_italic_slant_deg: None,
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
            faux_bold: None,
            faux_italic_slant_deg: None,
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
            faux_bold: None,
            faux_italic_slant_deg: None,
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
    fn bare_bold_italic_tags_stay_real_face() {
        // Legacy `<b>`/`<i>` must keep the pre-faux semantics: bold/italic flags
        // set, NO faux parameters attached.
        let parsed = parse_inline_style_tags("a<b><i>bc</i></b>d", 24.0);
        assert_eq!(parsed.plain_text, "abcd");
        let span = &parsed.spans[1];
        assert!(span.bold && span.italic);
        assert_eq!(span.faux_bold, None);
        assert_eq!(span.faux_italic_slant_deg, None);
    }

    #[test]
    fn faux_bold_tag_parses_thicken_only() {
        let parsed = parse_inline_style_tags("a<b=3>bc</b>d", 24.0);
        assert_eq!(parsed.plain_text, "abcd");
        let span = &parsed.spans[1];
        assert!(span.bold, "faux bold still marks the span bold");
        assert_eq!(
            span.faux_bold,
            Some(FauxBoldParams {
                thicken_percent: 3.0,
                expand_percent: 0.0,
                sharp_corners: true,
                // Unspecified tokens come from `FauxBoldParams::default`, whose
                // counter mode is the uniform-weight `both`.
                outward_only: false,
            })
        );
        assert_eq!(parsed.spans[2].faux_bold, None);

        // A NEGATIVE thicken is a first-class value (glyph thinning), not a
        // clamp-to-zero typo, and it survives the whole grammar.
        let thinned = parse_inline_style_tags("a<b=-4,out,round>bc</b>d", 24.0);
        assert_eq!(
            thinned.spans[1].faux_bold,
            Some(FauxBoldParams {
                thicken_percent: -4.0,
                expand_percent: 0.0,
                sharp_corners: false,
                outward_only: true,
            })
        );
        // Out-of-range values clamp to the documented -5..=25 bounds.
        let clamped_low = parse_inline_style_tags("a<b=-50>bc</b>d", 24.0);
        assert_eq!(
            clamped_low.spans[1].faux_bold.map(|faux| faux.thicken_percent),
            Some(-5.0)
        );
        let clamped_high = parse_inline_style_tags("a<b=500>bc</b>d", 24.0);
        assert_eq!(
            clamped_high.spans[1].faux_bold.map(|faux| faux.thicken_percent),
            Some(25.0)
        );
    }

    #[test]
    fn faux_bold_tag_parses_full_token_list_in_any_order() {
        let parsed = parse_inline_style_tags("a<b=3,round,both,1.5>bc</b>d", 24.0);
        let span = &parsed.spans[1];
        assert_eq!(
            span.faux_bold,
            Some(FauxBoldParams {
                thicken_percent: 3.0,
                expand_percent: 1.5,
                sharp_corners: false,
                outward_only: false,
            })
        );
        // Token order after the leading thicken number is free.
        let reordered = parse_inline_style_tags("a<b=3,1.5,both,round>bc</b>d", 24.0);
        assert_eq!(reordered.spans[1].faux_bold, span.faux_bold);
    }

    #[test]
    fn faux_bold_default_keyword_uses_defaults() {
        let parsed = parse_inline_style_tags("a<b=default>bc</b>d", 24.0);
        assert_eq!(parsed.spans[1].faux_bold, Some(FauxBoldParams::default()));
        assert!(parsed.spans[1].bold);
    }

    #[test]
    fn faux_italic_tag_parses_signed_slant() {
        let parsed = parse_inline_style_tags("a<i=-14>bc</i>d", 24.0);
        let span = &parsed.spans[1];
        assert!(span.italic, "faux italic still marks the span italic");
        assert_eq!(span.faux_italic_slant_deg, Some(-14.0));
        // Slant clamps to the documented -45..45 range.
        let clamped = parse_inline_style_tags("a<i=90>bc</i>d", 24.0);
        assert_eq!(clamped.spans[1].faux_italic_slant_deg, Some(45.0));
    }

    #[test]
    fn unreadable_faux_values_fall_back_to_literal_text() {
        // The file's convention for unrecognized tags: they are not consumed and
        // appear literally in the plain text.
        let parsed = parse_inline_style_tags("a<b=abc>b", 24.0);
        assert_eq!(parsed.plain_text, "a<b=abc>b");
        let parsed = parse_inline_style_tags("a<b=3,round,1,2>b", 24.0);
        assert_eq!(
            parsed.plain_text, "a<b=3,round,1,2>b",
            "a second bare number is invalid"
        );
        let parsed = parse_inline_style_tags("a<i=wide>b", 24.0);
        assert_eq!(parsed.plain_text, "a<i=wide>b");
    }

    #[test]
    fn machine_tag_b_i_value_payload_round_trip() {
        // Machine keys accept the same value grammar; valueless keys keep the
        // legacy real-face flags.
        let parsed = parse_inline_style_tags("a<m b=5,round,both,2 i=-10>bc</m>d", 24.0);
        let span = &parsed.spans[1];
        assert!(span.bold && span.italic);
        assert_eq!(
            span.faux_bold,
            Some(FauxBoldParams {
                thicken_percent: 5.0,
                expand_percent: 2.0,
                sharp_corners: false,
                outward_only: false,
            })
        );
        assert_eq!(span.faux_italic_slant_deg, Some(-10.0));
        // Closed cleanly by </m>.
        assert!(!parsed.spans[2].bold);
        assert_eq!(parsed.spans[2].faux_bold, None);

        let legacy = parse_inline_style_tags("a<m b i>bc</m>d", 24.0);
        let legacy_span = &legacy.spans[1];
        assert!(legacy_span.bold && legacy_span.italic);
        assert_eq!(legacy_span.faux_bold, None);
        assert_eq!(legacy_span.faux_italic_slant_deg, None);
    }

    /// The faux baseline of a plain Regular file (weight 400, upright) — the
    /// overwhelmingly common selected face, and the historical hardcoded reset.
    fn regular_baseline() -> FauxFaceBaseline {
        FauxFaceBaseline::default()
    }

    /// A registered face carrying an explicit weight/style, as a non-Regular
    /// file of a family resolves to.
    fn registered_face(family: &str, weight: Weight, style: Style) -> RegisteredFontFace {
        RegisteredFontFace {
            family_name: Some(family.to_string()),
            style: Some(style),
            weight: Some(weight),
            stretch: None,
        }
    }

    #[test]
    fn faux_spans_do_not_set_weight_or_style_on_attrs() {
        let attrs = Attrs::new().metrics(Metrics::new(20.0, 24.0));
        let parsed = parse_inline_style_tags("<b=3><i=12>x</i></b>", 24.0);
        let span = parsed
            .spans
            .iter()
            .find(|span| span.faux_bold.is_some())
            .expect("faux span");
        let applied = apply_inline_style_to_attrs(
            &attrs,
            span,
            &InlineFontRegistry::default(),
            regular_baseline(),
        );
        assert_ne!(applied.weight, Weight::BOLD, "faux bold keeps Regular face");
        assert_ne!(applied.style, Style::Italic, "faux italic keeps upright face");
    }

    #[test]
    fn faux_spans_reset_global_real_bold_italic_to_selected_face() {
        // Base attrs carrying a GLOBAL real bold + italic (force flags without
        // faux): an inline faux span must explicitly reset to the SELECTED
        // face's own weight/style, never stack faux geometry on the real faces.
        // With a Regular selected face the reset is the historical
        // 400/upright — byte-identical to the pre-fix behavior.
        let attrs = Attrs::new()
            .metrics(Metrics::new(20.0, 24.0))
            .weight(Weight::BOLD)
            .style(Style::Italic);

        let parsed = parse_inline_style_tags("<b=3>x</b>", 24.0);
        let bold_span = parsed
            .spans
            .iter()
            .find(|span| span.faux_bold.is_some())
            .expect("faux bold span");
        let applied = apply_inline_style_to_attrs(
            &attrs,
            bold_span,
            &InlineFontRegistry::default(),
            regular_baseline(),
        );
        assert_eq!(
            applied.weight,
            Weight::NORMAL,
            "faux bold span must fall back to the Regular selected face over a global real bold"
        );
        // The italic flag is not set on this span, so the global italic stays.
        assert_eq!(applied.style, Style::Italic);

        let parsed = parse_inline_style_tags("<i=14>x</i>", 24.0);
        let italic_span = parsed
            .spans
            .iter()
            .find(|span| span.faux_italic_slant_deg.is_some())
            .expect("faux italic span");
        let applied = apply_inline_style_to_attrs(
            &attrs,
            italic_span,
            &InlineFontRegistry::default(),
            regular_baseline(),
        );
        assert_eq!(
            applied.style,
            Style::Normal,
            "faux italic span must fall back to the upright selected face over a global real italic"
        );
        assert_eq!(applied.weight, Weight::BOLD);
    }

    #[test]
    fn faux_bold_span_keeps_the_selected_faces_own_weight() {
        // Regression: the user selected a Bold FILE of a family (a separate
        // entry in the font list, weight 700). A faux `<b=...>` span inside it
        // must keep 700 — hardcoding 400 would push the span off the selected
        // face, and cosmic-text matches weight EXACTLY, so it could silently
        // resolve to a different file of the render font base.
        let attrs = Attrs::new()
            .metrics(Metrics::new(20.0, 24.0))
            .weight(Weight(700));
        let baseline = FauxFaceBaseline::from_registered_face(&registered_face(
            "Ms Faux Test Family",
            Weight(700),
            Style::Normal,
        ));

        let parsed = parse_inline_style_tags("<b=3>x</b>", 24.0);
        let bold_span = parsed
            .spans
            .iter()
            .find(|span| span.faux_bold.is_some())
            .expect("faux bold span");
        let applied =
            apply_inline_style_to_attrs(&attrs, bold_span, &InlineFontRegistry::default(), baseline);

        assert_eq!(
            applied.weight,
            Weight(700),
            "a faux bold span must keep the selected face's own weight, not drop to 400"
        );
    }

    #[test]
    fn faux_italic_span_keeps_the_selected_faces_own_style() {
        // Same defect on the style axis: an Italic FILE selected as the face
        // must stay Italic under a faux `<i=...>` span.
        let attrs = Attrs::new()
            .metrics(Metrics::new(20.0, 24.0))
            .style(Style::Italic);
        let baseline = FauxFaceBaseline::from_registered_face(&registered_face(
            "Ms Faux Test Family",
            Weight::NORMAL,
            Style::Italic,
        ));

        let parsed = parse_inline_style_tags("<i=14>x</i>", 24.0);
        let italic_span = parsed
            .spans
            .iter()
            .find(|span| span.faux_italic_slant_deg.is_some())
            .expect("faux italic span");
        let applied = apply_inline_style_to_attrs(
            &attrs,
            italic_span,
            &InlineFontRegistry::default(),
            baseline,
        );

        assert_eq!(
            applied.style,
            Style::Italic,
            "a faux italic span must keep the selected face's own style, not force upright"
        );
    }

    #[test]
    fn bare_bold_span_still_requests_the_real_bold_face() {
        // The real-face path is unchanged: a bare `<b>` asks for Weight::BOLD
        // whatever the selected face's own weight is.
        let attrs = Attrs::new().metrics(Metrics::new(20.0, 24.0));
        for baseline_weight in [Weight::NORMAL, Weight(300), Weight(700)] {
            let baseline = FauxFaceBaseline::from_registered_face(&registered_face(
                "Ms Faux Test Family",
                baseline_weight,
                Style::Normal,
            ));
            let parsed = parse_inline_style_tags("<b>x</b>", 24.0);
            let bold_span = parsed
                .spans
                .iter()
                .find(|span| span.bold)
                .expect("real bold span");
            assert_eq!(bold_span.faux_bold, None, "bare <b> must stay a real bold");
            let applied = apply_inline_style_to_attrs(
                &attrs,
                bold_span,
                &InlineFontRegistry::default(),
                baseline,
            );
            assert_eq!(
                applied.weight,
                Weight::BOLD,
                "a bare <b> must request the real Bold face regardless of the baseline"
            );
        }
    }

    #[test]
    fn faux_span_with_inline_font_takes_that_fonts_face() {
        // Ordering contract: the inline `<font=...>` is resolved FIRST and
        // becomes the span's effective baseline, so a faux `<b=...>` on the same
        // span pins the INLINE font's own weight/style, not the selected face's.
        let attrs = Attrs::new().metrics(Metrics::new(20.0, 24.0));
        let baseline = FauxFaceBaseline::from_registered_face(&registered_face(
            "Ms Selected Family",
            Weight(700),
            Style::Normal,
        ));
        let mut registry = InlineFontRegistry::default();
        registry.insert(
            "inline".to_string(),
            registered_face("Ms Inline Family", Weight(300), Style::Italic),
        );

        let parsed = parse_inline_style_tags("<b=3><font=inline>x</font></b>", 24.0);
        let span = parsed
            .spans
            .iter()
            .find(|span| span.faux_bold.is_some() && span.font_label.is_some())
            .expect("faux span with an inline font");
        let applied = apply_inline_style_to_attrs(&attrs, span, &registry, baseline);

        assert_eq!(
            applied.weight,
            Weight(300),
            "an inline font's own weight must win over the selected face baseline"
        );
        assert_eq!(
            applied.style,
            Style::Italic,
            "an inline font's own style must win over the selected face baseline"
        );
    }

    /// Registry holding one inline font under the label `inline`, with an
    /// explicitly declared non-default weight/style so a pass-through of the
    /// baseline is distinguishable from a Bold/Italic request.
    fn inline_font_registry_with(weight: Weight, style: Style) -> InlineFontRegistry {
        let mut registry = InlineFontRegistry::default();
        registry.insert(
            "inline".to_string(),
            registered_face("Ms Inline Family", weight, style),
        );
        registry
    }

    /// The inline family the fixture registry registers, as it lands in attrs.
    fn inline_family() -> cosmic_text::FamilyOwned {
        cosmic_text::FamilyOwned::new(cosmic_text::Family::Name("Ms Inline Family"))
    }

    #[test]
    fn inline_font_with_bare_bold_still_requests_the_real_bold_face() {
        // Regression: the inline `<font=...>` used to be applied LAST and
        // overwrote the `Weight::BOLD` a bare `<b>` had just set, so
        // `<b><font=X>..</font></b>` rendered with no bold at all.
        let attrs = Attrs::new().metrics(Metrics::new(20.0, 24.0));
        let baseline = FauxFaceBaseline::from_registered_face(&registered_face(
            "Ms Selected Family",
            Weight::NORMAL,
            Style::Normal,
        ));
        let registry = inline_font_registry_with(Weight(300), Style::Normal);

        let parsed = parse_inline_style_tags("<b><font=inline>x</font></b>", 24.0);
        let span = parsed
            .spans
            .iter()
            .find(|span| span.bold && span.font_label.is_some())
            .expect("real bold span with an inline font");
        assert_eq!(span.faux_bold, None, "bare <b> must stay a real bold");
        let applied = apply_inline_style_to_attrs(&attrs, span, &registry, baseline);

        assert_eq!(
            applied.weight,
            Weight::BOLD,
            "a real <b> must survive an inline font on the same span"
        );
        assert_eq!(
            applied.family_owned,
            inline_family(),
            "the inline font still selects its own family"
        );
    }

    #[test]
    fn inline_font_with_bare_italic_still_requests_the_real_italic_face() {
        // Same defect on the style axis.
        let attrs = Attrs::new().metrics(Metrics::new(20.0, 24.0));
        let baseline = FauxFaceBaseline::from_registered_face(&registered_face(
            "Ms Selected Family",
            Weight::NORMAL,
            Style::Normal,
        ));
        let registry = inline_font_registry_with(Weight(300), Style::Normal);

        let parsed = parse_inline_style_tags("<i><font=inline>x</font></i>", 24.0);
        let span = parsed
            .spans
            .iter()
            .find(|span| span.italic && span.font_label.is_some())
            .expect("real italic span with an inline font");
        assert_eq!(
            span.faux_italic_slant_deg, None,
            "bare <i> must stay a real italic"
        );
        let applied = apply_inline_style_to_attrs(&attrs, span, &registry, baseline);

        assert_eq!(
            applied.style,
            Style::Italic,
            "a real <i> must survive an inline font on the same span"
        );
        assert_eq!(
            applied.family_owned,
            inline_family(),
            "the inline font still selects its own family"
        );
    }

    #[test]
    fn inline_font_with_faux_bold_keeps_that_fonts_own_weight() {
        // Guard for the other direction: a FAUX `<b=...>` must NOT request the
        // Bold face — it pins the inline font's own weight so the geometric
        // thickening is applied to exactly the face the span already matched.
        let attrs = Attrs::new().metrics(Metrics::new(20.0, 24.0));
        let baseline = FauxFaceBaseline::from_registered_face(&registered_face(
            "Ms Selected Family",
            Weight(700),
            Style::Normal,
        ));
        let registry = inline_font_registry_with(Weight(300), Style::Normal);

        let parsed = parse_inline_style_tags("<b=30><font=inline>x</font></b>", 24.0);
        let span = parsed
            .spans
            .iter()
            .find(|span| span.faux_bold.is_some() && span.font_label.is_some())
            .expect("faux bold span with an inline font");
        let applied = apply_inline_style_to_attrs(&attrs, span, &registry, baseline);

        assert_eq!(
            applied.weight,
            Weight(300),
            "a faux bold must keep the inline font's own weight, not request Bold"
        );
        assert_eq!(applied.family_owned, inline_family());
    }

    #[test]
    fn inline_font_alone_takes_that_fonts_weight_and_style() {
        // A span that sets neither bold nor italic keeps the effective baseline,
        // i.e. the inline font's own face — unchanged behavior.
        let attrs = Attrs::new()
            .metrics(Metrics::new(20.0, 24.0))
            .weight(Weight::BOLD)
            .style(Style::Italic);
        let baseline = FauxFaceBaseline::from_registered_face(&registered_face(
            "Ms Selected Family",
            Weight(700),
            Style::Italic,
        ));
        let registry = inline_font_registry_with(Weight(300), Style::Normal);

        let parsed = parse_inline_style_tags("<font=inline>x</font>", 24.0);
        let span = parsed
            .spans
            .iter()
            .find(|span| span.font_label.is_some())
            .expect("inline font span");
        assert!(!span.bold && !span.italic, "the span sets no bold/italic");
        let applied = apply_inline_style_to_attrs(&attrs, span, &registry, baseline);

        assert_eq!(applied.weight, Weight(300));
        assert_eq!(applied.style, Style::Normal);
        assert_eq!(applied.family_owned, inline_family());
    }

    #[test]
    fn faux_face_baseline_falls_back_to_regular_without_face_metadata() {
        // A face that declares no weight/style reproduces the historical
        // hardcoded 400/upright reset.
        let face = RegisteredFontFace {
            family_name: Some("Ms Faux Test Family".to_string()),
            style: None,
            weight: None,
            stretch: None,
        };
        let baseline = FauxFaceBaseline::from_registered_face(&face);
        assert_eq!(baseline.weight, Weight::NORMAL);
        assert_eq!(baseline.style, Style::Normal);
    }

    #[test]
    fn apply_inline_style_to_attrs_updates_weight_style_and_metrics() {
        let attrs = Attrs::new().metrics(Metrics::new(20.0, 24.0));
        let style = InlineStyleSpan {
            start: 0,
            end: 3,
            bold: true,
            italic: true,
            faux_bold: None,
            faux_italic_slant_deg: None,
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

        let applied = apply_inline_style_to_attrs(
            &attrs,
            &style,
            &InlineFontRegistry::default(),
            regular_baseline(),
        );

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
