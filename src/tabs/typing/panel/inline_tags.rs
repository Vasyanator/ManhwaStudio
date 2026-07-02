/*
File: panel/inline_tags.rs

Purpose:
Free-function helpers extracted verbatim from panel.rs for the inline-tag
tooling of the create/edit text panel.

Main responsibilities:
- build and parse the machine `<m ...>` tag plus the opening/closing inline tags;
- convert inline offset, stretch, color, and alignment tokens to and from styles;
- color the inline-tag editor's text spans for the tag-aware text editor.

Notes:
Free fns are `pub(super)` so `panel.rs` and sibling submodules can use them
unqualified. `use super::*;` pulls in the parent module's types and imports.
*/

use super::*;

pub(super) fn build_inline_tag_editor_text_colors(text: &str) -> Vec<TextEditPlusTextColor> {
    let mut content_styles = Vec::new();
    let mut tag_styles = Vec::new();
    let mut stack = Vec::<TypingInlineTagToken>::new();
    let mut cursor = 0usize;

    while cursor < text.len() {
        let Some(relative_start) = text[cursor..].find('<') else {
            break;
        };
        let tag_start = cursor + relative_start;
        let Some(relative_end) = text[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + relative_end + 1;
        let raw = &text[tag_start + 1..tag_end - 1];

        if let Some(kind) = parse_opening_inline_tag(raw) {
            push_editor_text_color(
                text,
                tag_start..tag_end,
                INLINE_TAG_DIM_TEXT_COLOR,
                &mut tag_styles,
            );
            stack.push(TypingInlineTagToken {
                byte_range: tag_start..tag_end,
                kind,
            });
        } else if let Some(kind) = parse_closing_inline_tag(raw) {
            push_editor_text_color(
                text,
                tag_start..tag_end,
                INLINE_TAG_DIM_TEXT_COLOR,
                &mut tag_styles,
            );
            if let Some(open_idx) = stack
                .iter()
                .rposition(|open_tag| inline_tag_kinds_match(&open_tag.kind, &kind))
            {
                let open_tag = stack.remove(open_idx);
                push_editor_text_color(
                    text,
                    open_tag.byte_range.end..tag_start,
                    INLINE_TAG_CONTENT_TEXT_COLOR,
                    &mut content_styles,
                );
            }
        }

        cursor = tag_end;
    }

    let mut styles = content_styles;
    styles.extend(tag_styles);
    styles
}

pub(super) fn push_editor_text_color(
    text: &str,
    byte_range: Range<usize>,
    color: Color32,
    out: &mut Vec<TextEditPlusTextColor>,
) {
    if byte_range.is_empty() {
        return;
    }
    let Some(char_start) = byte_index_to_char_index(text, byte_range.start) else {
        return;
    };
    let Some(char_end) = byte_index_to_char_index(text, byte_range.end) else {
        return;
    };
    if char_start < char_end {
        out.push(TextEditPlusTextColor::new(char_start..char_end, color));
    }
}

pub(super) fn collect_adjacent_opening_inline_tags(
    text: &str,
    selection_start: usize,
) -> Vec<TypingInlineTagToken> {
    let mut out = Vec::new();
    let mut cursor = selection_start;
    while cursor > 0 {
        let Some(raw_start) = text[..cursor].rfind('<') else {
            break;
        };
        if !text[raw_start..cursor].ends_with('>') {
            break;
        }
        let raw = &text[raw_start + 1..cursor - 1];
        let Some(kind) = parse_opening_inline_tag(raw) else {
            break;
        };
        out.push(TypingInlineTagToken {
            byte_range: raw_start..cursor,
            kind,
        });
        cursor = raw_start;
    }
    out
}

pub(super) fn collect_adjacent_closing_inline_tags(
    text: &str,
    selection_end: usize,
) -> Vec<TypingInlineTagToken> {
    let mut out = Vec::new();
    let mut cursor = selection_end;
    while cursor < text.len() {
        let rest = &text[cursor..];
        if !rest.starts_with('<') {
            break;
        }
        let Some(rel_end) = rest.find('>') else {
            break;
        };
        let tag_end = cursor + rel_end + 1;
        let raw = &text[cursor + 1..tag_end - 1];
        let Some(kind) = parse_closing_inline_tag(raw) else {
            break;
        };
        out.push(TypingInlineTagToken {
            byte_range: cursor..tag_end,
            kind,
        });
        cursor = tag_end;
    }
    out
}

pub(super) fn parse_opening_inline_tag(raw: &str) -> Option<TypingInlineTagKind> {
    let compact = raw
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    match compact.as_str() {
        "b" | "strong" => return Some(TypingInlineTagKind::Bold),
        "i" | "em" => return Some(TypingInlineTagKind::Italic),
        "no-break" | "nobreak" | "nobr" => return Some(TypingInlineTagKind::NoBreak),
        _ => {}
    }

    if let Some(style) = parse_machine_tag_style(raw) {
        return Some(TypingInlineTagKind::Machine(style));
    }

    if let Some(align) = parse_inline_align_tag(raw) {
        return Some(TypingInlineTagKind::Align(align));
    }

    if let Some((tag_name, value)) = raw.split_once('=')
        && tag_name.trim().eq_ignore_ascii_case("font")
    {
        let label = value
            .trim()
            .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
            .trim();
        if !label.is_empty() {
            return Some(TypingInlineTagKind::Font(label.to_string()));
        }
    }

    if let Some((tag_name, value)) = raw.split_once('=')
        && tag_name.trim().eq_ignore_ascii_case("size")
    {
        let value = value
            .trim()
            .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
            .trim()
            .strip_suffix("px")
            .unwrap_or(value)
            .trim();
        if let Ok(parsed) = value.parse::<f32>()
            && parsed.is_finite()
            && parsed > 0.0
        {
            return Some(TypingInlineTagKind::Size(parsed));
        }
    }

    if let Some((tag_name, value)) = raw.split_once('=')
        && tag_name.trim().eq_ignore_ascii_case("color")
        && let Some(color) = parse_inline_hex_color(value)
    {
        return Some(TypingInlineTagKind::Color(color));
    }

    if let Some(value) = parse_inline_value_or_legacy_pair(raw, "line-spacing", 300.0) {
        return Some(TypingInlineTagKind::LineSpacing(value));
    }

    if let Some(value) = parse_inline_value_or_legacy_pair(raw, "kerning", 300.0) {
        return Some(TypingInlineTagKind::Kerning(value));
    }

    if let Some(value) = parse_inline_stretch_value(raw) {
        return Some(TypingInlineTagKind::Stretching(value));
    }

    if let Some(offset) = parse_inline_offset_value(raw) {
        return Some(TypingInlineTagKind::Offset(offset));
    }

    None
}

pub(super) fn parse_closing_inline_tag(raw: &str) -> Option<TypingInlineTagKind> {
    let compact = raw
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    match compact.as_str() {
        "/b" | "/strong" => Some(TypingInlineTagKind::Bold),
        "/i" | "/em" => Some(TypingInlineTagKind::Italic),
        "/no-break" | "/nobreak" | "/nobr" => Some(TypingInlineTagKind::NoBreak),
        "/align" => Some(TypingInlineTagKind::Align(HorizontalAlign::CENTER)),
        "/font" => Some(TypingInlineTagKind::Font(String::new())),
        "/size" => Some(TypingInlineTagKind::Size(0.0)),
        "/color" => Some(TypingInlineTagKind::Color(Color32::TRANSPARENT)),
        "/line-spacing" => Some(TypingInlineTagKind::LineSpacing(PxOrPercent::percent(0.0))),
        "/kerning" => Some(TypingInlineTagKind::Kerning(PxOrPercent::percent(0.0))),
        "/stretching" => Some(TypingInlineTagKind::Stretching([
            PxOrPercent::percent(100.0),
            PxOrPercent::percent(100.0),
        ])),
        "/offset" => Some(TypingInlineTagKind::Offset(
            TypingInlineOffsetStyle::global_only([0.0, 0.0]),
        )),
        "/m" => Some(TypingInlineTagKind::Machine(TypingInlineTagStyle::default())),
        _ => None,
    }
}

pub(super) fn inline_tag_kinds_match(left: &TypingInlineTagKind, right: &TypingInlineTagKind) -> bool {
    matches!(
        (left, right),
        (TypingInlineTagKind::Bold, TypingInlineTagKind::Bold)
            | (TypingInlineTagKind::Italic, TypingInlineTagKind::Italic)
            | (TypingInlineTagKind::NoBreak, TypingInlineTagKind::NoBreak)
            | (TypingInlineTagKind::Align(_), TypingInlineTagKind::Align(_))
            | (TypingInlineTagKind::Font(_), TypingInlineTagKind::Font(_))
            | (TypingInlineTagKind::Size(_), TypingInlineTagKind::Size(_))
            | (TypingInlineTagKind::Color(_), TypingInlineTagKind::Color(_))
            | (
                TypingInlineTagKind::LineSpacing(_),
                TypingInlineTagKind::LineSpacing(_)
            )
            | (
                TypingInlineTagKind::Kerning(_),
                TypingInlineTagKind::Kerning(_)
            )
            | (
                TypingInlineTagKind::Stretching(_),
                TypingInlineTagKind::Stretching(_)
            )
            | (
                TypingInlineTagKind::Offset(_),
                TypingInlineTagKind::Offset(_)
            )
            | (
                TypingInlineTagKind::Machine(_),
                TypingInlineTagKind::Machine(_)
            )
    )
}

pub(super) fn parse_inline_align_tag(raw: &str) -> Option<HorizontalAlign> {
    let value = inline_tag_value(raw, "align")?;
    parse_inline_align_value(value)
}

pub(super) fn parse_inline_align_value(value: &str) -> Option<HorizontalAlign> {
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

pub(super) fn format_inline_align_value(align: HorizontalAlign) -> String {
    if align.justify || align.bias <= -0.95 || align.bias.abs() <= 0.05 || align.bias >= 0.95 {
        align.legacy_str().to_string()
    } else {
        format!("{:.2}", align.bias.clamp(-1.0, 1.0))
    }
}

/// Собрать машиночитаемый тег `<m ...>` (см. контракт ключей в `parse_machine_tag`).
/// Возвращает пустую строку, если стиль ничего не задаёт.
pub(super) fn build_inline_machine_tag(style: &TypingInlineTagStyle) -> String {
    let mut out = String::from("<m");
    if style.bold {
        out.push_str(" b");
    }
    if style.italic {
        out.push_str(" i");
    }
    if style.no_break {
        out.push_str(" j");
    }
    if let Some(align) = style.align {
        out.push_str(format!(" a={}", format_inline_align_value(align)).as_str());
    }
    if let Some(font_label) = style.font_label.as_deref() {
        let sanitized = font_label.replace(['"', '<', '>'], "");
        out.push_str(format!(" f=\"{sanitized}\"").as_str());
    }
    if let Some(font_size_px) = style.font_size_px {
        out.push_str(format!(" s={font_size_px:.2}").as_str());
    }
    if let Some(color) = style.text_color {
        out.push_str(
            format!(
                " c={:02X}{:02X}{:02X}{:02X}",
                color.r(),
                color.g(),
                color.b(),
                color.a()
            )
            .as_str(),
        );
    }
    if let Some(line_spacing) = style.line_spacing {
        out.push_str(format!(" l={}", line_spacing.to_token()).as_str());
    }
    if let Some(kerning) = style.kerning {
        out.push_str(format!(" k={}", kerning.to_token()).as_str());
    }
    if let Some([stretch_x, stretch_y]) = style.glyph_stretching {
        out.push_str(format!(" w={} h={}", stretch_x.to_token(), stretch_y.to_token()).as_str());
    }
    if let Some(offset) = style.glyph_offset {
        if offset.global_x.value != 0.0 {
            out.push_str(format!(" x={}", offset.global_x.to_token()).as_str());
        }
        if offset.global_y.value != 0.0 {
            out.push_str(format!(" y={}", offset.global_y.to_token()).as_str());
        }
        if offset.line.value != 0.0 {
            out.push_str(format!(" n={}", offset.line.to_token()).as_str());
        }
        if offset.shift_following {
            out.push_str(" q");
        }
        if offset.group_rotation_deg != 0.0 {
            out.push_str(format!(" g={:.2}", offset.group_rotation_deg).as_str());
        }
        if offset.glyph_rotation_deg != 0.0 {
            out.push_str(format!(" r={:.2}", offset.glyph_rotation_deg).as_str());
        }
    }
    out.push('>');
    if out == "<m>" { String::new() } else { out }
}

pub(super) fn build_inline_opening_tags(style: &TypingInlineTagStyle) -> String {
    let mut out = String::new();
    if let Some(font_label) = style.font_label.as_deref() {
        out.push_str(format!("<font={font_label}>").as_str());
    }
    if let Some(font_size_px) = style.font_size_px {
        out.push_str(format!("<size={font_size_px:.2}>").as_str());
    }
    if let Some(text_color) = style.text_color {
        out.push_str(format_inline_color_tag(text_color).as_str());
    }
    if let Some(line_spacing) = style.line_spacing {
        out.push_str(format!("<line-spacing={}>", line_spacing.to_token()).as_str());
    }
    if let Some(kerning) = style.kerning {
        out.push_str(format!("<kerning={}>", kerning.to_token()).as_str());
    }
    if let Some([stretch_x, stretch_y]) = style.glyph_stretching {
        out.push_str(
            format!("<stretching={},{}>", stretch_x.to_token(), stretch_y.to_token()).as_str(),
        );
    }
    if let Some(offset) = style.glyph_offset {
        out.push_str(format_inline_offset_tag(offset).as_str());
    }
    if style.no_break {
        out.push_str("<no-break>");
    }
    if let Some(align) = style.align {
        out.push_str(format!("<align={}>", format_inline_align_value(align)).as_str());
    }
    if style.bold {
        out.push_str("<b>");
    }
    if style.italic {
        out.push_str("<i>");
    }
    out
}

pub(super) fn build_inline_closing_tags(style: &TypingInlineTagStyle) -> String {
    let mut out = String::new();
    if style.italic {
        out.push_str("</i>");
    }
    if style.bold {
        out.push_str("</b>");
    }
    if style.align.is_some() {
        out.push_str("</align>");
    }
    if style.no_break {
        out.push_str("</no-break>");
    }
    if style.glyph_offset.is_some() {
        out.push_str("</offset>");
    }
    if style.glyph_stretching.is_some() {
        out.push_str("</stretching>");
    }
    if style.kerning.is_some() {
        out.push_str("</kerning>");
    }
    if style.line_spacing.is_some() {
        out.push_str("</line-spacing>");
    }
    if style.text_color.is_some() {
        out.push_str("</color>");
    }
    if style.font_size_px.is_some() {
        out.push_str("</size>");
    }
    if style.font_label.is_some() {
        out.push_str("</font>");
    }
    out
}

pub(super) fn format_inline_offset_tag(offset: TypingInlineOffsetStyle) -> String {
    format!(
        "<offset={},{},{},{},{:.2},{:.2}>",
        offset.global_x.to_token(),
        offset.global_y.to_token(),
        offset.line.to_token(),
        if offset.shift_following { 1 } else { 0 },
        offset.group_rotation_deg,
        offset.glyph_rotation_deg
    )
}

pub(super) fn clamp_px_or_percent(value: PxOrPercent, limit: f32) -> PxOrPercent {
    PxOrPercent {
        value: value.value.clamp(-limit, limit),
        is_percent: value.is_percent,
    }
}

/// Считаются ли два значения различающимися (по единице или по величине).
pub(super) fn px_or_percent_differs(left: PxOrPercent, right: PxOrPercent) -> bool {
    left.is_percent != right.is_percent || (left.value - right.value).abs() > 0.05
}

/// Прочитать параметр `px-или-%`: сначала новый строковый ключ-токен, затем
/// устаревшие отдельные ключи `*_px`/`*_percent` (с приоритетом пикселей).
pub(super) fn read_legacy_or_token_px_or_percent(
    obj: &serde_json::Map<String, Value>,
    token_key: &str,
    legacy_px_key: &str,
    legacy_percent_key: &str,
    default: PxOrPercent,
) -> PxOrPercent {
    if let Some(value) = obj.get(token_key) {
        if let Some(text) = value.as_str() {
            if let Some(parsed) = PxOrPercent::parse(text) {
                return parsed;
            }
        } else if let Some(number) = value_as_f32(value) {
            // Голое число в ключе-токене встречается лишь в легаси `line_spacing`,
            // где оно означало пиксели.
            return PxOrPercent::px(number);
        }
    }
    let legacy_px = obj.get(legacy_px_key).and_then(value_as_f32);
    let legacy_percent = obj.get(legacy_percent_key).and_then(value_as_f32);
    if legacy_px.is_some() || legacy_percent.is_some() {
        return PxOrPercent::from_legacy_pair(
            legacy_px.unwrap_or(0.0),
            legacy_percent.unwrap_or(0.0),
        );
    }
    default
}

pub(super) fn normalize_inline_offset_style(offset: TypingInlineOffsetStyle) -> TypingInlineOffsetStyle {
    TypingInlineOffsetStyle {
        global_x: clamp_px_or_percent(offset.global_x, 100.0),
        global_y: clamp_px_or_percent(offset.global_y, 100.0),
        line: clamp_px_or_percent(offset.line, 300.0),
        shift_following: offset.shift_following,
        group_rotation_deg: offset.group_rotation_deg.clamp(-180.0, 180.0),
        glyph_rotation_deg: offset.glyph_rotation_deg.clamp(-180.0, 180.0),
    }
}

pub(super) fn inline_offset_style_is_non_default(offset: &TypingInlineOffsetStyle) -> bool {
    offset.global_x.value.abs() > 0.05
        || offset.global_y.value.abs() > 0.05
        || offset.line.value.abs() > 0.05
        || offset.shift_following
        || offset.group_rotation_deg.abs() > 0.05
        || offset.glyph_rotation_deg.abs() > 0.05
}

pub(super) fn format_inline_color_tag(color: Color32) -> String {
    format!(
        "<color=#{:02X}{:02X}{:02X}{:02X}>",
        color.r(),
        color.g(),
        color.b(),
        color.a()
    )
}

pub(super) fn parse_inline_hex_color(value: &str) -> Option<Color32> {
    let hex = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
        .trim()
        .strip_prefix('#')
        .unwrap_or(value.trim())
        .trim();
    match hex.len() {
        6 => {
            let rgb = u32::from_str_radix(hex, 16).ok()?;
            Some(Color32::from_rgba_unmultiplied(
                u8::try_from((rgb >> 16) & 0xFF).ok()?,
                u8::try_from((rgb >> 8) & 0xFF).ok()?,
                u8::try_from(rgb & 0xFF).ok()?,
                255,
            ))
        }
        8 => {
            let rgba = u32::from_str_radix(hex, 16).ok()?;
            Some(Color32::from_rgba_unmultiplied(
                u8::try_from((rgba >> 24) & 0xFF).ok()?,
                u8::try_from((rgba >> 16) & 0xFF).ok()?,
                u8::try_from((rgba >> 8) & 0xFF).ok()?,
                u8::try_from(rgba & 0xFF).ok()?,
            ))
        }
        _ => None,
    }
}

/// Разобрать машиночитаемый тег `<m ...>` в полный inline-стиль панели.
pub(super) fn parse_machine_tag_style(raw: &str) -> Option<TypingInlineTagStyle> {
    let attrs = parse_machine_tag(raw)?;
    let mut style = TypingInlineTagStyle::default();
    let mut offset = TypingInlineOffsetStyle::global_only([0.0, 0.0]);
    let mut has_offset = false;
    let mut stretch_w: Option<PxOrPercent> = None;
    let mut stretch_h: Option<PxOrPercent> = None;

    for (key, value) in &attrs {
        match key {
            'b' => style.bold = true,
            'i' => style.italic = true,
            'j' | 'J' => style.no_break = true,
            'a' | 'A' => {
                if let Some(align) = parse_inline_align_value(value) {
                    style.align = Some(align);
                }
            }
            'f' => {
                let label = value.trim();
                if !label.is_empty() {
                    style.font_label = Some(label.to_string());
                }
            }
            's' => {
                if let Ok(px) = value.trim().parse::<f32>()
                    && px.is_finite()
                    && px > 0.0
                {
                    style.font_size_px = Some(px);
                }
            }
            'c' => {
                if let Some(color) = parse_inline_hex_color(value) {
                    style.text_color = Some(color);
                }
            }
            'l' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    style.line_spacing = Some(clamp_px_or_percent(parsed, 300.0));
                }
            }
            'k' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    style.kerning = Some(clamp_px_or_percent(parsed, 300.0));
                }
            }
            'w' => stretch_w = PxOrPercent::parse(value).map(clamp_stretch_px_or_percent),
            'h' => stretch_h = PxOrPercent::parse(value).map(clamp_stretch_px_or_percent),
            'x' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    offset.global_x = clamp_px_or_percent(parsed, 100.0);
                    has_offset = true;
                }
            }
            'y' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    offset.global_y = clamp_px_or_percent(parsed, 100.0);
                    has_offset = true;
                }
            }
            'n' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    offset.line = clamp_px_or_percent(parsed, 300.0);
                    has_offset = true;
                }
            }
            'g' => {
                if let Ok(deg) = value.trim().parse::<f32>()
                    && deg.is_finite()
                {
                    offset.group_rotation_deg = deg.clamp(-180.0, 180.0);
                    has_offset = true;
                }
            }
            'r' => {
                if let Ok(deg) = value.trim().parse::<f32>()
                    && deg.is_finite()
                {
                    offset.glyph_rotation_deg = deg.clamp(-180.0, 180.0);
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
        style.glyph_stretching = Some([
            stretch_w.unwrap_or(PxOrPercent::percent(100.0)),
            stretch_h.unwrap_or(PxOrPercent::percent(100.0)),
        ]);
    }
    if has_offset {
        style.glyph_offset = Some(offset);
    }

    Some(style)
}

pub(super) fn parse_inline_offset_value(raw: &str) -> Option<TypingInlineOffsetStyle> {
    let (tag_name, value) = raw.split_once('=')?;
    if !tag_name.trim().eq_ignore_ascii_case("offset") {
        return None;
    }

    let value = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
        .trim();
    let parts = value.split(',').map(str::trim).collect::<Vec<_>>();
    // X/Y/«по линии» поддерживают суффикс `%` (проценты от кегля), иначе пиксели.
    let global_x = PxOrPercent::parse(parts.first()?)?;
    let global_y = PxOrPercent::parse(parts.get(1)?)?;
    if !global_x.value.is_finite() || !global_y.value.is_finite() {
        return None;
    }

    let line = parts
        .get(2)
        .and_then(|value| PxOrPercent::parse(value))
        .filter(|value| value.value.is_finite())
        .unwrap_or(PxOrPercent::px(0.0));
    let shift_following = parts
        .get(3)
        .is_some_and(|value| parse_inline_bool(value).unwrap_or(false));
    let group_rotation_deg = parts
        .get(4)
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(0.0);
    let glyph_rotation_deg = parts
        .get(5)
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(0.0);

    Some(normalize_inline_offset_style(TypingInlineOffsetStyle {
        global_x,
        global_y,
        line,
        shift_following,
        group_rotation_deg,
        glyph_rotation_deg,
    }))
}

pub(super) fn parse_inline_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Извлечь значение тега `name=...`, обрезав кавычки/пробелы.
pub(super) fn inline_tag_value<'a>(raw: &'a str, tag_name: &str) -> Option<&'a str> {
    let (raw_name, value) = raw.split_once('=')?;
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

/// Одиночное значение `px-или-%` (или устаревшая пара `px,percent`, которая
/// сворачивается с приоритетом пикселей) для тегов line-spacing/kerning.
pub(super) fn parse_inline_value_or_legacy_pair(
    raw: &str,
    tag_name: &str,
    clamp_abs: f32,
) -> Option<PxOrPercent> {
    let value = inline_tag_value(raw, tag_name)?;
    if let Some((x_raw, y_raw)) = value.split_once(',') {
        let px = x_raw.trim().parse::<f32>().ok()?;
        let percent = y_raw.trim().parse::<f32>().ok()?;
        if !px.is_finite() || !percent.is_finite() {
            return None;
        }
        return Some(clamp_px_or_percent(
            PxOrPercent::from_legacy_pair(px, percent),
            clamp_abs,
        ));
    }
    let parsed = PxOrPercent::parse(value)?;
    if !parsed.value.is_finite() {
        return None;
    }
    Some(clamp_px_or_percent(parsed, clamp_abs))
}

/// `stretching=ширина,высота`, где каждая компонента — `px-или-%` (1..=300).
pub(super) fn parse_inline_stretch_value(raw: &str) -> Option<[PxOrPercent; 2]> {
    let value = inline_tag_value(raw, "stretching")?;
    let (x_raw, y_raw) = value.split_once(',')?;
    let width = PxOrPercent::parse(x_raw)?;
    let height = PxOrPercent::parse(y_raw)?;
    if !width.value.is_finite() || !height.value.is_finite() {
        return None;
    }
    Some([
        clamp_stretch_px_or_percent(width),
        clamp_stretch_px_or_percent(height),
    ])
}

pub(super) fn clamp_stretch_px_or_percent(value: PxOrPercent) -> PxOrPercent {
    PxOrPercent {
        value: value.value.clamp(1.0, 300.0),
        is_percent: value.is_percent,
    }
}
