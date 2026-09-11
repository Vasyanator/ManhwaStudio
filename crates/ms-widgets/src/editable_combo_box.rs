/*
FILE HEADER (widgets/editable_combo_box.rs)
- Назначение: переиспользуемый stateful-виджет редактируемого комбобокса,
  который совмещает `TextEdit` и popup со списком готовых значений.
- Ключевые сущности:
  - `EditableComboBox`: хранит `Id`, текст подсказки и параметры popup.
  - `EditableComboBoxResponse`: результат кадра с общим `Response`, флагами
    изменения/submit и выбранным значением из списка.
- Ключевые методы:
  - `new`: создание виджета с явным id-source.
  - `with_hint_text`, `with_popup_max_height`, `with_desired_text_width`:
    настройка внешнего вида.
  - `draw`: рендер строки ввода, кнопки раскрытия и popup со списком.
- Особенности:
  - значение можно как ввести вручную, так и выбрать мышью из списка;
  - popup закрывается по Esc и по клику вне виджета;
  - при выборе варианта значение пишется в ту же строку без отдельного state.
*/
#![allow(dead_code)]

use eframe::egui;
use egui::style::StyleModifier;
use egui::{Id, Key, Response};

const DEFAULT_POPUP_MAX_HEIGHT: f32 = 220.0;
const MIN_TEXT_WIDTH: f32 = 48.0;

#[derive(Debug)]
pub struct EditableComboBoxResponse {
    pub response: Response,
    pub changed: bool,
    pub submitted: bool,
    pub popup_open: bool,
    pub selected_option: Option<String>,
}

#[derive(Debug)]
pub struct EditableComboBox {
    id: Id,
    hint_text: String,
    popup_open: bool,
    popup_max_height: f32,
    desired_text_width: Option<f32>,
    popup_style: StyleModifier,
}

impl EditableComboBox {
    pub fn new(id_source: impl std::hash::Hash + std::fmt::Debug) -> Self {
        Self {
            id: Id::new(id_source),
            hint_text: String::new(),
            popup_open: false,
            popup_max_height: DEFAULT_POPUP_MAX_HEIGHT,
            desired_text_width: None,
            popup_style: StyleModifier::default(),
        }
    }

    pub fn set_hint_text(&mut self, hint_text: impl Into<String>) {
        self.hint_text = hint_text.into();
    }

    pub fn with_hint_text(mut self, hint_text: impl Into<String>) -> Self {
        self.set_hint_text(hint_text);
        self
    }

    pub fn set_popup_max_height(&mut self, popup_max_height: f32) {
        self.popup_max_height = popup_max_height.max(ui_min_interact_height());
    }

    pub fn with_popup_max_height(mut self, popup_max_height: f32) -> Self {
        self.set_popup_max_height(popup_max_height);
        self
    }

    pub fn set_desired_text_width(&mut self, desired_text_width: f32) {
        self.desired_text_width = Some(desired_text_width.max(MIN_TEXT_WIDTH));
    }

    pub fn with_desired_text_width(mut self, desired_text_width: f32) -> Self {
        self.set_desired_text_width(desired_text_width);
        self
    }

    pub fn set_popup_style(&mut self, popup_style: StyleModifier) {
        self.popup_style = popup_style;
    }

    pub fn with_popup_style(mut self, popup_style: StyleModifier) -> Self {
        self.set_popup_style(popup_style);
        self
    }

    pub fn draw<S: AsRef<str>>(
        &mut self,
        ui: &mut egui::Ui,
        value: &mut String,
        options: &[S],
    ) -> EditableComboBoxResponse {
        let text_id = self.id.with("text");
        let button_id = self.id.with("button");
        let popup_id = self.id.with("popup");

        let mut changed = false;
        let mut submitted = false;
        let mut selected_option = None;
        let button_width = ui.spacing().interact_size.y;

        let combined_response = ui
            .horizontal(|ui| {
                let text_width = self.desired_text_width.unwrap_or_else(|| {
                    let spacing = ui.spacing().item_spacing.x;
                    (ui.available_width() - button_width - spacing).max(MIN_TEXT_WIDTH)
                });

                let mut text_edit = egui::TextEdit::singleline(value).id(text_id);
                if !self.hint_text.is_empty() {
                    text_edit = text_edit.hint_text(self.hint_text.as_str());
                }
                text_edit = text_edit.background_color(ui.visuals().widgets.inactive.bg_fill);

                let text_response =
                    ui.add_sized([text_width, ui.spacing().interact_size.y], text_edit);
                let button_response = ui.push_id(button_id, |ui| {
                    ui.add_sized(
                        [button_width, text_response.rect.height()],
                        egui::Button::new("v"),
                    )
                });
                (text_response, button_response.inner)
            })
            .inner;

        let (mut text_response, button_response) = combined_response;
        let button_clicked = button_response.clicked();
        let mut response = text_response.union(button_response);

        if text_response.changed() {
            changed = true;
        }
        if text_response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
            submitted = true;
        }
        if text_response.has_focus() && ui.input(|input| input.key_pressed(Key::ArrowDown)) {
            self.popup_open = true;
        }
        if button_clicked {
            self.popup_open = !self.popup_open;
        }

        let base_rect = response.rect;
        let mut popup_rect = None;
        if self.popup_open {
            let popup_pos = egui::pos2(base_rect.left(), base_rect.bottom());
            let mut popup_style = ui.style().as_ref().clone();
            self.popup_style.apply(&mut popup_style);
            let popup_response = egui::Area::new(popup_id)
                .order(egui::Order::Foreground)
                .fixed_pos(popup_pos)
                .show(ui.ctx(), |ui| {
                    ui.scope_builder(egui::UiBuilder::new().style(popup_style), |ui| {
                        ui.set_min_width(base_rect.width());
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_min_width(base_rect.width());
                            egui::ScrollArea::vertical()
                                .max_width(base_rect.width())
                                .min_scrolled_width(base_rect.width())
                                .auto_shrink([false, true])
                                .max_height(self.popup_max_height)
                                .show(ui, |ui| {
                                    if options.is_empty() {
                                        ui.label("No options");
                                    } else {
                                        for option in options {
                                            let option_text = option.as_ref();
                                            let option_clicked = ui
                                                .selectable_label(
                                                    option_text == value.as_str(),
                                                    option_text,
                                                )
                                                .clicked();
                                            if option_clicked {
                                                if value != option_text {
                                                    value.clear();
                                                    value.push_str(option_text);
                                                    changed = true;
                                                }
                                                selected_option = Some(option_text.to_owned());
                                                self.popup_open = false;
                                                ui.memory_mut(|mem| mem.request_focus(text_id));
                                            }
                                        }
                                    }
                                });
                        });
                    })
                });
            popup_rect = Some(popup_response.response.rect);
        }

        if self.popup_open && ui.input(|input| input.key_pressed(Key::Escape)) {
            self.popup_open = false;
            ui.memory_mut(|mem| mem.request_focus(text_id));
        }

        if self.popup_open
            && ui.input(|input| {
                input.pointer.any_pressed()
                    && input.pointer.interact_pos().is_some_and(|pos| {
                        !base_rect.contains(pos)
                            && !popup_rect.is_some_and(|rect| rect.contains(pos))
                    })
            })
        {
            self.popup_open = false;
        }

        if changed {
            text_response.mark_changed();
            response.mark_changed();
        }

        EditableComboBoxResponse {
            response,
            changed,
            submitted,
            popup_open: self.popup_open,
            selected_option,
        }
    }
}

fn ui_min_interact_height() -> f32 {
    18.0
}
