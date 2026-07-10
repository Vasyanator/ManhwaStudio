/*
FILE HEADER (widgets/wheel_combo_box.rs)
- Назначение: переиспользуемый `egui::ComboBox`, который может циклически
  менять выбранный элемент колесом мыши и глушить прокрутку родителя.
- Ключевые сущности:
  - `WheelComboBox`: builder-обёртка над `ComboBox` для сценариев `show_index`.
- Ключевые методы:
  - `new`, `from_label`, `from_id_salt`: создание combobox с id/label.
  - `selected_text`, `width`, `height`, `wrap*`, `icon`, `close_behavior`,
    `popup_style`: проброс основных настроек `ComboBox`.
  - `show_index`: рендер списка по выбранному индексу и переключение колесом.
- Особенности:
  - при hover/focus и закрытом popup колесо меняет индекс на один шаг;
  - при открытом popup публикует общий wheel guard, чтобы нижние wheel-виджеты
    не реагировали на прокрутку выпадающего списка;
  - popup публикует viewport списка, чтобы нижние виджеты не подсвечивались
    и не считали себя наведёнными под раскрытым списком;
  - a discrete wheel notch is detected from the raw `Event::MouseWheel` events
    (egui 0.35 has no `raw_scroll_delta`), giving one index step per notch, and the
    smoothed scroll delta is zeroed so a parent `ScrollArea` receives no scroll.
*/
#![allow(dead_code)]

use super::wheel_input_guard::{
    combo_popup_open, publish_combo_popup_open, publish_combo_popup_rect,
};
use eframe::egui;
use egui::style::{StyleModifier, WidgetVisuals};
use egui::{
    Context, Id, InnerResponse, PopupCloseBehavior, Rect, Response, TextWrapMode, Ui, WidgetText,
};

type IconPainter = Box<dyn FnOnce(&Ui, Rect, &WidgetVisuals, bool) + 'static>;

pub struct WheelComboBox {
    id_salt: Id,
    label: Option<WidgetText>,
    selected_text: WidgetText,
    width: Option<f32>,
    height: Option<f32>,
    icon: Option<IconPainter>,
    wrap_mode: Option<TextWrapMode>,
    close_behavior: Option<PopupCloseBehavior>,
    popup_style: StyleModifier,
}

pub struct WheelComboBoxUiResponse<R> {
    pub inner: InnerResponse<Option<R>>,
    pub wheel_steps: Option<i32>,
}

impl WheelComboBox {
    pub fn new(
        id_salt: impl std::hash::Hash + std::fmt::Debug,
        label: impl Into<WidgetText>,
    ) -> Self {
        Self {
            id_salt: Id::new(id_salt),
            label: Some(label.into()),
            selected_text: WidgetText::default(),
            width: None,
            height: None,
            icon: None,
            wrap_mode: None,
            close_behavior: None,
            popup_style: StyleModifier::default(),
        }
    }

    pub fn from_label(label: impl Into<WidgetText>) -> Self {
        let label = label.into();
        Self {
            id_salt: Id::new(label.text()),
            label: Some(label),
            selected_text: WidgetText::default(),
            width: None,
            height: None,
            icon: None,
            wrap_mode: None,
            close_behavior: None,
            popup_style: StyleModifier::default(),
        }
    }

    pub fn from_id_salt(id_salt: impl std::hash::Hash + std::fmt::Debug) -> Self {
        Self {
            id_salt: Id::new(id_salt),
            label: None,
            selected_text: WidgetText::default(),
            width: None,
            height: None,
            icon: None,
            wrap_mode: None,
            close_behavior: None,
            popup_style: StyleModifier::default(),
        }
    }

    /// Overrides the widget's persistent id source with a stable salt, keeping the
    /// visible label.
    ///
    /// `from_label` seeds the id from the label text, so a localized label makes the
    /// id language-dependent and a live language switch resets popup state
    /// (`docs/i18n_exclusions.md` §C). Chain `.id_salt("stable_key")` after
    /// `from_label(t!("…"))` to keep the id language-independent.
    #[inline]
    pub fn id_salt(mut self, id_salt: impl std::hash::Hash + std::fmt::Debug) -> Self {
        self.id_salt = Id::new(id_salt);
        self
    }

    #[inline]
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    #[inline]
    pub fn height(mut self, height: f32) -> Self {
        self.height = Some(height);
        self
    }

    #[inline]
    pub fn selected_text(mut self, selected_text: impl Into<WidgetText>) -> Self {
        self.selected_text = selected_text.into();
        self
    }

    #[inline]
    pub fn icon(mut self, icon_fn: impl FnOnce(&Ui, Rect, &WidgetVisuals, bool) + 'static) -> Self {
        self.icon = Some(Box::new(icon_fn));
        self
    }

    #[inline]
    pub fn wrap_mode(mut self, wrap_mode: TextWrapMode) -> Self {
        self.wrap_mode = Some(wrap_mode);
        self
    }

    #[inline]
    pub fn wrap(mut self) -> Self {
        self.wrap_mode = Some(TextWrapMode::Wrap);
        self
    }

    #[inline]
    pub fn truncate(mut self) -> Self {
        self.wrap_mode = Some(TextWrapMode::Truncate);
        self
    }

    #[inline]
    pub fn close_behavior(mut self, close_behavior: PopupCloseBehavior) -> Self {
        self.close_behavior = Some(close_behavior);
        self
    }

    #[inline]
    pub fn popup_style(mut self, popup_style: StyleModifier) -> Self {
        self.popup_style = popup_style;
        self
    }

    pub fn show_ui<R>(
        self,
        ui: &mut Ui,
        menu_contents: impl FnOnce(&mut Ui) -> R,
    ) -> InnerResponse<Option<R>> {
        let button_id = ui.make_persistent_id(self.id_salt);
        let combo = self.into_egui_combo(button_id);
        let inner = combo.show_ui(ui, |ui| {
            publish_combo_popup_rect(ui.ctx(), ui.clip_rect());
            menu_contents(ui)
        });

        let popup_open = egui::ComboBox::is_open(ui.ctx(), button_id);
        if popup_open {
            publish_combo_popup_open(ui.ctx());
        } else {
            suppress_wheel_scroll_if_hovered(ui.ctx(), &inner.response);
        }
        inner
    }

    pub fn show_ui_with_wheel<R>(
        self,
        ui: &mut Ui,
        menu_contents: impl FnOnce(&mut Ui) -> R,
    ) -> WheelComboBoxUiResponse<R> {
        let button_id = ui.make_persistent_id(self.id_salt);
        let combo = self.into_egui_combo(button_id);
        let inner = combo.show_ui(ui, |ui| {
            publish_combo_popup_rect(ui.ctx(), ui.clip_rect());
            menu_contents(ui)
        });

        let popup_open = egui::ComboBox::is_open(ui.ctx(), button_id);
        if popup_open {
            publish_combo_popup_open(ui.ctx());
        }
        let wheel_steps = if popup_open {
            None
        } else {
            wheel_steps_if_hovered(ui.ctx(), &inner.response)
        };
        WheelComboBoxUiResponse { inner, wheel_steps }
    }

    pub fn show_index<Text: Into<WidgetText>>(
        self,
        ui: &mut Ui,
        selected: &mut usize,
        len: usize,
        get: impl Fn(usize) -> Text,
    ) -> Response {
        let selected_text: WidgetText = if len == 0 {
            self.selected_text.clone()
        } else {
            get((*selected).min(len.saturating_sub(1))).into()
        };
        let slf = self.selected_text(selected_text);

        let mut changed = false;
        let combo = slf.show_ui_with_wheel(ui, |ui| {
            for i in 0..len {
                if ui.selectable_label(i == *selected, get(i)).clicked() {
                    *selected = i;
                    changed = true;
                }
            }
        });
        if let Some(steps) = combo.wheel_steps {
            let prev = *selected;
            *selected = cycle_wrapped_index(prev, len, steps);
            changed |= *selected != prev;
        }
        let mut response = combo.inner.response;

        if changed {
            response.mark_changed();
        }
        response
    }

    fn into_egui_combo(self, button_id: Id) -> egui::ComboBox {
        let mut combo = if let Some(label) = self.label {
            egui::ComboBox::new(button_id, label)
        } else {
            egui::ComboBox::from_id_salt(button_id)
        }
        .selected_text(self.selected_text);

        if let Some(width) = self.width {
            combo = combo.width(width);
        }
        if let Some(height) = self.height {
            combo = combo.height(height);
        }
        if let Some(icon) = self.icon {
            combo = combo.icon(icon);
        }
        if let Some(wrap_mode) = self.wrap_mode {
            combo = combo.wrap_mode(wrap_mode);
        }
        if let Some(close_behavior) = self.close_behavior {
            combo = combo.close_behavior(close_behavior);
        }
        combo.popup_style(self.popup_style)
    }
}

fn suppress_wheel_scroll_if_hovered(ctx: &Context, response: &Response) {
    let _ = wheel_steps_if_hovered(ctx, response);
}

fn cycle_wrapped_index(index: usize, len: usize, steps: i32) -> usize {
    if len == 0 || steps == 0 {
        return index;
    }

    let shift = (steps.unsigned_abs() as usize) % len;
    if steps > 0 {
        (index + shift) % len
    } else {
        (index + len - shift) % len
    }
}

fn wheel_steps_if_hovered(ctx: &Context, response: &Response) -> Option<i32> {
    if combo_popup_open(ctx) {
        return None;
    }
    if !response.hovered() && !response.has_focus() {
        return None;
    }

    let (raw_wheel_events, smooth_scroll_delta) =
        ctx.input(|input| (raw_wheel_events_delta(input), input.smooth_scroll_delta));
    let raw_wheel_delta = axis_wheel_delta(raw_wheel_events);
    let smooth_wheel_delta = axis_wheel_delta(smooth_scroll_delta);
    if raw_wheel_delta.abs() <= f32::EPSILON && smooth_wheel_delta.abs() <= f32::EPSILON {
        return None;
    }

    consume_wheel_scroll_delta(ctx);
    if smooth_wheel_delta.abs() > f32::EPSILON {
        ctx.request_repaint();
    }

    if raw_wheel_delta.abs() <= f32::EPSILON {
        None
    } else {
        Some(-wheel_direction(raw_wheel_delta))
    }
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

fn consume_wheel_scroll_delta(ctx: &Context) {
    ctx.input_mut(|input| {
        input.smooth_scroll_delta = egui::Vec2::ZERO;
    });
}
