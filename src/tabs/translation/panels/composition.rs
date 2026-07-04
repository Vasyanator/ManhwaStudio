use crate::project::{Bubble, ProjectData};
use crate::tabs::translation::panels::bubbles::{
    bubble_extra_bool, bubble_extra_i32, bubble_extra_string,
};
use crate::widgets::WheelSpinBox;
use eframe::egui;
use minijinja::{AutoEscape, Environment, context};
#[cfg(not(target_arch = "wasm32"))]
use rfd::FileDialog;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const LIMIT_MIN: usize = 100;
const LIMIT_MAX: usize = 100_000;
const LIMIT_DEFAULT: usize = 700;
const NO_ITEMS_TEXT: &str = "(нет реплик для компоновки)";
const EMPTY_TEMPLATE_TEXT: &str = "(шаблон MiniJinja пуст)";
const UNKNOWN_CHARACTER: &str = "(неизвестный персонаж)";
const NARRATOR_CHARACTER: &str = "(рассказчик)";
// Only used by the native file-save picker (`select_export_path`), which is
// compiled out on wasm.
#[cfg(not(target_arch = "wasm32"))]
const DEFAULT_EXPORT_NAME: &str = "composition_export";
const COMPOSED_TEXT_ROWS: usize = 12;
const TEMPLATE_ROWS: usize = 6;
const VARS_ROWS: usize = 6;
const VARS_INFO_TEXT: &str = "bubbles - список всех пузырей\n\n\
Поля bubble:\n\
id - уникальный ID пузыря\n\
img_idx - индекс страницы\n\
img_u - X-координата пузыря на странице (0..1)\n\
img_v - Y-координата пузыря на странице (0..1)\n\
side - сторона страницы (left/right)\n\
text - перевод пузыря\n\
original_text - исходный текст\n\
translation_status - статус перевода\n\
is_known_character - известный персонаж (true/false)\n\
character_name - имя персонажа\n\
clarification - уточнение\n\
bubble_order - порядок реплики внутри страницы\n\n\
Пример:\n\
{% for bubble in bubbles %}\n\
{{ bubble.id }} | {{ bubble.img_idx }} | {{ bubble.bubble_order }} | {{ bubble.original_text }} | {{ bubble.text }}\n\
{% endfor %}";

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum CompositionSortMethod {
    #[default]
    Height,
    Order,
}

impl CompositionSortMethod {
    pub fn key(self) -> &'static str {
        match self {
            CompositionSortMethod::Height => "height",
            CompositionSortMethod::Order => "order",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            CompositionSortMethod::Height => "[По высоте]",
            CompositionSortMethod::Order => "[По номеру реплики]",
        }
    }

    pub fn from_key(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("order") {
            Self::Order
        } else {
            Self::Height
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum CompositionSourceMode {
    #[default]
    Original,
    Translation,
}

impl CompositionSourceMode {
    pub fn key(self) -> &'static str {
        match self {
            CompositionSourceMode::Original => "original",
            CompositionSourceMode::Translation => "translation",
        }
    }

    pub fn from_key(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("translation") {
            Self::Translation
        } else {
            Self::Original
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompositionPanelOptions {
    pub sort_method: CompositionSortMethod,
    pub source_mode: CompositionSourceMode,
    pub ignore_translated_lines: bool,
    pub merge_same_character: bool,
    pub sep_same_character: String,
    pub sep_between: String,
    pub replica_prefix: String,
    pub nl_replace: String,
    pub nl_replace_enabled: bool,
    pub wrap_with: String,
    pub wrap_with_enabled: bool,
    pub limit: usize,
    pub limit_enabled: bool,
    pub use_character_names: bool,
    /// Include image-bubble translations in the composition. When enabled, each text area of an
    /// image bubble contributes one line `{translation}` (plus ` - {description}` when
    /// `use_character_names` is on and the description is non-empty).
    pub include_image_bubbles: bool,
    pub jinja2_enabled: bool,
    pub jinja2_template: String,
}

impl Default for CompositionPanelOptions {
    fn default() -> Self {
        Self {
            sort_method: CompositionSortMethod::Height,
            source_mode: CompositionSourceMode::Original,
            ignore_translated_lines: true,
            merge_same_character: true,
            sep_same_character: "\\n".to_string(),
            sep_between: "\\n\\n".to_string(),
            replica_prefix: String::new(),
            nl_replace: " ".to_string(),
            nl_replace_enabled: true,
            wrap_with: "``".to_string(),
            wrap_with_enabled: true,
            limit: LIMIT_DEFAULT,
            limit_enabled: true,
            use_character_names: true,
            include_image_bubbles: false,
            jinja2_enabled: false,
            jinja2_template: String::new(),
        }
    }
}

impl CompositionPanelOptions {
    pub fn normalize(&mut self) {
        self.wrap_with = normalize_wrap_with(&self.wrap_with);
        self.limit = self.limit.clamp(LIMIT_MIN, LIMIT_MAX);
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum CompositionTab {
    #[default]
    Text,
    Params,
}

#[derive(Debug, Clone)]
struct CompositionNotice {
    message: String,
    is_error: bool,
}

#[derive(Debug, Clone)]
pub struct CompositionPanelState {
    tab: CompositionTab,
    pub composed_text: String,
    notice: Option<CompositionNotice>,
}

impl Default for CompositionPanelState {
    fn default() -> Self {
        Self {
            tab: CompositionTab::Text,
            composed_text: NO_ITEMS_TEXT.to_string(),
            notice: None,
        }
    }
}

impl CompositionPanelState {
    fn set_info(&mut self, message: impl Into<String>) {
        self.notice = Some(CompositionNotice {
            message: message.into(),
            is_error: false,
        });
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.notice = Some(CompositionNotice {
            message: message.into(),
            is_error: true,
        });
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CompositionPanelActions {
    pub options_changed: bool,
    pub request_rebuild: bool,
}

pub fn draw_composition_panel(
    ui: &mut egui::Ui,
    project: &ProjectData,
    state: &mut CompositionPanelState,
    options: &mut CompositionPanelOptions,
) -> CompositionPanelActions {
    let mut actions = CompositionPanelActions::default();
    options.normalize();

    ui.horizontal(|ui| {
        if ui
            .selectable_label(state.tab == CompositionTab::Text, "Текст")
            .clicked()
        {
            state.tab = CompositionTab::Text;
        }
        if ui
            .selectable_label(state.tab == CompositionTab::Params, "Параметры")
            .clicked()
        {
            state.tab = CompositionTab::Params;
        }
    });
    ui.separator();

    match state.tab {
        CompositionTab::Text => draw_text_tab(ui, project, state, options, &mut actions),
        CompositionTab::Params => draw_params_tab(ui, state, options, &mut actions),
    }

    actions
}

pub fn compose_translation_text(
    project: &ProjectData,
    options: &CompositionPanelOptions,
) -> String {
    if options.jinja2_enabled {
        return compose_minijinja(project, options);
    }
    compose_plain(project, options)
}

pub fn normalize_wrap_with(value: &str) -> String {
    let mut chars = value.chars();
    match (chars.next(), chars.next()) {
        (Some(left), Some(right)) => format!("{left}{right}"),
        (Some(one), None) => format!("{one}{one}"),
        _ => "``".to_string(),
    }
}

fn draw_text_tab(
    ui: &mut egui::Ui,
    project: &ProjectData,
    state: &mut CompositionPanelState,
    options: &mut CompositionPanelOptions,
    actions: &mut CompositionPanelActions,
) {
    ui.horizontal(|ui| {
        ui.label("Сортировка:");
        if ui
            .button("⇆")
            .on_hover_text("Переключить режим сортировки")
            .clicked()
        {
            options.sort_method = if options.sort_method == CompositionSortMethod::Height {
                CompositionSortMethod::Order
            } else {
                CompositionSortMethod::Height
            };
            actions.options_changed = true;
            actions.request_rebuild = true;
        }
        ui.label(options.sort_method.title());
    });

    draw_readonly_big_text(
        ui,
        "composition_text_output",
        &state.composed_text,
        COMPOSED_TEXT_ROWS,
        "Скомпонованные реплики появятся здесь...",
    );

    ui.horizontal_wrapped(|ui| {
        if ui.button("Скопировать").clicked() {
            ui.ctx().copy_text(state.composed_text.clone());
            state.set_info("Текст скопирован в буфер обмена.");
        }
        if ui.button("Обновить").clicked() {
            actions.request_rebuild = true;
        }
        if ui.button("Экспорт в txt").clicked() {
            match export_txt(project, &state.composed_text) {
                Ok(Some(path)) => state.set_info(format!("TXT сохранён: {}", path.display())),
                Ok(None) => {}
                Err(err) => state.set_error(format!("Не удалось сохранить TXT: {err}")),
            }
        }
        if ui.button("Экспорт в docx").clicked() {
            match export_docx(project, &state.composed_text) {
                Ok(Some(path)) => state.set_info(format!("DOCX сохранён: {}", path.display())),
                Ok(None) => {}
                Err(err) => state.set_error(format!("Не удалось сохранить DOCX: {err}")),
            }
        }
    });

    if let Some(notice) = &state.notice {
        let color = if notice.is_error {
            egui::Color32::from_rgb(208, 84, 62)
        } else {
            egui::Color32::from_rgb(42, 168, 88)
        };
        ui.colored_label(color, &notice.message);
    }
}

fn draw_params_tab(
    ui: &mut egui::Ui,
    state: &mut CompositionPanelState,
    options: &mut CompositionPanelOptions,
    actions: &mut CompositionPanelActions,
) {
    let mut changed = false;
    let use_jinja = options.jinja2_enabled;

    ui.add_enabled_ui(!use_jinja, |ui| {
        ui.horizontal(|ui| {
            ui.label("Реплики:");
            changed |= ui
                .selectable_value(
                    &mut options.source_mode,
                    CompositionSourceMode::Original,
                    "Оригинал",
                )
                .changed();
            changed |= ui
                .selectable_value(
                    &mut options.source_mode,
                    CompositionSourceMode::Translation,
                    "Перевод",
                )
                .changed();
        });

        ui.add_enabled_ui(
            options.source_mode == CompositionSourceMode::Original,
            |ui| {
                changed |= ui
                    .checkbox(
                        &mut options.ignore_translated_lines,
                        "Игнорировать переведённые строки",
                    )
                    .changed();
            },
        );

        ui.horizontal(|ui| {
            ui.label("Замена \\n:");
            changed |= ui.checkbox(&mut options.nl_replace_enabled, "").changed();
            ui.add_enabled_ui(options.nl_replace_enabled, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut options.nl_replace)
                        .desired_width(f32::INFINITY)
                        .hint_text("пробел"),
                );
                if resp.changed() {
                    truncate_chars(&mut options.nl_replace, 8);
                    changed = true;
                }
            });
        });

        ui.horizontal(|ui| {
            ui.label("Оборачивать реплики в:");
            changed |= ui.checkbox(&mut options.wrap_with_enabled, "").changed();
            ui.add_enabled_ui(options.wrap_with_enabled, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut options.wrap_with)
                        .desired_width(f32::INFINITY)
                        .hint_text("``"),
                );
                if resp.changed() {
                    truncate_chars(&mut options.wrap_with, 2);
                    options.wrap_with = normalize_wrap_with(&options.wrap_with);
                    changed = true;
                }
            });
        });

        ui.horizontal(|ui| {
            ui.label("Префикс реплики:");
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut options.replica_prefix)
                        .desired_width(f32::INFINITY),
                )
                .changed();
        });

        ui.horizontal(|ui| {
            ui.label("Лимит символов:");
            changed |= ui.checkbox(&mut options.limit_enabled, "").changed();
            ui.add_enabled_ui(options.limit_enabled, |ui| {
                let mut limit = options.limit as i64;
                let resp = ui.add(
                    WheelSpinBox::new(&mut limit)
                        .range(LIMIT_MIN as i64..=LIMIT_MAX as i64)
                        .speed(1.0),
                );
                if resp.changed() {
                    options.limit = (limit as usize).clamp(LIMIT_MIN, LIMIT_MAX);
                    changed = true;
                }
            });
        });

        changed |= ui
            .checkbox(
                &mut options.use_character_names,
                "Использовать имена персонажей",
            )
            .changed();
        changed |= ui
            .checkbox(
                &mut options.merge_same_character,
                "Объединять реплики одного персонажа",
            )
            .changed();
        changed |= ui
            .checkbox(
                &mut options.include_image_bubbles,
                "Включать перевод из ImageBubble",
            )
            .on_hover_text(
                "Для каждой области текста: «Перевод» (плюс « - Описание», если включены имена персонажей).",
            )
            .changed();

        ui.horizontal(|ui| {
            ui.label("Между репликами одного персонажа:");
            ui.add_enabled_ui(options.merge_same_character, |ui| {
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut options.sep_same_character)
                            .desired_width(f32::INFINITY),
                    )
                    .changed();
            });
        });

        ui.horizontal(|ui| {
            ui.label("Между репликами:");
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut options.sep_between)
                        .desired_width(f32::INFINITY),
                )
                .changed();
        });
    });

    ui.separator();
    ui.heading("MiniJinja");
    changed |= ui
        .checkbox(&mut options.jinja2_enabled, "Использовать MiniJinja-шаблон")
        .changed();

    ui.horizontal(|ui| {
        ui.label("Доступные переменные:");
        if ui.button("Копировать").clicked() {
            ui.ctx().copy_text(VARS_INFO_TEXT.to_string());
            state.set_info("Описание переменных скопировано в буфер обмена.");
        }
    });
    draw_readonly_big_text(ui, "composition_vars_info", VARS_INFO_TEXT, VARS_ROWS, "");
    ui.label("Шаблон MiniJinja:");
    changed |= draw_editable_big_text(
        ui,
        "composition_template_editor",
        &mut options.jinja2_template,
        TEMPLATE_ROWS,
        "{% for bubble in bubbles %}{{ bubble.id }}: {{ bubble.original_text }}\n{% endfor %}",
    );

    if changed {
        actions.options_changed = true;
        actions.request_rebuild = true;
    }
}

fn compose_plain(project: &ProjectData, options: &CompositionPanelOptions) -> String {
    let use_original = options.source_mode == CompositionSourceMode::Original;
    let mut filtered = Vec::new();
    for bubble in project.bubbles.iter() {
        // Image bubbles are gated by their own option and always contribute their area translations
        // (independent of the original/translation source mode used for text bubbles).
        if is_image_bubble(bubble) {
            if options.include_image_bubbles && !image_bubble_area_translations(bubble).is_empty() {
                filtered.push(bubble);
            }
            continue;
        }
        let translation_text = bubble.text.trim();
        let original_text = bubble.original_text.trim();
        if use_original {
            if original_text.is_empty() {
                continue;
            }
            if options.ignore_translated_lines && !translation_text.is_empty() {
                continue;
            }
        } else if translation_text.is_empty() {
            continue;
        }
        filtered.push(bubble);
    }

    if filtered.is_empty() {
        return NO_ITEMS_TEXT.to_string();
    }

    filtered.sort_by(|a, b| match options.sort_method {
        CompositionSortMethod::Height => a
            .img_idx
            .cmp(&b.img_idx)
            .then_with(|| a.img_v.total_cmp(&b.img_v)),
        CompositionSortMethod::Order => {
            let a_order = bubble_extra_i32(&a.extra, "bubble_order", 0);
            let b_order = bubble_extra_i32(&b.extra, "bubble_order", 0);
            a.img_idx
                .cmp(&b.img_idx)
                .then(a_order.cmp(&b_order))
                .then_with(|| a.img_v.total_cmp(&b.img_v))
        }
    });

    let newline_replacement = if options.nl_replace.is_empty() {
        " ".to_string()
    } else {
        options.nl_replace.clone()
    };
    let sep_same_character = decode_separator_text(&options.sep_same_character);
    let sep_between = decode_separator_text(&options.sep_between);
    let (wrap_left, wrap_right) = if options.wrap_with_enabled {
        let normalized = normalize_wrap_with(&options.wrap_with);
        let mut chars = normalized.chars();
        (
            chars.next().unwrap_or('`').to_string(),
            chars.next().unwrap_or('`').to_string(),
        )
    } else {
        (String::new(), String::new())
    };

    let mut result_lines = Vec::<String>::new();
    let mut current_length = 0_usize;
    let mut prev_character: Option<String> = None;
    let mut current_group = Vec::<String>::new();

    for bubble in filtered {
        if is_image_bubble(bubble) {
            // Flush any pending merged-character group first so image lines keep reading order.
            if options.use_character_names
                && options.merge_same_character
                && !current_group.is_empty()
                && let Some(prev) = prev_character.take()
            {
                let group_text = format!("{} - {prev}", current_group.join(&sep_same_character));
                current_group.clear();
                if !append_result_item(
                    &mut result_lines,
                    &mut current_length,
                    &sep_between,
                    &group_text,
                    options.limit_enabled,
                    options.limit,
                    false,
                ) {
                    break;
                }
            }
            prev_character = None;
            let mut hit_limit = false;
            for (translation, description) in image_bubble_area_translations(bubble) {
                let mut normalized_text = translation.replace("\r\n", "\n").replace('\r', "\n");
                if options.nl_replace_enabled {
                    normalized_text = normalized_text.replace('\n', &newline_replacement);
                }
                normalized_text = collapse_inline_whitespace(&normalized_text)
                    .trim()
                    .to_string();
                if normalized_text.is_empty() {
                    continue;
                }
                let mut line_text = format!(
                    "{}{}{}{}",
                    options.replica_prefix, wrap_left, normalized_text, wrap_right
                );
                // "{translation} - {description}" only when character names are enabled and the
                // description is non-empty; otherwise just the translation (no trailing dash).
                if options.use_character_names {
                    let description = description.trim();
                    if !description.is_empty() {
                        line_text.push_str(" - ");
                        line_text.push_str(description);
                    }
                }
                if !append_result_item(
                    &mut result_lines,
                    &mut current_length,
                    &sep_between,
                    &line_text,
                    options.limit_enabled,
                    options.limit,
                    false,
                ) {
                    hit_limit = true;
                    break;
                }
            }
            if hit_limit {
                break;
            }
            continue;
        }

        let source_text = if use_original {
            bubble.original_text.trim()
        } else {
            bubble.text.trim()
        };
        if source_text.is_empty() {
            continue;
        }

        let mut normalized_text = source_text.replace("\r\n", "\n").replace('\r', "\n");
        if options.nl_replace_enabled {
            normalized_text = normalized_text.replace('\n', &newline_replacement);
        }
        normalized_text = collapse_inline_whitespace(&normalized_text)
            .trim()
            .to_string();
        if normalized_text.is_empty() {
            continue;
        }

        let line_text = format!(
            "{}{}{}{}",
            options.replica_prefix, wrap_left, normalized_text, wrap_right
        );

        if !options.use_character_names {
            if !append_result_item(
                &mut result_lines,
                &mut current_length,
                &sep_between,
                &line_text,
                options.limit_enabled,
                options.limit,
                false,
            ) {
                break;
            }
            continue;
        }

        let character = bubble_character_text(bubble);
        if !options.merge_same_character {
            let single_line = format!("{line_text} - {character}");
            if !append_result_item(
                &mut result_lines,
                &mut current_length,
                &sep_between,
                &single_line,
                options.limit_enabled,
                options.limit,
                false,
            ) {
                break;
            }
            continue;
        }

        match prev_character.as_ref() {
            None => {
                current_group.clear();
                current_group.push(line_text);
                prev_character = Some(character);
            }
            Some(prev) if prev == &character => {
                current_group.push(line_text);
            }
            Some(prev) => {
                let group_text = format!("{} - {prev}", current_group.join(&sep_same_character));
                if !append_result_item(
                    &mut result_lines,
                    &mut current_length,
                    &sep_between,
                    &group_text,
                    options.limit_enabled,
                    options.limit,
                    false,
                ) {
                    break;
                }
                current_group.clear();
                current_group.push(line_text);
                prev_character = Some(character);
            }
        }
    }

    if options.merge_same_character
        && !current_group.is_empty()
        && let Some(prev) = prev_character
    {
        let group_text = format!("{} - {prev}", current_group.join(&sep_same_character));
        let _ = append_result_item(
            &mut result_lines,
            &mut current_length,
            &sep_between,
            &group_text,
            options.limit_enabled,
            options.limit,
            true,
        );
    }

    if result_lines.is_empty() {
        NO_ITEMS_TEXT.to_string()
    } else {
        result_lines.join(&sep_between)
    }
}

fn compose_minijinja(project: &ProjectData, options: &CompositionPanelOptions) -> String {
    if options.jinja2_template.trim().is_empty() {
        return EMPTY_TEMPLATE_TEXT.to_string();
    }

    let mut env = Environment::new();
    env.set_auto_escape_callback(|_| AutoEscape::None);
    if let Err(err) = env.add_template("composition", &options.jinja2_template) {
        return format!("Ошибка MiniJinja: {err}");
    }
    let template = match env.get_template("composition") {
        Ok(template) => template,
        Err(err) => return format!("Ошибка MiniJinja: {err}"),
    };

    let bubbles = project
        .bubbles
        .iter()
        .filter(|bubble| options.include_image_bubbles || !is_image_bubble(bubble))
        .map(|bubble| serde_json::to_value(bubble).unwrap_or(Value::Null))
        .collect::<Vec<_>>();

    template
        .render(context! { bubbles => bubbles })
        .unwrap_or_else(|err| format!("Ошибка MiniJinja: {err}"))
}

/// True when the bubble is an `ImageBubble` (`bubble_class == "image"`).
fn is_image_bubble(bubble: &Bubble) -> bool {
    bubble
        .bubble_class
        .as_deref()
        .is_some_and(|class| class.eq_ignore_ascii_case("image"))
}

/// Returns `(translation, description)` for each image-bubble text area that has a non-empty
/// translation, in area order. Area 0 reads the legacy `text` / `extra.description` fields; later
/// areas read their entries in `extra["text_areas"]`.
fn image_bubble_area_translations(bubble: &Bubble) -> Vec<(String, String)> {
    let legacy_description = bubble_extra_string(&bubble.extra, "description");
    let mut out = Vec::new();
    match bubble.extra.get("text_areas").and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => {
            for (idx, entry) in arr.iter().enumerate() {
                let (translation, description) = if idx == 0 {
                    (bubble.text.clone(), legacy_description.clone())
                } else {
                    (
                        entry
                            .get("translation")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        entry
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    )
                };
                if !translation.trim().is_empty() {
                    out.push((translation, description));
                }
            }
        }
        _ => {
            if !bubble.text.trim().is_empty() {
                out.push((bubble.text.clone(), legacy_description));
            }
        }
    }
    out
}

fn bubble_character_text(bubble: &Bubble) -> String {
    let is_known = bubble_extra_bool(&bubble.extra, "is_known_character", true);
    let character_name = bubble_extra_string(&bubble.extra, "character_name");
    let clarification = bubble_extra_string(&bubble.extra, "clarification");

    let mut character = if !character_name.is_empty() {
        character_name
    } else if is_known {
        UNKNOWN_CHARACTER.to_string()
    } else {
        NARRATOR_CHARACTER.to_string()
    };

    if is_known && !clarification.is_empty() {
        character.push_str(" (");
        character.push_str(&clarification);
        character.push(')');
    }
    character
}

fn append_result_item(
    result_lines: &mut Vec<String>,
    current_length: &mut usize,
    sep_between: &str,
    item_text: &str,
    use_limit: bool,
    limit: usize,
    force: bool,
) -> bool {
    let sep_len = if result_lines.is_empty() {
        0
    } else {
        char_len(sep_between)
    };
    let new_length = current_length.saturating_add(sep_len + char_len(item_text));
    if use_limit && new_length > limit && !result_lines.is_empty() && !force {
        return false;
    }
    result_lines.push(item_text.to_string());
    *current_length = new_length;
    true
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn collapse_inline_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        if matches!(ch, ' ' | '\t') {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn decode_separator_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('0') => out.push('\0'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn truncate_chars(text: &mut String, max_chars: usize) {
    if text.chars().count() <= max_chars {
        return;
    }
    *text = text.chars().take(max_chars).collect::<String>();
}

fn draw_readonly_big_text(
    ui: &mut egui::Ui,
    id_salt: &str,
    source: &str,
    rows: usize,
    hint_text: &str,
) {
    let width = ui.available_width();
    let height = textedit_height(ui, rows);
    let mut text = source.to_string();
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::ScrollArea::both().id_salt(id_salt).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .code_editor()
                        .desired_rows(rows)
                        .desired_width(width)
                        .hint_text(hint_text),
                );
            });
        },
    );
}

fn draw_editable_big_text(
    ui: &mut egui::Ui,
    id_salt: &str,
    text: &mut String,
    rows: usize,
    hint_text: &str,
) -> bool {
    let width = ui.available_width();
    let height = textedit_height(ui, rows);
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::ScrollArea::both()
                .id_salt(id_salt)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .code_editor()
                            .desired_rows(rows)
                            .desired_width(width)
                            .hint_text(hint_text),
                    )
                    .changed()
                })
                .inner
        },
    )
    .inner
}

fn textedit_height(ui: &egui::Ui, rows: usize) -> f32 {
    let line_h = ui.text_style_height(&egui::TextStyle::Monospace);
    line_h * rows as f32 + 12.0
}

fn export_txt(project: &ProjectData, text: &str) -> Result<Option<PathBuf>, String> {
    let Some(path) = select_export_path(project, "txt", "Text files") else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(&path, text).map_err(|err| err.to_string())?;
    Ok(Some(path))
}

fn export_docx(project: &ProjectData, text: &str) -> Result<Option<PathBuf>, String> {
    let Some(path) = select_export_path(project, "docx", "Word document") else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    save_simple_docx(&path, text)?;
    Ok(Some(path))
}

/// Opens the native "save as" dialog for a composed-text export and returns the
/// chosen path (with the extension enforced), or `None` if the user cancelled.
///
/// Web stub: there is no native save dialog in the browser build, so this returns
/// `None` and the export becomes a no-op (browser download export is added
/// later). The `_` parameters keep the signature identical on both targets.
#[cfg(not(target_arch = "wasm32"))]
fn select_export_path(project: &ProjectData, ext: &str, filter_name: &str) -> Option<PathBuf> {
    let mut path = FileDialog::new()
        .set_directory(&project.project_dir)
        .set_file_name(format!("{DEFAULT_EXPORT_NAME}.{ext}"))
        .add_filter(filter_name, &[ext])
        .save_file()?;
    ensure_path_extension(&mut path, ext);
    Some(path)
}

#[cfg(target_arch = "wasm32")]
fn select_export_path(_project: &ProjectData, _ext: &str, _filter_name: &str) -> Option<PathBuf> {
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn ensure_path_extension(path: &mut PathBuf, ext: &str) {
    let has_ext = path
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|v| v.eq_ignore_ascii_case(ext));
    if !has_ext {
        path.set_extension(ext);
    }
}

fn save_simple_docx(path: &Path, text: &str) -> Result<(), String> {
    let mut paragraph_xml = String::new();
    if text.is_empty() {
        paragraph_xml.push_str("<w:p/>");
    } else {
        for line in text.split('\n') {
            if line.is_empty() {
                paragraph_xml.push_str("<w:p/>");
                continue;
            }
            paragraph_xml.push_str("<w:p><w:r><w:t xml:space=\"preserve\">");
            paragraph_xml.push_str(&xml_escape(line));
            paragraph_xml.push_str("</w:t></w:r></w:p>");
        }
    }

    let document_xml = format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
            "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">",
            "<w:body>{}<w:sectPr/></w:body></w:document>"
        ),
        paragraph_xml
    );
    let content_types_xml = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
        "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
        "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>",
        "<Default Extension=\"xml\" ContentType=\"application/xml\"/>",
        "<Override PartName=\"/word/document.xml\" ",
        "ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>",
        "</Types>"
    );
    let rels_xml = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
        "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
        "<Relationship Id=\"rId1\" ",
        "Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" ",
        "Target=\"word/document.xml\"/>",
        "</Relationships>"
    );

    let entries = [
        ("[Content_Types].xml", content_types_xml.as_bytes().to_vec()),
        ("_rels/.rels", rels_xml.as_bytes().to_vec()),
        ("word/document.xml", document_xml.as_bytes().to_vec()),
    ];
    write_zip_store(path, &entries)
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn write_zip_store(path: &Path, entries: &[(&str, Vec<u8>)]) -> Result<(), String> {
    let mut file = fs::File::create(path).map_err(|err| err.to_string())?;
    let mut central = Vec::<u8>::new();
    let mut local_offset = 0_u32;

    for (name, data) in entries {
        let name_bytes = name.as_bytes();
        let crc = crc32(data);
        let size_u32 = u32::try_from(data.len()).map_err(|_| "Слишком большой файл".to_string())?;
        let name_len =
            u16::try_from(name_bytes.len()).map_err(|_| "Слишком длинное имя файла".to_string())?;

        write_u32(&mut file, 0x0403_4b50)?;
        write_u16(&mut file, 20)?;
        write_u16(&mut file, 0)?;
        write_u16(&mut file, 0)?;
        write_u16(&mut file, 0)?;
        write_u16(&mut file, 0)?;
        write_u32(&mut file, crc)?;
        write_u32(&mut file, size_u32)?;
        write_u32(&mut file, size_u32)?;
        write_u16(&mut file, name_len)?;
        write_u16(&mut file, 0)?;
        file.write_all(name_bytes).map_err(|err| err.to_string())?;
        file.write_all(data).map_err(|err| err.to_string())?;

        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, 20);
        push_u16(&mut central, 20);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, crc);
        push_u32(&mut central, size_u32);
        push_u32(&mut central, size_u32);
        push_u16(&mut central, name_len);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, 0);
        push_u32(&mut central, local_offset);
        central.extend_from_slice(name_bytes);

        let local_size = 30_u32
            .saturating_add(u32::from(name_len))
            .saturating_add(size_u32);
        local_offset = local_offset.saturating_add(local_size);
    }

    let central_offset = local_offset;
    file.write_all(&central).map_err(|err| err.to_string())?;
    let central_size =
        u32::try_from(central.len()).map_err(|_| "Слишком большой архив".to_string())?;
    let entries_count =
        u16::try_from(entries.len()).map_err(|_| "Слишком много файлов в архиве".to_string())?;

    write_u32(&mut file, 0x0605_4b50)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, entries_count)?;
    write_u16(&mut file, entries_count)?;
    write_u32(&mut file, central_size)?;
    write_u32(&mut file, central_offset)?;
    write_u16(&mut file, 0)?;
    Ok(())
}

fn write_u16<W: Write>(w: &mut W, value: u16) -> Result<(), String> {
    w.write_all(&value.to_le_bytes())
        .map_err(|err| err.to_string())
}

fn write_u32<W: Write>(w: &mut W, value: u32) -> Result<(), String> {
    w.write_all(&value.to_le_bytes())
        .map_err(|err| err.to_string())
}

fn push_u16(dst: &mut Vec<u8>, value: u16) {
    dst.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(dst: &mut Vec<u8>, value: u32) {
    dst.extend_from_slice(&value.to_le_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = if (crc & 1) == 1 { 0xedb8_8320 } else { 0 };
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{image_bubble_area_translations, is_image_bubble};
    use crate::project::Bubble;
    use serde_json::{Map, Value, json};

    fn image_bubble(text: &str, extra: Map<String, Value>) -> Bubble {
        Bubble {
            id: 1,
            img_idx: 0,
            img_u: 0.5,
            img_v: 0.5,
            side: Some("right".to_string()),
            bubble_class: Some("image".to_string()),
            bubble_type: Some("aside".to_string()),
            text: text.to_string(),
            original_text: String::new(),
            extra,
        }
    }

    #[test]
    fn is_image_bubble_detects_class() {
        let mut text_bubble = image_bubble("t", Map::new());
        assert!(is_image_bubble(&text_bubble));
        text_bubble.bubble_class = Some("text".to_string());
        assert!(!is_image_bubble(&text_bubble));
        text_bubble.bubble_class = None;
        assert!(!is_image_bubble(&text_bubble));
    }

    #[test]
    fn area_translations_use_legacy_for_area0_and_array_for_rest() {
        let mut extra = Map::new();
        extra.insert(
            "description".to_string(),
            Value::String("desc0".to_string()),
        );
        extra.insert(
            "text_areas".to_string(),
            json!([
                {"rect": [0.0, 0.0, 0.4, 0.4], "anchor": [0.2, 0.2]},
                {"rect": [0.5, 0.5, 0.9, 0.9], "anchor": [0.7, 0.7],
                 "translation": "tr1", "description": "desc1"},
                {"rect": [0.5, 0.1, 0.9, 0.3], "anchor": [0.7, 0.2],
                 "translation": "   ", "description": "empty-skipped"}
            ]),
        );
        let areas = image_bubble_area_translations(&image_bubble("tr0", extra));
        // Area 0 from legacy text + extra.description; area 1 from the array; the blank area skipped.
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0], ("tr0".to_string(), "desc0".to_string()));
        assert_eq!(areas[1], ("tr1".to_string(), "desc1".to_string()));
    }

    #[test]
    fn area_translations_fall_back_to_legacy_single_area() {
        let mut extra = Map::new();
        extra.insert("description".to_string(), Value::String("only".to_string()));
        let areas = image_bubble_area_translations(&image_bubble("solo", extra));
        assert_eq!(areas, vec![("solo".to_string(), "only".to_string())]);
        // No translation at all -> no lines.
        let empty = image_bubble_area_translations(&image_bubble("  ", Map::new()));
        assert!(empty.is_empty());
    }
}
