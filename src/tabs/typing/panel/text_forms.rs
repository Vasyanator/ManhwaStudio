/*
File: panel/text_forms.rs

Purpose:
Free-function helpers extracted verbatim from panel.rs for text-form editing:
char/byte range conversions, inclusive bounds over an iterator, and the
advanced-form window support (range-row filter UI, the presentation order of the
ranked search, and form-card drawing).

Key functions:
- `order_advanced_forms`: presentation order of the ranked search (layer C of
  `dev-docs/text_forms_ranking_plan.md` §2.3) — quality floor, line-count
  buckets, narrow lean, round-robin split into SUB-ROUNDS (which is what lets the
  narrow lean coexist with "every height appears before any height repeats").

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

/// Порядок ПОКАЗА форм в окне «Продвинутая форма текста» — слой C плана
/// (`dev-docs/text_forms_ranking_plan.md` §2.3) поверх выхода
/// `forms::search_forms`.
///
/// Вход: формы, уже сгруппированные по числу строк и отсортированные по
/// `quality_milli` внутри группы (гарантия `search_forms`; порядок внутри корзины
/// здесь дополнительно закрепляется УСТОЙЧИВОЙ сортировкой по `quality_milli`,
/// поэтому контракт не зависит от того, кто собрал вектор).
///
/// Что делает порядок:
/// 1. **Порог качества** — форма, чей `quality_milli` хуже лучшего в наборе более
///    чем на `params.quality_floor_milli()`, отбрасывается (выбывают целые
///    «мусорные» корзины, а не хвосты хороших).
/// 2. **Корзины по числу строк** — естественное семейство альтернатив одной высоты.
/// 3. **Уклон в узкие** — корзина, чья ЛУЧШАЯ форма не шире МЕДИАНЫ лучших форм
///    всех корзин (нижняя медиана), получает `params.narrow_slots` мест за круг,
///    остальные — одно. Мера ОТНОСИТЕЛЬНАЯ: у большого текста все формы высокие,
///    и абсолютный порог пропорции не разделил бы их.
/// 4. **Круговой показ ПОДКРУГАМИ** — карточка ранга `i` корзины с `мест` мест за
///    круг встаёт в `круг = i / мест`, `подкруг = i % мест`; итоговый порядок —
///    по `(круг, подкруг, quality_milli)`. Подкруг 0 любого круга содержит РОВНО
///    по одной карточке каждой непустой корзины, поэтому держатся ОБА свойства
///    плана §2.3 сразу: карточка №1 — глобально лучшая форма, ни одна высота не
///    повторяется, пока не показаны все высоты, — и при этом вторая карточка
///    узкой корзины приходит сразу следующим подкругом, то есть уклон в узкие
///    остаётся ранним. Без подкругов (все места одного круга вперемешку) эти
///    свойства противоречили бы друг другу: корзина с двумя местами повторяла бы
///    свою высоту ВНУТРИ круга 0.
///
/// Формы с `forms::UNSCORED_QUALITY_MILLI` (выход легаси-перебора
/// `enumerate_forms`, качество не считалось) ранжируются ОТДЕЛЬНО и уходят в
/// ХВОСТ: их «худшее возможное» качество иначе либо целиком выбило бы их порогом,
/// либо, вперемешку с оценёнными, подняло бы неоценённую форму выше настоящих
/// карточек следующего круга.
#[must_use]
pub(super) fn order_advanced_forms(
    forms: Vec<TextForm>,
    params: &AdvancedFormParams,
) -> Vec<TextForm> {
    let (scored, unscored): (Vec<TextForm>, Vec<TextForm>) = forms
        .into_iter()
        .partition(|form| form.quality_milli != forms::UNSCORED_QUALITY_MILLI);
    let mut ordered = order_form_group(scored, params);
    // Порог качества неоценённой группы вырожден (лучшее = `u32::MAX`, сумма
    // насыщается) и не отбрасывает ничего — группа лишь раскладывается по кругам.
    ordered.extend(order_form_group(unscored, params));
    ordered
}

/// Слой C для ОДНОЙ группы форм с сопоставимым качеством (см.
/// [`order_advanced_forms`]): порог качества, корзины по высоте, уклон в узкие и
/// круговой показ подкругами. Пустой вход даёт пустой выход.
#[must_use]
fn order_form_group(forms: Vec<TextForm>, params: &AdvancedFormParams) -> Vec<TextForm> {
    let Some(best_quality) = forms.iter().map(|form| form.quality_milli).min() else {
        return Vec::new();
    };
    let quality_ceiling = best_quality.saturating_add(params.quality_floor_milli());

    let mut buckets: BTreeMap<usize, Vec<TextForm>> = BTreeMap::new();
    for form in forms
        .into_iter()
        .filter(|form| form.quality_milli <= quality_ceiling)
    {
        buckets.entry(form.line_count()).or_default().push(form);
    }
    for bucket in buckets.values_mut() {
        // Устойчивая сортировка: на выходе `search_forms` это no-op, а для любого
        // другого источника она восстанавливает «лучшее в корзине — первым».
        bucket.sort_by_key(|form| form.quality_milli);
    }

    // Нижняя медиана: при чётном числе корзин лишние места достаются более узкой
    // половине, а не обеим сразу.
    let mut best_aspects: Vec<u32> = buckets
        .values()
        .filter_map(|bucket| bucket.first().map(|form| form.aspect_milli))
        .collect();
    best_aspects.sort_unstable();
    let median_aspect = best_aspects
        .get(best_aspects.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(0);
    let narrow_slots = params.narrow_slots.max(1);

    let queues: Vec<(usize, Vec<TextForm>)> = buckets
        .into_values()
        .map(|bucket| {
            let slots = match bucket.first() {
                Some(best) if best.aspect_milli <= median_aspect => narrow_slots,
                _ => 1,
            };
            (slots, bucket)
        })
        .collect();

    // Место карточки — `(круг, подкруг, качество)`. Подкруг существует ровно ради
    // того, чтобы «уклон в узкие» не ломал «ни одна высота не повторяется, пока не
    // показаны все»: вторая карточка узкой корзины уходит в подкруг 1, то есть за
    // ПОЛНЫЙ подкруг 0, где каждая высота представлена ровно один раз.
    let mut slotted: Vec<(usize, usize, u32, TextForm)> = Vec::new();
    for (slots, bucket) in queues {
        for (rank, form) in bucket.into_iter().enumerate() {
            let quality = form.quality_milli;
            // `slots >= 1` по построению: либо `narrow_slots.max(1)`, либо единица.
            slotted.push((rank / slots, rank % slots, quality, form));
        }
    }
    // Устойчиво: при равных круге, подкруге и качестве порядок остаётся «по
    // возрастанию высоты» — корзины пришли из `BTreeMap`, то есть по возрастанию
    // числа строк.
    slotted.sort_by_key(|(round, sub_round, quality, _)| (*round, *sub_round, *quality));
    slotted.into_iter().map(|(_, _, _, form)| form).collect()
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
