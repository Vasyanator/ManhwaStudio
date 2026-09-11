/*
FILE HEADER (widgets/wheel_spin_box.rs)
- Назначение: переиспользуемый spinbox-виджет на базе `egui::DragValue`,
  который реагирует на колесо мыши при наведении.
- Ключевые сущности:
  - `WheelSpinBox`: обёртка над числовым вводом с конфигурацией форматирования,
    диапазона и шага колеса.
- Ключевые методы:
  - `new`: создание виджета для числа по mutable-ссылке.
  - `wheel_step`: явная настройка шага изменения от колеса.
  - builder-методы `range/speed/prefix/suffix/...`: проброс ключевых настроек `DragValue`.
- Особенности:
  - при наведении колесо мыши изменяет значение на один шаг и перехватывается,
    чтобы родительский `ScrollArea` не прокручивался.
  - если открыт popup combobox, wheel-реакция отключается, чтобы список не
    менял значения под собой.
  - если курсор находится над popup-списком combobox, hover-визуал подавляется
    для spinbox'а, который геометрически лежит под списком.
*/
#![allow(dead_code)]

use super::wheel_input_guard::{combo_popup_blocks_pointer, combo_popup_open};
use eframe::egui;
use egui::{Response, Ui, Widget, emath};
use std::ops::RangeInclusive;

type NumFormatter<'a> = Box<dyn 'a + Fn(f64, RangeInclusive<usize>) -> String>;
type NumParser<'a> = Box<dyn 'a + Fn(&str) -> Option<f64>>;
type GetSetValue<'a> = Box<dyn 'a + FnMut(Option<f64>) -> f64>;

pub struct WheelSpinBox<'a> {
    get_set_value: GetSetValue<'a>,
    speed: f64,
    wheel_step: Option<f64>,
    prefix: String,
    suffix: String,
    range: RangeInclusive<f64>,
    clamp_existing_to_range: bool,
    min_decimals: usize,
    max_decimals: Option<usize>,
    custom_formatter: Option<NumFormatter<'a>>,
    custom_parser: Option<NumParser<'a>>,
    update_while_editing: bool,
    integral: bool,
}

impl<'a> WheelSpinBox<'a> {
    pub fn new<Num: emath::Numeric>(value: &'a mut Num) -> Self {
        let mut slf = Self::from_get_set(
            move |v: Option<f64>| {
                if let Some(v) = v {
                    *value = Num::from_f64(v);
                }
                value.to_f64()
            },
            Num::INTEGRAL,
        );

        if Num::INTEGRAL {
            slf = slf.max_decimals(0).range(Num::MIN..=Num::MAX).speed(0.25);
        }

        slf
    }

    pub fn from_get_set(
        get_set_value: impl 'a + FnMut(Option<f64>) -> f64,
        integral: bool,
    ) -> Self {
        Self {
            get_set_value: Box::new(get_set_value),
            speed: 1.0,
            wheel_step: None,
            prefix: String::new(),
            suffix: String::new(),
            range: f64::NEG_INFINITY..=f64::INFINITY,
            clamp_existing_to_range: true,
            min_decimals: 0,
            max_decimals: None,
            custom_formatter: None,
            custom_parser: None,
            update_while_editing: true,
            integral,
        }
    }

    #[inline]
    pub fn speed(mut self, speed: impl Into<f64>) -> Self {
        self.speed = speed.into();
        self
    }

    #[inline]
    pub fn wheel_step(mut self, wheel_step: impl Into<f64>) -> Self {
        self.wheel_step = Some(wheel_step.into());
        self
    }

    #[inline]
    pub fn range<Num: emath::Numeric>(mut self, range: RangeInclusive<Num>) -> Self {
        self.range = range.start().to_f64()..=range.end().to_f64();
        self
    }

    #[inline]
    pub fn clamp_existing_to_range(mut self, clamp_existing_to_range: bool) -> Self {
        self.clamp_existing_to_range = clamp_existing_to_range;
        self
    }

    #[inline]
    pub fn prefix(mut self, prefix: impl ToString) -> Self {
        self.prefix = prefix.to_string();
        self
    }

    #[inline]
    pub fn suffix(mut self, suffix: impl ToString) -> Self {
        self.suffix = suffix.to_string();
        self
    }

    #[inline]
    pub fn min_decimals(mut self, min_decimals: usize) -> Self {
        self.min_decimals = min_decimals;
        self
    }

    #[inline]
    pub fn max_decimals(mut self, max_decimals: usize) -> Self {
        self.max_decimals = Some(max_decimals);
        self
    }

    #[inline]
    pub fn max_decimals_opt(mut self, max_decimals: Option<usize>) -> Self {
        self.max_decimals = max_decimals;
        self
    }

    #[inline]
    pub fn fixed_decimals(mut self, num_decimals: usize) -> Self {
        self.min_decimals = num_decimals;
        self.max_decimals = Some(num_decimals);
        self
    }

    #[inline]
    pub fn update_while_editing(mut self, update_while_editing: bool) -> Self {
        self.update_while_editing = update_while_editing;
        self
    }

    pub fn custom_formatter(
        mut self,
        formatter: impl 'a + Fn(f64, RangeInclusive<usize>) -> String,
    ) -> Self {
        self.custom_formatter = Some(Box::new(formatter));
        self
    }

    pub fn custom_parser(mut self, parser: impl 'a + Fn(&str) -> Option<f64>) -> Self {
        self.custom_parser = Some(Box::new(parser));
        self
    }

    fn effective_wheel_step(&self) -> f64 {
        if let Some(wheel_step) = self.wheel_step {
            return wheel_step.abs();
        }
        if self.integral {
            return 1.0;
        }
        if let Some(max_decimals) = self.max_decimals {
            return 10f64.powi(-(max_decimals as i32));
        }
        self.speed.abs().max(f64::EPSILON)
    }

    fn build_drag_value(&mut self) -> egui::DragValue<'_> {
        let mut widget = egui::DragValue::from_get_set(&mut self.get_set_value)
            .speed(self.speed)
            .range(self.range.clone())
            .clamp_existing_to_range(self.clamp_existing_to_range)
            .min_decimals(self.min_decimals)
            .max_decimals_opt(self.max_decimals)
            .update_while_editing(self.update_while_editing);

        if !self.prefix.is_empty() {
            widget = widget.prefix(self.prefix.as_str());
        }
        if !self.suffix.is_empty() {
            widget = widget.suffix(self.suffix.as_str());
        }
        if let Some(formatter) = self.custom_formatter.take() {
            widget = widget.custom_formatter(formatter);
        }
        if let Some(parser) = self.custom_parser.take() {
            widget = widget.custom_parser(parser);
        }

        widget
    }
}

impl Widget for WheelSpinBox<'_> {
    fn ui(mut self, ui: &mut Ui) -> Response {
        let pointer_blocked_by_combo_popup = combo_popup_blocks_pointer(ui.ctx());
        let mut response = if pointer_blocked_by_combo_popup {
            ui.scope(|ui| {
                let inactive = ui.visuals().widgets.inactive;
                ui.style_mut().visuals.widgets.hovered = inactive;
                ui.style_mut().visuals.widgets.active = inactive;
                ui.add(self.build_drag_value())
            })
            .inner
        } else {
            ui.add(self.build_drag_value())
        };
        let wheel_step = self.effective_wheel_step();

        if apply_hovered_wheel_delta(
            ui,
            &response,
            &mut self.get_set_value,
            &self.range,
            wheel_step,
        ) {
            response.mark_changed();
        }

        response
    }
}

fn apply_hovered_wheel_delta(
    ui: &Ui,
    response: &Response,
    get_set_value: &mut GetSetValue<'_>,
    range: &RangeInclusive<f64>,
    step_size: f64,
) -> bool {
    if combo_popup_blocks_pointer(ui.ctx()) || combo_popup_open(ui.ctx()) {
        return false;
    }
    if !response.hovered() && !response.has_focus() {
        return false;
    }

    let (raw_wheel_events, smooth_scroll_delta) = ui
        .ctx()
        .input(|input| (raw_wheel_events_delta(input), input.smooth_scroll_delta));
    let raw_wheel_delta = axis_wheel_delta(raw_wheel_events);
    let smooth_wheel_delta = axis_wheel_delta(smooth_scroll_delta);
    if raw_wheel_delta.abs() <= f32::EPSILON && smooth_wheel_delta.abs() <= f32::EPSILON {
        return false;
    }

    let mut changed = false;
    if raw_wheel_delta.abs() > f32::EPSILON {
        let prev = get(get_set_value);
        let next = (prev + f64::from(wheel_direction(raw_wheel_delta)) * step_size)
            .clamp(*range.start(), *range.end());
        if (next - prev).abs() > f64::EPSILON {
            set(get_set_value, next);
            changed = true;
        }
    }

    consume_wheel_scroll_delta(ui.ctx());
    if smooth_wheel_delta.abs() > f32::EPSILON {
        ui.ctx().request_repaint();
    }
    changed
}

/// Sums the raw (unsmoothed) mouse-wheel delta reported this frame.
///
/// egui 0.35 removed `InputState::raw_scroll_delta`, so the per-frame unsmoothed
/// wheel movement is recovered by summing `Event::MouseWheel` deltas. Unlike
/// `smooth_scroll_delta`, which ramps over several frames, this is nonzero only on
/// the frame a physical wheel notch arrives, so it yields exactly one step per notch.
/// Only the sign is used downstream, so the event `unit` is irrelevant.
fn raw_wheel_events_delta(input: &egui::InputState) -> egui::Vec2 {
    input
        .events
        .iter()
        .filter_map(|event| match event {
            egui::Event::MouseWheel { delta, .. } => Some(*delta),
            _ => None,
        })
        .fold(egui::Vec2::ZERO, |acc, delta| acc + delta)
}

fn axis_wheel_delta(delta: egui::Vec2) -> f32 {
    if delta.y.abs() > f32::EPSILON {
        delta.y
    } else {
        delta.x
    }
}

fn wheel_direction(wheel_delta: f32) -> i32 {
    if wheel_delta > 0.0 { 1 } else { -1 }
}

fn consume_wheel_scroll_delta(ctx: &egui::Context) {
    ctx.input_mut(|input| {
        input.smooth_scroll_delta = egui::Vec2::ZERO;
    });
}

fn get(get_set_value: &mut GetSetValue<'_>) -> f64 {
    (get_set_value)(None)
}

fn set(get_set_value: &mut GetSetValue<'_>, value: f64) {
    (get_set_value)(Some(value));
}
