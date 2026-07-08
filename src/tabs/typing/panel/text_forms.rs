/*
File: panel/text_forms.rs

Purpose:
Free-function helpers extracted verbatim from panel.rs for text-form editing:
char/byte range conversions, inclusive bounds over an iterator, and the
advanced-form window support (range-row filter UI, form sorting, and form-card
drawing).

Notes:
Extracted verbatim from `panel.rs`. Free fns are `pub(super)` and the parent
`panel.rs` glob-re-exports them so siblings can call them unqualified.
`use super::*;` pulls in the parent module's types and imports.
*/

use super::*;

pub(super) fn clamp_char_range(text: &str, range: Range<usize>) -> Range<usize> {
    let text_char_count = text.chars().count();
    let start = range.start.min(text_char_count);
    let end = range.end.min(text_char_count);
    start.min(end)..end.max(start)
}

pub(super) fn cycle_wrapped_index_in_values(current: &mut usize, values: &[usize], steps: i32) {
    if steps == 0 || values.is_empty() {
        return;
    }
    let current_pos = values
        .iter()
        .position(|value| value == current)
        .unwrap_or(0);
    let mut next_pos = i32::try_from(current_pos).unwrap_or(0) + steps;
    let values_len = i32::try_from(values.len()).unwrap_or(0);
    while next_pos < 0 {
        next_pos += values_len;
    }
    while next_pos >= values_len {
        next_pos -= values_len;
    }
    if let Some(next_value) = usize::try_from(next_pos)
        .ok()
        .and_then(|idx| values.get(idx))
        .copied()
    {
        *current = next_value;
    }
}

pub(super) fn char_range_to_byte_range(text: &str, range: &Range<usize>) -> Option<Range<usize>> {
    let clamped = clamp_char_range(text, range.clone());
    let start = char_index_to_byte_index(text, clamped.start)?;
    let end = char_index_to_byte_index(text, clamped.end)?;
    Some(start..end)
}

pub(super) fn byte_range_to_char_range(text: &str, range: &Range<usize>) -> Option<Range<usize>> {
    let start = byte_index_to_char_index(text, range.start)?;
    let end = byte_index_to_char_index(text, range.end)?;
    Some(start..end)
}

pub(super) fn char_index_to_byte_index(text: &str, char_index: usize) -> Option<usize> {
    let char_count = text.chars().count();
    if char_index > char_count {
        return None;
    }
    if char_index == char_count {
        return Some(text.len());
    }
    text.char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
}

pub(super) fn byte_index_to_char_index(text: &str, byte_index: usize) -> Option<usize> {
    if byte_index > text.len() || !text.is_char_boundary(byte_index) {
        return None;
    }
    Some(text[..byte_index].chars().count())
}

/// `(min, max)` значений итератора; `(0, 0)` для пустого. `Default` даёт ноль
/// для числовых типов.
pub(super) fn inclusive_bounds<T: Ord + Copy + Default>(values: impl Iterator<Item = T>) -> (T, T) {
    let mut iter = values;
    let Some(first) = iter.next() else {
        return (T::default(), T::default());
    };
    let mut lo = first;
    let mut hi = first;
    for value in iter {
        if value < lo {
            lo = value;
        }
        if value > hi {
            hi = value;
        }
    }
    (lo, hi)
}

/// Строка фильтра-диапазона `(от, до)` для окна форм. Не рисуется, если границы
/// схлопнуты (`bounds.0 >= bounds.1`) — фильтровать нечего. Возвращает `true`,
/// если строка была показана.
pub(super) fn advanced_form_range_row<T>(
    ui: &mut egui::Ui,
    label: &str,
    suffix: &str,
    value: &mut (T, T),
    bounds: (T, T),
) -> bool
where
    T: egui::emath::Numeric + Ord + Copy,
{
    if bounds.0 >= bounds.1 {
        // Все формы имеют одно значение — фильтр бессмыслен; держим диапазон полным.
        *value = bounds;
        return false;
    }
    value.0 = value.0.clamp(bounds.0, bounds.1);
    value.1 = value.1.clamp(bounds.0, bounds.1);
    if value.0 > value.1 {
        value.0 = value.1;
    }
    // Шаг колеса/перетаскивания ~1/100 диапазона, чтобы крупные пиксельные
    // ширины не приходилось крутить по единице, а мелкие счётчики шли точно.
    let span = bounds.1.to_f64() - bounds.0.to_f64();
    let step = (span / 100.0).max(1.0);
    ui.horizontal(|ui| {
        ui.label(label);
        let hi_now = value.1;
        ui.add(
            WheelSpinBox::new(&mut value.0)
                .range(bounds.0..=hi_now)
                .wheel_step(step)
                .speed(step)
                .suffix(suffix),
        );
        ui.label("–");
        let lo_now = value.0;
        ui.add(
            WheelSpinBox::new(&mut value.1)
                .range(lo_now..=bounds.1)
                .wheel_step(step)
                .speed(step)
                .suffix(suffix),
        );
    });
    true
}

/// Сортировка форм для окна: узкие → широкие; в пределах допуска по ширине —
/// по ровности (меньшая неравномерность раньше), затем по цене разрывов,
/// пиковости и числу переносов.
pub(super) fn sort_advanced_forms(forms: &mut [TextForm]) {
    forms.sort_by_key(|a| a.max_width);
    let mut i = 0;
    while i < forms.len() {
        let run_min = forms[i].max_width;
        let mut j = i + 1;
        while j < forms.len() && forms[j].max_width <= run_min + forms::DEFAULT_WIDTH_TOLERANCE {
            j += 1;
        }
        forms[i..j].sort_by(|a, b| {
            a.conservatism
                .cmp(&b.conservatism)
                .then(a.unevenness_pct.cmp(&b.unevenness_pct))
                .then(a.break_cost.cmp(&b.break_cost))
                .then(a.max_width.cmp(&b.max_width))
                .then(
                    a.peakiness_pct(PeakBase::Min)
                        .cmp(&b.peakiness_pct(PeakBase::Min)),
                )
                .then(a.word_break_count.cmp(&b.word_break_count))
        });
        i = j;
    }
}

/// Рисует одну карточку формы: чёрный текст на белом, строки центрированы по
/// «ядру», висящая пунктуация выходит за края. Возвращает отклик клика.
pub(super) fn draw_advanced_form_card(
    ui: &mut egui::Ui,
    font_id: &egui::FontId,
    lines: &[String],
) -> egui::Response {
    const PAD_PX: f32 = 8.0;
    let row_height = ui.fonts_mut(|fonts| fonts.row_height(font_id));

    struct CardRow {
        lead: Arc<egui::Galley>,
        core: Arc<egui::Galley>,
        trail: Arc<egui::Galley>,
        core_w: f32,
        lead_w: f32,
    }

    let mut rows: Vec<CardRow> = Vec::with_capacity(lines.len());
    let mut half_extent = PAD_PX;
    for line in lines {
        let (lead_text, core_text, trail_text) = forms::split_hanging_edges(line);
        let (lead, core, trail) = ui.fonts_mut(|fonts| {
            (
                fonts.layout_no_wrap(lead_text, font_id.clone(), Color32::BLACK),
                fonts.layout_no_wrap(core_text, font_id.clone(), Color32::BLACK),
                fonts.layout_no_wrap(trail_text, font_id.clone(), Color32::BLACK),
            )
        });
        let core_w = core.size().x;
        let lead_w = lead.size().x;
        let trail_w = trail.size().x;
        half_extent = half_extent
            .max(core_w / 2.0 + lead_w)
            .max(core_w / 2.0 + trail_w);
        rows.push(CardRow {
            lead,
            core,
            trail,
            core_w,
            lead_w,
        });
    }

    let card_w = (half_extent * 2.0 + PAD_PX * 2.0).max(48.0);
    let card_h = PAD_PX * 2.0 + row_height * lines.len().max(1) as f32;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(card_w, card_h), egui::Sense::click());

    let hovered = response.hovered();
    let painter = ui.painter();
    let bg = if hovered {
        Color32::from_gray(244)
    } else {
        Color32::WHITE
    };
    painter.rect_filled(rect, 4.0, bg);
    let border = if hovered {
        Color32::from_rgb(90, 140, 220)
    } else {
        Color32::from_gray(170)
    };
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );

    let center_x = rect.center().x;
    let mut y = rect.top() + PAD_PX;
    for row in rows {
        let core_x0 = center_x - row.core_w / 2.0;
        painter.galley(
            egui::pos2(core_x0 - row.lead_w, y),
            row.lead,
            Color32::BLACK,
        );
        painter.galley(egui::pos2(core_x0, y), row.core, Color32::BLACK);
        painter.galley(
            egui::pos2(core_x0 + row.core_w, y),
            row.trail,
            Color32::BLACK,
        );
        y += row_height;
    }

    response
}
