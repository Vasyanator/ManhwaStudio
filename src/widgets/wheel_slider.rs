/*
FILE HEADER (widgets/wheel_slider.rs)
- Назначение: переиспользуемый `egui::Slider`, который реагирует на колесо мыши
  при наведении и блокирует прокрутку родительского интерфейса.
- Ключевые сущности:
  - `WheelSlider`: обёртка над `Slider` с настройками диапазона, подписи,
    форматирования и шага колеса.
- Ключевые методы:
  - `new`: создание виджета для числа по mutable-ссылке и диапазону.
  - `wheel_step`: явная настройка шага изменения от колеса.
  - builder-методы `text/show_value/step_by/...`: проброс основных настроек `Slider`.
- Особенности:
  - один сдвиг колеса даёт одно логическое изменение значения;
  - Shift ускоряет колесо до пяти логических шагов за движение;
  - если открыт popup combobox, wheel-реакция отключается, чтобы список не
    менял параметры под собой;
  - если курсор находится над popup-списком combobox, hover-визуал слайдера
    подавляется, даже если слайдер геометрически под списком;
  - публикует hover-состояние, чтобы глобальные wheel-hotkey не забирали событие у слайдера;
  - событие колеса потребляется локально, чтобы не скроллился контейнер выше.
*/
#![allow(dead_code)]

use super::wheel_input_guard::{combo_popup_blocks_pointer, combo_popup_open};
use eframe::egui;
use egui::style::HandleShape;
use egui::{
    Color32, Id, Rect, Response, SliderClamping, SliderOrientation, Ui, Widget, WidgetText, emath,
};
use std::ops::RangeInclusive;

type NumFormatter<'a> = Box<dyn 'a + Fn(f64, RangeInclusive<usize>) -> String>;
type NumParser<'a> = Box<dyn 'a + Fn(&str) -> Option<f64>>;
type GetSetValue<'a> = Box<dyn 'a + FnMut(Option<f64>) -> f64>;

const SHIFT_WHEEL_STEP_MULTIPLIER: f64 = 5.0;
const WHEEL_SLIDER_HOVER_BLOCK_ID: &str = "wheel_slider_hover_block";

#[derive(Clone, Copy, Debug)]
struct WheelSliderHoverBlock {
    frame_nr: u64,
    rect: Rect,
}

pub struct WheelSlider<'a> {
    get_set_value: GetSetValue<'a>,
    range: RangeInclusive<f64>,
    clamping: SliderClamping,
    smart_aim: bool,
    show_value: bool,
    orientation: SliderOrientation,
    prefix: String,
    suffix: String,
    text: WidgetText,
    step: Option<f64>,
    wheel_step: Option<f64>,
    drag_value_speed: Option<f64>,
    min_decimals: usize,
    max_decimals: Option<usize>,
    custom_formatter: Option<NumFormatter<'a>>,
    custom_parser: Option<NumParser<'a>>,
    trailing_fill: Option<bool>,
    handle_shape: Option<HandleShape>,
    update_while_editing: bool,
    logarithmic: bool,
    smallest_positive: f64,
    largest_finite: f64,
    integral: bool,
}

impl<'a> WheelSlider<'a> {
    pub fn new<Num: emath::Numeric>(value: &'a mut Num, range: RangeInclusive<Num>) -> Self {
        let range_f64 = range.start().to_f64()..=range.end().to_f64();
        let mut slf = Self::from_get_set(
            range_f64,
            move |v: Option<f64>| {
                if let Some(v) = v {
                    *value = Num::from_f64(v);
                }
                value.to_f64()
            },
            Num::INTEGRAL,
        );

        if Num::INTEGRAL {
            slf = slf.integer();
        }

        slf
    }

    pub fn from_get_set(
        range: RangeInclusive<f64>,
        get_set_value: impl 'a + FnMut(Option<f64>) -> f64,
        integral: bool,
    ) -> Self {
        Self {
            get_set_value: Box::new(get_set_value),
            range,
            clamping: SliderClamping::default(),
            smart_aim: true,
            show_value: true,
            orientation: SliderOrientation::Horizontal,
            prefix: String::new(),
            suffix: String::new(),
            text: WidgetText::default(),
            step: None,
            wheel_step: None,
            drag_value_speed: None,
            min_decimals: 0,
            max_decimals: None,
            custom_formatter: None,
            custom_parser: None,
            trailing_fill: None,
            handle_shape: None,
            update_while_editing: true,
            logarithmic: false,
            smallest_positive: if integral { 1.0 } else { 1e-6 },
            largest_finite: f64::INFINITY,
            integral,
        }
    }

    #[inline]
    pub fn show_value(mut self, show_value: bool) -> Self {
        self.show_value = show_value;
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
    pub fn text(mut self, text: impl Into<WidgetText>) -> Self {
        self.text = text.into();
        self
    }

    #[inline]
    pub fn text_color(mut self, text_color: Color32) -> Self {
        self.text = self.text.color(text_color);
        self
    }

    #[inline]
    pub fn orientation(mut self, orientation: SliderOrientation) -> Self {
        self.orientation = orientation;
        self
    }

    #[inline]
    pub fn vertical(mut self) -> Self {
        self.orientation = SliderOrientation::Vertical;
        self
    }

    #[inline]
    pub fn logarithmic(mut self, logarithmic: bool) -> Self {
        self.logarithmic = logarithmic;
        self
    }

    #[inline]
    pub fn smallest_positive(mut self, smallest_positive: f64) -> Self {
        self.smallest_positive = smallest_positive;
        self
    }

    #[inline]
    pub fn largest_finite(mut self, largest_finite: f64) -> Self {
        self.largest_finite = largest_finite;
        self
    }

    #[inline]
    pub fn clamping(mut self, clamping: SliderClamping) -> Self {
        self.clamping = clamping;
        self
    }

    #[inline]
    pub fn smart_aim(mut self, smart_aim: bool) -> Self {
        self.smart_aim = smart_aim;
        self
    }

    #[inline]
    pub fn step_by(mut self, step: f64) -> Self {
        self.step = Some(step);
        self
    }

    #[inline]
    pub fn wheel_step(mut self, wheel_step: impl Into<f64>) -> Self {
        self.wheel_step = Some(wheel_step.into());
        self
    }

    #[inline]
    pub fn drag_value_speed(mut self, drag_value_speed: f64) -> Self {
        self.drag_value_speed = Some(drag_value_speed);
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
    pub fn trailing_fill(mut self, trailing_fill: bool) -> Self {
        self.trailing_fill = Some(trailing_fill);
        self
    }

    #[inline]
    pub fn handle_shape(mut self, handle_shape: HandleShape) -> Self {
        self.handle_shape = Some(handle_shape);
        self
    }

    #[inline]
    pub fn update_while_editing(mut self, update_while_editing: bool) -> Self {
        self.update_while_editing = update_while_editing;
        self
    }

    #[inline]
    pub fn integer(mut self) -> Self {
        self.max_decimals = Some(0);
        self.smallest_positive = 1.0;
        self.integral = true;
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

    pub fn pointer_recently_over_any(ctx: &egui::Context) -> bool {
        let Some(block) = ctx.data(|data| {
            data.get_temp::<WheelSliderHoverBlock>(Id::new(WHEEL_SLIDER_HOVER_BLOCK_ID))
        }) else {
            return false;
        };

        if ctx.cumulative_frame_nr().saturating_sub(block.frame_nr) > 1 {
            return false;
        }

        ctx.input(|input| {
            input
                .pointer
                .hover_pos()
                .or_else(|| input.pointer.interact_pos())
                .is_some_and(|pos| block.rect.contains(pos))
        })
    }

    fn effective_wheel_step(&self) -> f64 {
        if let Some(wheel_step) = self.wheel_step {
            return wheel_step.abs();
        }
        if let Some(step) = self.step {
            return step.abs().max(f64::EPSILON);
        }
        if self.integral {
            return 1.0;
        }
        if let Some(max_decimals) = self.max_decimals {
            return 10f64.powi(-(max_decimals as i32));
        }
        let span = (*self.range.end() - *self.range.start()).abs();
        if span.is_finite() && span > f64::EPSILON {
            return (span / 100.0).max(f64::EPSILON);
        }
        1.0
    }

    fn build_slider(&mut self) -> egui::Slider<'_> {
        let mut widget = egui::Slider::from_get_set(self.range.clone(), &mut self.get_set_value)
            .clamping(self.clamping)
            .smart_aim(self.smart_aim)
            .show_value(self.show_value)
            .orientation(self.orientation)
            .prefix(self.prefix.as_str())
            .suffix(self.suffix.as_str())
            .text(self.text.clone())
            .logarithmic(self.logarithmic)
            .smallest_positive(self.smallest_positive)
            .largest_finite(self.largest_finite)
            .min_decimals(self.min_decimals)
            .max_decimals_opt(self.max_decimals)
            .update_while_editing(self.update_while_editing);

        if let Some(step) = self.step {
            widget = widget.step_by(step);
        }
        if let Some(speed) = self.drag_value_speed {
            widget = widget.drag_value_speed(speed);
        }
        if let Some(trailing_fill) = self.trailing_fill {
            widget = widget.trailing_fill(trailing_fill);
        }
        if let Some(handle_shape) = self.handle_shape {
            widget = widget.handle_shape(handle_shape);
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

impl Widget for WheelSlider<'_> {
    fn ui(mut self, ui: &mut Ui) -> Response {
        let pointer_blocked_by_combo_popup = combo_popup_blocks_pointer(ui.ctx());
        let mut response = if pointer_blocked_by_combo_popup {
            ui.scope(|ui| {
                let inactive = ui.visuals().widgets.inactive;
                ui.style_mut().visuals.widgets.hovered = inactive;
                ui.style_mut().visuals.widgets.active = inactive;
                ui.add(self.build_slider())
            })
            .inner
        } else {
            ui.add(self.build_slider())
        };
        let pointer_over_slider = !pointer_blocked_by_combo_popup
            && !combo_popup_open(ui.ctx())
            && pointer_over_response_rect(&response);
        if pointer_over_slider && !response.hovered() {
            response = response.highlight();
        }
        publish_pointer_hover_state(ui.ctx(), &response, pointer_over_slider);
        let wheel_step = self.effective_wheel_step();

        if apply_hovered_wheel_delta(
            ui,
            &response,
            pointer_over_slider,
            &mut self.get_set_value,
            &self.range,
            wheel_step,
        ) {
            response.mark_changed();
        }

        response
    }
}

fn publish_pointer_hover_state(
    ctx: &egui::Context,
    response: &Response,
    pointer_over_slider: bool,
) {
    if !pointer_over_slider {
        return;
    }

    let frame_nr = ctx.cumulative_frame_nr();
    ctx.data_mut(|data| {
        data.insert_temp(
            Id::new(WHEEL_SLIDER_HOVER_BLOCK_ID),
            WheelSliderHoverBlock {
                frame_nr,
                rect: response.rect,
            },
        );
    });
}

fn pointer_over_response_rect(response: &Response) -> bool {
    // Respect element overlap (z-order): rely only on egui's occlusion-aware
    // hit-test. `contains_pointer()` is derived from the frame hit-test, which
    // drops widgets covered by a higher layer, so a combo popup, modal, or
    // tutorial overlay above the slider correctly suppresses its hover/wheel.
    // Never fall back to the raw pointer position (`hover_pos`) against the
    // unclipped rect — that would react through anything drawn on top.
    response.hovered() || response.contains_pointer()
}

fn apply_hovered_wheel_delta(
    ui: &Ui,
    response: &Response,
    pointer_over_slider: bool,
    get_set_value: &mut GetSetValue<'_>,
    range: &RangeInclusive<f64>,
    step_size: f64,
) -> bool {
    if combo_popup_blocks_pointer(ui.ctx()) || combo_popup_open(ui.ctx()) {
        return false;
    }
    if !pointer_over_slider && !response.has_focus() {
        return false;
    }

    let (raw_wheel_events, smooth_scroll_delta, shift_pressed) = ui.ctx().input(|input| {
        (
            raw_wheel_events_delta(input),
            input.smooth_scroll_delta,
            input.modifiers.shift,
        )
    });

    let raw_wheel_delta = axis_wheel_delta(raw_wheel_events);
    let smooth_wheel_delta = axis_wheel_delta(smooth_scroll_delta);
    if raw_wheel_delta.abs() <= f32::EPSILON && smooth_wheel_delta.abs() <= f32::EPSILON {
        return false;
    }

    let mut changed = false;
    if raw_wheel_delta.abs() > f32::EPSILON {
        let prev = get(get_set_value);
        let step_multiplier = if shift_pressed {
            SHIFT_WHEEL_STEP_MULTIPLIER
        } else {
            1.0
        };
        let next = (prev
            + f64::from(wheel_direction(raw_wheel_delta)) * step_size * step_multiplier)
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
