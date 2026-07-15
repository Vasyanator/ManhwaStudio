/*
File: panel/ui_helpers.rs

Purpose:
Free-function UI helpers extracted verbatim from panel.rs for the typing tab's
create/edit panels.

Main responsibilities:
- font-family binding, deterministic combo naming, group/path matching;
- size-to-box fitting for previews;
- horizontal wheel-scroll handling for parameter strips;
- the px-or-percent parameter row and the wheel-step appliers (f32/u32/u8);
- enum cyclers for text shape, wrap mode, anti-aliasing, line mode, vertical
  line direction, and layout mode;
- enum-to-string and string-to-enum parse/label helpers;
- serde Value scalar readers (u8/u64/f32/color);
- formula-layout approximate equality and angle normalization.

Notes:
`use super::*;` pulls in the parent panel module's types and imports. Moved free
fns are `pub(super)` so panel.rs and its sibling submodules can call them. The
local `normalize_angle_deg` is panel-scoped and independent of the same-named
helper in tab/geometry.rs.
*/

use super::*;

/// Mutable borrows of the faux bold/italic controls for [`draw_faux_style_controls`].
///
/// Bundles the faux sub-state (bold: enable + thicken/expand/sharp/outward,
/// italic: enable + slant) into one parameter so the draw fn stays under the
/// argument-count lint. The caller owns the fields, so the same widget set drives
/// both the whole-overlay state and a per-selection inline style.
pub(super) struct FauxStyleControlValues<'a> {
    pub(super) faux_bold: &'a mut bool,
    pub(super) faux_bold_thicken_percent: &'a mut f32,
    pub(super) faux_bold_expand_percent: &'a mut f32,
    pub(super) faux_bold_sharp_corners: &'a mut bool,
    pub(super) faux_bold_outward_only: &'a mut bool,
    pub(super) faux_italic: &'a mut bool,
    pub(super) faux_italic_slant_deg: &'a mut f32,
}

/// Draw the bold and italic rows: each a `[style checkbox] [«Принудительно» faux
/// checkbox]` pair, followed by the faux parameter strip (thicken/expand/sharp/
/// outward for bold, slant for italic) shown only while both that style and its
/// faux flag are on. `bold`/`italic` and the `values` fields are edited in place;
/// every widget response is OR-ed into `changed`. `id_salt` disambiguates the two
/// indented parameter strips across the panels that reuse this helper.
pub(super) fn draw_faux_style_controls(
    ui: &mut egui::Ui,
    bold: &mut bool,
    italic: &mut bool,
    values: FauxStyleControlValues<'_>,
    changed: &mut bool,
    block_hscroll_by_hovered_param: &mut bool,
    id_salt: &str,
) {
    let FauxStyleControlValues { faux_bold, faux_bold_thicken_percent, faux_bold_expand_percent, faux_bold_sharp_corners, faux_bold_outward_only, faux_italic, faux_italic_slant_deg } = values;
    ui.horizontal(|ui| {
        let response = ui.checkbox(bold, t!("typing.params.bold"));
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &response);
        *changed |= response.changed();
        let response = ui.checkbox(faux_bold, t!("typing.params.force"));
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &response);
        *changed |= response.changed();
    });
    if *bold && *faux_bold {
        ui.indent(Id::new(id_salt).with("faux_bold"), |ui| {
            let response = ui.add(WheelSlider::new(faux_bold_thicken_percent, 0.0..=25.0).suffix("%").text(t!("typing.params.thicken")));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &response);
            *changed |= response.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &response) {
                *changed |= apply_wheel_step_f32(faux_bold_thicken_percent, steps, 1.0, 0.0, 25.0);
            }
            let response = ui.add(WheelSlider::new(faux_bold_expand_percent, 0.0..=50.0).suffix("%").text(t!("typing.params.extra_spacing")));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &response);
            *changed |= response.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &response) {
                *changed |= apply_wheel_step_f32(faux_bold_expand_percent, steps, 1.0, 0.0, 50.0);
            }
            let response = ui.checkbox(faux_bold_sharp_corners, t!("typing.params.sharp_corners"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &response);
            *changed |= response.changed();
            let response = ui.checkbox(faux_bold_outward_only, t!("typing.params.outward_only"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &response);
            *changed |= response.changed();
        });
    }
    ui.horizontal(|ui| {
        let response = ui.checkbox(italic, t!("typing.params.italic"));
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &response);
        *changed |= response.changed();
        let response = ui.checkbox(faux_italic, t!("typing.params.force"));
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &response);
        *changed |= response.changed();
    });
    if *italic && *faux_italic {
        ui.indent(Id::new(id_salt).with("faux_italic"), |ui| {
            let response = ui.add(WheelSlider::new(faux_italic_slant_deg, -45.0..=45.0).suffix("°").text(t!("typing.params.slant")));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &response);
            *changed |= response.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &response) {
                *changed |= apply_wheel_step_f32(faux_italic_slant_deg, steps, 1.0, -45.0, 45.0);
            }
        });
    }
}

pub(super) fn is_font_family_bound(ctx: &egui::Context, family: &egui::FontFamily) -> bool {
    ctx.fonts(|fonts| fonts.definitions().families.contains_key(family))
}

/// Детерминированное имя egui-семейства для UI-превью шрифта в комбобоксе.
/// Зависит только от (путь, индекс начертания), поэтому один и тот же файл всегда
/// регистрируется под одним именем (безопасно разделяется между панелями `create`
/// и `edit`, у которых общий egui-`Context`), а разные файлы получают разные имена.
pub(super) fn combo_font_family_name(font_path: &Path, face_index: usize) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    font_path.hash(&mut hasher);
    face_index.hash(&mut hasher);
    format!("typing-panel-combo-font-{:016x}", hasher.finish())
}

/// Принадлежит ли шрифт группе `group` (учитывает объединённые копии).
pub(super) fn font_in_group(font: &FontEntry, group: &str) -> bool {
    font.groups.iter().any(|g| g.as_deref() == Some(group))
}

/// Совпадает ли `raw`-путь с представительным или альтернативным путём шрифта.
pub(super) fn font_matches_path(font: &FontEntry, raw: &str) -> bool {
    let candidate = Path::new(raw);
    font.path == candidate
        || font.path.to_string_lossy() == raw
        || font
            .alt_paths
            .iter()
            .any(|alt| alt == candidate || alt.to_string_lossy() == raw)
}

pub(super) fn fit_size_to_box(source_size: [usize; 2], box_size: Vec2) -> Vec2 {
    let src_w = source_size[0].max(1) as f32;
    let src_h = source_size[1].max(1) as f32;
    let box_w = box_size.x.max(1.0);
    let box_h = box_size.y.max(1.0);
    let scale = (box_w / src_w).min(box_h / src_h).min(1.0);
    Vec2::new((src_w * scale).max(1.0), (src_h * scale).max(1.0))
}

pub(super) fn mark_hscroll_block_on_hover(block: &mut bool, response: &egui::Response) {
    let _ = (block, response);
}

pub(super) fn apply_horizontal_wheel_scroll_if_idle(ui: &mut egui::Ui, block_by_hovered_param: bool) {
    if block_by_hovered_param || !ui.ui_contains_pointer() {
        return;
    }

    let scroll_delta = ui.ctx().input(|input| {
        // For horizontal-only strip we intentionally treat vertical wheel as horizontal scroll.
        input.smooth_scroll_delta.x + input.smooth_scroll_delta.y
    });
    if scroll_delta.abs() <= f32::EPSILON {
        return;
    }

    ui.scroll_with_delta(Vec2::new(scroll_delta, 0.0));
    consume_wheel_scroll_delta(ui);
}

pub(super) fn consume_wheel_scroll_delta(ui: &egui::Ui) {
    ui.ctx().input_mut(|input| {
        input.smooth_scroll_delta = Vec2::ZERO;
    });
}

pub(super) fn wheel_steps_if_hovered(ui: &egui::Ui, response: &egui::Response) -> Option<i32> {
    let _ = (ui, response);
    None
}

/// Конфигурация строки `px_or_percent_param_row`: диапазон слайдера, шаг колеса и размер
/// шрифта, через который пересчитываются пиксели ↔ проценты.
pub(super) struct PxOrPercentRowCfg {
    /// Допустимый диапазон значения (в текущей единице строки).
    pub(super) range: std::ops::RangeInclusive<f32>,
    /// Шаг изменения значения колесом мыши.
    pub(super) wheel_step: f32,
    /// Размер шрифта в px, используемый для конверсии px ↔ % от кегля.
    pub(super) font_size_px: f32,
}

/// Строка параметра «значение + переключатель X / X%» (пиксели или проценты от кегля).
///
/// При переключении единицы значение пересчитывается через `cfg.font_size_px`, чтобы
/// итоговый результат остался максимально близким (px ↔ % от размера шрифта).
pub(super) fn px_or_percent_param_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut PxOrPercent,
    cfg: PxOrPercentRowCfg,
    changed: &mut bool,
    block_hscroll_by_hovered_param: &mut bool,
) {
    ui.horizontal(|ui| {
        let min = *cfg.range.start();
        let max = *cfg.range.end();
        let slider_resp = ui.add(WheelSlider::new(&mut value.value, cfg.range).text(label));
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &slider_resp);
        *changed |= slider_resp.changed();
        if let Some(steps) = wheel_steps_if_hovered(ui, &slider_resp) {
            *changed |= apply_wheel_step_f32(&mut value.value, steps, cfg.wheel_step, min, max);
        }
        let mut want_percent = value.is_percent;
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(4, 1))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if ui
                    .selectable_label(!want_percent, "X")
                    .on_hover_text(t!("typing.params.unit_pixels"))
                    .clicked()
                {
                    want_percent = false;
                }
                if ui
                    .selectable_label(want_percent, "X%")
                    .on_hover_text(t!("typing.params.unit_percent_of_font"))
                    .clicked()
                {
                    want_percent = true;
                }
            });
        if want_percent != value.is_percent {
            // Подбираем значение в новой единице с наиболее близким результатом.
            let converted = if want_percent {
                value.as_percent_of(cfg.font_size_px)
            } else {
                value.as_px_of(cfg.font_size_px)
            };
            value.value = converted.clamp(min, max);
            value.is_percent = want_percent;
            *changed = true;
        }
    });
}

pub(super) fn apply_wheel_step_f32(value: &mut f32, steps: i32, step_size: f32, min: f32, max: f32) -> bool {
    if steps == 0 {
        return false;
    }
    let prev = *value;
    *value = (*value + steps as f32 * step_size).clamp(min, max);
    (*value - prev).abs() > f32::EPSILON
}

pub(super) fn apply_wheel_step_u32(value: &mut u32, steps: i32, step_size: u32, min: u32, max: u32) -> bool {
    if steps == 0 || step_size == 0 {
        return false;
    }
    let prev = *value;
    let signed = *value as i64 + steps as i64 * step_size as i64;
    *value = signed.clamp(min as i64, max as i64) as u32;
    *value != prev
}

pub(super) fn apply_wheel_step_u8(value: &mut u8, steps: i32, step_size: u8, min: u8, max: u8) -> bool {
    if steps == 0 || step_size == 0 {
        return false;
    }
    let prev = *value;
    let signed = i32::from(*value) + steps.saturating_mul(i32::from(step_size));
    let clamped = signed.clamp(i32::from(min), i32::from(max));
    let Ok(next) = u8::try_from(clamped) else {
        return false;
    };
    *value = next;
    *value != prev
}

pub(super) fn cycle_wrapped_index(index: &mut usize, len: usize, steps: i32) -> bool {
    if len == 0 || steps == 0 {
        return false;
    }

    let prev = (*index).min(len - 1);
    let shift = (steps.unsigned_abs() as usize) % len;
    if shift == 0 {
        return false;
    }

    *index = if steps > 0 {
        (prev + shift) % len
    } else {
        (prev + len - shift) % len
    };
    *index != prev
}

pub(super) fn cycle_text_shape(shape: &mut TextShape, steps: i32) -> bool {
    let mut idx = match *shape {
        TextShape::Free => 0,
        TextShape::Rectangle => 1,
        TextShape::Oval => 2,
        TextShape::Hexagon => 3,
        TextShape::SoftPeak => 4,
    };
    if !cycle_wrapped_index(&mut idx, 5, steps) {
        return false;
    }

    *shape = match idx {
        0 => TextShape::Free,
        1 => TextShape::Rectangle,
        2 => TextShape::Oval,
        3 => TextShape::Hexagon,
        _ => TextShape::SoftPeak,
    };
    true
}

pub(super) fn cycle_text_wrap_mode(mode: &mut TextWrapMode, steps: i32) -> bool {
    let mut idx = match *mode {
        TextWrapMode::None => 0,
        TextWrapMode::WholeWords => 1,
        TextWrapMode::Minimal => 2,
        TextWrapMode::Moderate => 3,
        TextWrapMode::Aggressive => 4,
    };
    if !cycle_wrapped_index(&mut idx, 5, steps) {
        return false;
    }

    *mode = match idx {
        0 => TextWrapMode::None,
        1 => TextWrapMode::WholeWords,
        2 => TextWrapMode::Minimal,
        3 => TextWrapMode::Moderate,
        _ => TextWrapMode::Aggressive,
    };
    true
}

/// Wheel-step the anti-aliasing mode in enum order
/// (None, Sharp, Crisp, Strong, Smooth). Returns `true` when the value changed.
pub(super) fn cycle_anti_aliasing(mode: &mut AntiAliasingMode, steps: i32) -> bool {
    let mut idx = match *mode {
        AntiAliasingMode::None => 0,
        AntiAliasingMode::Sharp => 1,
        AntiAliasingMode::Crisp => 2,
        AntiAliasingMode::Strong => 3,
        AntiAliasingMode::Smooth => 4,
    };
    if !cycle_wrapped_index(&mut idx, 5, steps) {
        return false;
    }

    *mode = match idx {
        0 => AntiAliasingMode::None,
        1 => AntiAliasingMode::Sharp,
        2 => AntiAliasingMode::Crisp,
        3 => AntiAliasingMode::Strong,
        _ => AntiAliasingMode::Smooth,
    };
    true
}

pub(super) fn cycle_text_line_mode(mode: &mut TextLineMode, steps: i32) -> bool {
    let mut idx = match *mode {
        TextLineMode::Horizontal => 0,
        TextLineMode::Vertical => 1,
    };
    if !cycle_wrapped_index(&mut idx, 2, steps) {
        return false;
    }
    *mode = if idx == 0 {
        TextLineMode::Horizontal
    } else {
        TextLineMode::Vertical
    };
    true
}

pub(super) fn cycle_vertical_line_direction(direction: &mut VerticalLineDirection, steps: i32) -> bool {
    let mut idx = match *direction {
        VerticalLineDirection::LeftToRight => 0,
        VerticalLineDirection::RightToLeft => 1,
    };
    if !cycle_wrapped_index(&mut idx, 2, steps) {
        return false;
    }
    *direction = if idx == 0 {
        VerticalLineDirection::LeftToRight
    } else {
        VerticalLineDirection::RightToLeft
    };
    true
}

pub(super) fn cycle_text_layout_mode(mode: &mut TextLayoutMode, steps: i32) -> bool {
    let mut idx = match *mode {
        TextLayoutMode::Normal => 0,
        TextLayoutMode::Formula => 1,
        TextLayoutMode::Shape => 1,
        TextLayoutMode::CustomRasterLines | TextLayoutMode::CustomVectorLines => 2,
    };
    if !cycle_wrapped_index(&mut idx, 3, steps) {
        return false;
    }
    *mode = match idx {
        0 => TextLayoutMode::Normal,
        1 => TextLayoutMode::Formula,
        _ => TextLayoutMode::CustomVectorLines,
    };
    true
}

pub(super) fn compute_typing_vertical_panel_auto_height(
    content_height_px: f32,
    viewport_target_height: f32,
    available_panel_height: f32,
) -> f32 {
    if content_height_px > 0.0 {
        content_height_px
            .min(viewport_target_height)
            .min(available_panel_height)
            .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX)
    } else {
        viewport_target_height
            .min(available_panel_height)
            .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX)
    }
}

pub(super) fn parse_text_shape_str(raw: &str) -> Option<TextShape> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "free" => Some(TextShape::Free),
        "rectangle" => Some(TextShape::Rectangle),
        "oval" => Some(TextShape::Oval),
        "hexagon" => Some(TextShape::Hexagon),
        "soft_peak" | "soft" | "no_trees" => Some(TextShape::SoftPeak),
        _ => None,
    }
}

pub(super) fn parse_text_wrap_mode_str(raw: &str) -> Option<TextWrapMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" => Some(TextWrapMode::None),
        "whole_words" | "words" | "word" => Some(TextWrapMode::WholeWords),
        "minimal" => Some(TextWrapMode::Minimal),
        "moderate" => Some(TextWrapMode::Moderate),
        "aggressive" | "smart" => Some(TextWrapMode::Aggressive),
        _ => None,
    }
}


pub(super) fn text_wrap_mode_label(mode: TextWrapMode) -> &'static str {
    match mode {
        TextWrapMode::None => t!("typing.params.wrap_none"),
        TextWrapMode::WholeWords => t!("typing.params.wrap_whole_words"),
        TextWrapMode::Minimal => t!("typing.params.wrap_minimal"),
        TextWrapMode::Moderate => t!("typing.params.wrap_moderate"),
        TextWrapMode::Aggressive => t!("typing.params.wrap_active"),
    }
}

/// Parse the persisted anti-aliasing token
/// (`none`/`sharp`/`crisp`/`strong`/`smooth`). Returns `None` for unknown text.
pub(super) fn parse_anti_aliasing_str(raw: &str) -> Option<AntiAliasingMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" => Some(AntiAliasingMode::None),
        "sharp" => Some(AntiAliasingMode::Sharp),
        "crisp" => Some(AntiAliasingMode::Crisp),
        "strong" => Some(AntiAliasingMode::Strong),
        "smooth" => Some(AntiAliasingMode::Smooth),
        _ => None,
    }
}

/// Russian UI label for an anti-aliasing mode.
pub(super) fn anti_aliasing_label(mode: AntiAliasingMode) -> &'static str {
    match mode {
        AntiAliasingMode::None => t!("typing.params.antialias_none"),
        AntiAliasingMode::Sharp => t!("typing.params.antialias_sharp"),
        AntiAliasingMode::Crisp => t!("typing.params.antialias_crisp"),
        AntiAliasingMode::Strong => t!("typing.params.antialias_rich"),
        AntiAliasingMode::Smooth => t!("typing.params.antialias_smooth"),
    }
}

pub(super) fn parse_text_line_mode_str(raw: &str) -> Option<TextLineMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "horizontal" => Some(TextLineMode::Horizontal),
        "vertical" => Some(TextLineMode::Vertical),
        _ => None,
    }
}

pub(super) fn parse_vertical_line_direction_str(raw: &str) -> Option<VerticalLineDirection> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "left_to_right" | "ltr" => Some(VerticalLineDirection::LeftToRight),
        "right_to_left" | "rtl" => Some(VerticalLineDirection::RightToLeft),
        _ => None,
    }
}

pub(super) fn parse_text_layout_mode_str(raw: &str) -> Option<TextLayoutMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "normal" => Some(TextLayoutMode::Normal),
        "formula" => Some(TextLayoutMode::Formula),
        "shape" => Some(TextLayoutMode::Shape),
        "drawn_lines"
        | "drawn-lines"
        | "drawnlines"
        | "custom_raster_lines"
        | "custom-raster-lines"
        | "customrasterlines" => Some(TextLayoutMode::CustomRasterLines),
        "vector_lines"
        | "vector-lines"
        | "vectorlines"
        | "custom_vector_lines"
        | "custom-vector-lines"
        | "customvectorlines" => Some(TextLayoutMode::CustomVectorLines),
        _ => None,
    }
}

/// Parse a serialized kerning-mode string. Accepts the current tokens
/// (`"fixed"`/`"auto"`/`"optical"`) and the legacy `"metric"` token, which meant
/// font-pair kerning and therefore maps to [`KerningMode::Auto`] so old overlays
/// render identically. Returns `None` for unknown/missing values.
pub(super) fn parse_kerning_mode_str(raw: &str) -> Option<KerningMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "fixed" => Some(KerningMode::Fixed),
        "auto" | "metric" => Some(KerningMode::Auto),
        "optical" => Some(KerningMode::Optical),
        _ => None,
    }
}

pub(super) fn parse_color32_value(value: &Value) -> Option<Color32> {
    let arr = value.as_array()?;
    if arr.len() < 3 {
        return None;
    }
    let r = value_as_u8(arr.first()?)?;
    let g = value_as_u8(arr.get(1)?)?;
    let b = value_as_u8(arr.get(2)?)?;
    let a = arr.get(3).and_then(value_as_u8).unwrap_or(255);
    Some(Color32::from_rgba_unmultiplied(r, g, b, a))
}

pub(super) fn value_as_u8(value: &Value) -> Option<u8> {
    if let Some(v) = value.as_u64() {
        return u8::try_from(v).ok();
    }
    value.as_f64().map(|v| v.round().clamp(0.0, 255.0) as u8)
}

pub(super) fn value_as_u64(value: &Value) -> Option<u64> {
    if let Some(v) = value.as_u64() {
        return Some(v);
    }

    value.as_f64().and_then(|v| {
        let rounded = v.round();
        if rounded.is_finite() && rounded >= 0.0 && rounded <= u64::MAX as f64 {
            Some(rounded as u64)
        } else {
            None
        }
    })
}

pub(super) fn value_as_f32(value: &Value) -> Option<f32> {
    value.as_f64().map(|v| v as f32)
}

pub(super) fn formula_layout_approx_eq(a: &TextFormulaLayoutParams, b: &TextFormulaLayoutParams) -> bool {
    const EPS: f32 = 0.0005;
    if a.x_expr.trim() != b.x_expr.trim() {
        return false;
    }
    if a.y_expr.trim() != b.y_expr.trim() {
        return false;
    }
    if a.rotation_expr.trim() != b.rotation_expr.trim() {
        return false;
    }
    if a.use_tangent_rotation != b.use_tangent_rotation {
        return false;
    }
    if (a.t_start - b.t_start).abs() > EPS
        || (a.t_end - b.t_end).abs() > EPS
        || (a.offset_x_px - b.offset_x_px).abs() > EPS
        || (a.offset_y_px - b.offset_y_px).abs() > EPS
        || (a.scale_x - b.scale_x).abs() > EPS
        || (a.scale_y - b.scale_y).abs() > EPS
        || (a.normal_offset_px - b.normal_offset_px).abs() > EPS
        || (a.letter_spacing_mul - b.letter_spacing_mul).abs() > EPS
        || (a.letter_spacing_px - b.letter_spacing_px).abs() > EPS
    {
        return false;
    }
    for idx in 0..TEXT_FORMULA_USER_VAR_COUNT {
        if (a.vars[idx] - b.vars[idx]).abs() > EPS {
            return false;
        }
    }
    true
}

pub(super) fn normalize_angle_deg(angle: f32) -> f32 {
    ((angle + 180.0).rem_euclid(360.0)) - 180.0
}
