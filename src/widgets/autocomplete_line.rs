/*
FILE HEADER (widgets/autocomplete_line.rs)
- Назначение: переиспользуемый stateful-виджет однострочного ввода с автодополнением.
- Ключевые сущности:
  - `AutocompleteLine`: хранит `Id`, лимит подсказок, индекс текущей подсветки и запрос фильтрации.
  - `AutocompleteLineResponse`: результат кадра (изменение текста/подтверждение/видимость popup).
- Ключевые методы:
  - `new`: создание виджета с явным id-source.
  - `set_max_suggestions`: настройка лимита подсказок (минимум 1).
  - `draw`: рендер поля ввода + popup подсказок, browser-like inline-дополнение и hotkeys (`Tab/→/Esc/Enter`).
*/
#![allow(dead_code)]

use eframe::egui;
use egui::text_edit::TextEditState;
use egui::{Id, Key};

const DEFAULT_MAX_SUGGESTIONS: usize = 8;

#[derive(Debug, Clone, Default)]
pub struct AutocompleteLineResponse {
    pub changed: bool,
    pub submitted: bool,
    pub suggestion_applied: bool,
    pub popup_open: bool,
    pub selected_suggestion: Option<String>,
}

pub struct AutocompleteLine {
    id: Id,
    max_suggestions: usize,
    hint_text: String,
    highlighted_idx: Option<usize>,
    keep_popup_open: bool,
    filter_query: String,
    last_query: String,
}

impl AutocompleteLine {
    pub fn new(id_source: impl std::hash::Hash) -> Self {
        Self {
            id: Id::new(id_source),
            max_suggestions: DEFAULT_MAX_SUGGESTIONS,
            hint_text: String::new(),
            highlighted_idx: None,
            keep_popup_open: false,
            filter_query: String::new(),
            last_query: String::new(),
        }
    }

    pub fn set_max_suggestions(&mut self, max_suggestions: usize) {
        self.max_suggestions = max_suggestions.max(1);
    }

    pub fn with_max_suggestions(mut self, max_suggestions: usize) -> Self {
        self.set_max_suggestions(max_suggestions);
        self
    }

    pub fn set_hint_text(&mut self, hint_text: impl Into<String>) {
        self.hint_text = hint_text.into();
    }

    pub fn with_hint_text(mut self, hint_text: impl Into<String>) -> Self {
        self.set_hint_text(hint_text);
        self
    }

    pub fn draw<S: AsRef<str>>(
        &mut self,
        ui: &mut egui::Ui,
        value: &mut String,
        options: &[S],
    ) -> AutocompleteLineResponse {
        let mut out = AutocompleteLineResponse::default();
        let text_id = self.id.with("text");
        let popup_id = self.id.with("popup");
        let value_before_edit = value.clone();

        let mut text_edit = egui::TextEdit::singleline(value).id(text_id);
        if !self.hint_text.is_empty() {
            text_edit = text_edit.hint_text(self.hint_text.as_str());
        }
        let mut text_output = text_edit.show(ui);
        let text_response = &text_output.response;

        if *value != self.last_query {
            self.highlighted_idx = None;
            self.last_query.clone_from(value);
        }
        let text_changed = text_response.changed();
        let deletion_like_change =
            text_changed && is_deletion_like_change(ui, &value_before_edit, value);
        let caret_at_end_without_selection =
            text_output.state.cursor.char_range().is_some_and(|range| {
                range.is_empty() && range.primary.index == value.chars().count()
            });
        if text_changed {
            self.filter_query.clone_from(value);
        }
        out.changed = text_changed;

        let query_for_suggestions = if self.filter_query.is_empty() {
            value.as_str()
        } else {
            self.filter_query.as_str()
        };
        let suggestions = collect_suggestions(query_for_suggestions, options, self.max_suggestions);
        if text_changed
            && text_response.has_focus()
            && !value.is_empty()
            && !deletion_like_change
            && caret_at_end_without_selection
            && let Some(first_prefix) = first_prefix_suggestion(query_for_suggestions, &suggestions)
            && first_prefix.len() > value.len()
        {
            let typed_chars = value.chars().count();
            *value = first_prefix.to_owned();
            let full_chars = value.chars().count();
            text_output
                .state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(typed_chars),
                    egui::text::CCursor::new(full_chars),
                )));
            text_output.state.clone().store(ui.ctx(), text_id);
            self.last_query.clone_from(value);
            out.changed = true;
        }

        let popup_open =
            (text_response.has_focus() || self.keep_popup_open) && !suggestions.is_empty();
        out.popup_open = popup_open;

        if !popup_open {
            self.highlighted_idx = None;
            self.keep_popup_open = false;
        } else {
            let inline_completion_active = is_inline_completion_active(value, &self.filter_query);
            if ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                let next = match self.highlighted_idx {
                    Some(idx) if idx + 1 < suggestions.len() => idx + 1,
                    _ => 0,
                };
                self.highlighted_idx = Some(next);
            }
            if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                let prev = match self.highlighted_idx {
                    Some(idx) if idx > 0 => idx - 1,
                    Some(_) => suggestions.len().saturating_sub(1),
                    None => suggestions.len().saturating_sub(1),
                };
                self.highlighted_idx = Some(prev);
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                if restore_typed_query(
                    value,
                    &self.filter_query,
                    &mut text_output.state,
                    ui.ctx(),
                    text_id,
                    &mut self.last_query,
                ) {
                    self.highlighted_idx = None;
                    out.changed = true;
                } else if self.highlighted_idx.take().is_none() {
                    self.keep_popup_open = false;
                }
            }

            let tab_accept_pressed = if self.highlighted_idx.is_some() || inline_completion_active {
                ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Tab))
            } else {
                false
            };
            let right_accept_pressed = if inline_completion_active {
                ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowRight))
            } else {
                false
            };
            if tab_accept_pressed || right_accept_pressed {
                if let Some(idx) = self.highlighted_idx {
                    if let Some(suggestion) = suggestions.get(idx).copied() {
                        apply_suggestion(
                            value,
                            suggestion,
                            &mut out,
                            &mut self.last_query,
                            &mut self.highlighted_idx,
                            false,
                        );
                    }
                } else if inline_completion_active {
                    accept_inline_completion(
                        value,
                        &mut self.filter_query,
                        &mut text_output.state,
                        ui.ctx(),
                        text_id,
                        &mut out,
                        &mut self.last_query,
                    );
                }
                ui.memory_mut(|mem| mem.request_focus(text_id));
            }

            if ui.input(|i| i.key_pressed(Key::Enter)) {
                if let Some(idx) = self.highlighted_idx {
                    if let Some(suggestion) = suggestions.get(idx).copied() {
                        apply_suggestion(
                            value,
                            suggestion,
                            &mut out,
                            &mut self.last_query,
                            &mut self.highlighted_idx,
                            true,
                        );
                    }
                } else if inline_completion_active {
                    accept_inline_completion(
                        value,
                        &mut self.filter_query,
                        &mut text_output.state,
                        ui.ctx(),
                        text_id,
                        &mut out,
                        &mut self.last_query,
                    );
                    out.submitted = true;
                } else {
                    out.submitted = true;
                }
            }

            let popup_pos = egui::pos2(text_response.rect.left(), text_response.rect.bottom());
            let popup_response = egui::Area::new(popup_id)
                .order(egui::Order::Foreground)
                .fixed_pos(popup_pos)
                .show(ui.ctx(), |ui| {
                    ui.set_min_width(text_response.rect.width());
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(text_response.rect.width());
                        for (idx, suggestion) in suggestions.iter().enumerate() {
                            let selected = self.highlighted_idx == Some(idx);
                            let response = ui.selectable_label(selected, *suggestion);
                            if response.hovered() {
                                self.highlighted_idx = Some(idx);
                            }
                            if response.clicked() {
                                apply_suggestion(
                                    value,
                                    suggestion,
                                    &mut out,
                                    &mut self.last_query,
                                    &mut self.highlighted_idx,
                                    true,
                                );
                                self.filter_query.clone_from(value);
                                ui.memory_mut(|mem| mem.request_focus(text_id));
                                self.keep_popup_open = false;
                            }
                        }
                    });
                });
            self.keep_popup_open =
                popup_response.response.contains_pointer() || popup_response.response.hovered();
        }

        if text_response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            out.submitted = true;
        }

        if !text_response.has_focus() {
            self.filter_query.clear();
        }

        out
    }
}

fn collect_suggestions<'a, S: AsRef<str>>(
    query: &str,
    options: &'a [S],
    max_suggestions: usize,
) -> Vec<&'a str> {
    if query.trim().is_empty() || options.is_empty() || max_suggestions == 0 {
        return Vec::new();
    }

    let needle = query.to_lowercase();
    let mut out = Vec::with_capacity(max_suggestions.min(options.len()));

    for option in options {
        let option = option.as_ref();
        if option.to_lowercase().starts_with(&needle) {
            out.push(option);
            if out.len() >= max_suggestions {
                return out;
            }
        }
    }
    for option in options {
        let option = option.as_ref();
        if option.to_lowercase().contains(&needle) && !out.contains(&option) {
            out.push(option);
            if out.len() >= max_suggestions {
                break;
            }
        }
    }

    out
}

fn first_prefix_suggestion<'a>(query: &str, suggestions: &[&'a str]) -> Option<&'a str> {
    let needle = query.to_lowercase();
    suggestions
        .iter()
        .copied()
        .find(|item| item.to_lowercase().starts_with(&needle))
}

fn is_deletion_like_change(ui: &egui::Ui, before_edit: &str, after_edit: &str) -> bool {
    let before_len = before_edit.chars().count();
    let after_len = after_edit.chars().count();
    if after_len < before_len {
        return true;
    }

    ui.input(|i| {
        i.key_pressed(Key::Backspace)
            || i.key_pressed(Key::Delete)
            || i.events.iter().any(|ev| matches!(ev, egui::Event::Cut))
    })
}

fn is_inline_completion_active(value: &str, typed_query: &str) -> bool {
    if typed_query.is_empty() {
        return false;
    }
    let typed_chars = typed_query.chars().count();
    let value_chars = value.chars().count();
    typed_chars < value_chars
        && value
            .to_lowercase()
            .starts_with(&typed_query.to_lowercase())
}

fn restore_typed_query(
    value: &mut String,
    typed_query: &str,
    state: &mut TextEditState,
    ctx: &egui::Context,
    text_id: Id,
    last_query: &mut String,
) -> bool {
    if *value == typed_query {
        return false;
    }
    value.clear();
    value.push_str(typed_query);
    let end = value.chars().count();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(end),
            egui::text::CCursor::new(end),
        )));
    state.clone().store(ctx, text_id);
    last_query.clear();
    last_query.push_str(value);
    true
}

fn accept_inline_completion(
    value: &mut str,
    typed_query: &mut String,
    state: &mut TextEditState,
    ctx: &egui::Context,
    text_id: Id,
    out: &mut AutocompleteLineResponse,
    last_query: &mut String,
) {
    typed_query.clear();
    typed_query.push_str(value);
    let end = value.chars().count();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(end),
            egui::text::CCursor::new(end),
        )));
    state.clone().store(ctx, text_id);
    last_query.clear();
    last_query.push_str(value);
    out.suggestion_applied = true;
    out.selected_suggestion = Some(value.to_owned());
}

fn apply_suggestion(
    value: &mut String,
    suggestion: &str,
    out: &mut AutocompleteLineResponse,
    last_query: &mut String,
    highlighted_idx: &mut Option<usize>,
    submitted: bool,
) {
    value.clear();
    value.push_str(suggestion);
    last_query.clear();
    last_query.push_str(suggestion);
    *highlighted_idx = None;
    out.changed = true;
    out.submitted = submitted;
    out.suggestion_applied = true;
    out.selected_suggestion = Some(suggestion.to_owned());
}
