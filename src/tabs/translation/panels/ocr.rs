/*
FILE OVERVIEW: src/tabs/translation/panels/ocr.rs
UI for Translation panel "Распознавание текста".

Main types:
- `OcrPanelOptions`: user OCR options (engine/langs/model/behavior toggles).
- `OcrPanelActions`: UI actions emitted to tab controller.

Main function:
- `draw_ocr_panel`: renders OCR controls and last OCR preview.

UI specifics:
- EasyOCR selected languages are shown as removable rows with full names.
- EasyOCR/PaddleOCR model dropdown options are alphabetically sorted by title.
- AI API controls keep API key text transient and emit controller actions for
  credential-store writes; model/status text is constrained to avoid widening
  the side panel.
- Legacy local PaddleOCR engine keys are normalized back to `PaddleOCR`
  by the Translation tab loader.
*/

use crate::tabs::translation::ocr::{
    AiApiService, CharReplacementRule, OcrEngine, OcrLoadState, OcrRecognizeResult,
};
use crate::tabs::translation::panels::ocr_langs::{
    EASYOCR_FULL_LANGUAGES, EASYOCR_MAIN_LANGUAGES, PADDLEOCR_FULL_LANGUAGES,
    PADDLEOCR_MAIN_LANGUAGES,
};
use crate::widgets::WheelComboBox;

const PYTORCH_UNAVAILABLE_HINT: &str = "PyTorch не установлен";
const OCR_AI_API_PANEL_OUTSIDE_HEIGHT_RESERVE: f32 = 300.0;

#[derive(Debug, Clone)]
pub struct OcrPanelOptions {
    pub engine: OcrEngine,
    pub manga_model: String,
    pub paddle_lang: String,
    pub paddle_show_full_langs: bool,
    pub paddle_vl_script: String,
    pub easy_langs: String,
    pub easy_lang_to_add: String,
    pub easy_show_full_langs: bool,
    pub surya_task_name: String,
    pub surya_recognize_math: bool,
    pub surya_sort_lines: bool,
    pub surya_drop_repeated_text: bool,
    pub surya_max_sliding_window: u32,
    pub surya_max_tokens: u32,
    pub ai_api_service: AiApiService,
    pub ai_api_model: String,
    pub ai_api_key_edit: String,
    pub ai_api_key_configured: Option<bool>,
    pub ai_api_models: Vec<String>,
    pub ai_api_account_status: String,
    pub ai_api_status: String,
    pub ai_api_system_instruction: String,
    pub join_newlines: bool,
    pub reflect_strings: bool,
    pub copy_to_clipboard: bool,
    pub create_bubble: bool,
    pub replace_chars_enabled: bool,
    pub replace_chars_expanded: bool,
    pub char_replacements: Vec<CharReplacementRuleUi>,
}

/// Editable form of one character-substitution rule shown in the OCR panel.
///
/// `targets_raw` holds the user-facing quoted, comma-separated list of strings
/// to replace (e.g. `'·', '…'`); it is parsed into concrete targets only when a
/// runtime request is built. `enabled` toggles this single rule independently of
/// the master "Заменять символы" checkbox.
#[derive(Debug, Clone)]
pub struct CharReplacementRuleUi {
    pub enabled: bool,
    pub targets_raw: String,
    pub replacement: String,
}

impl CharReplacementRuleUi {
    /// Builds a rule with the given raw target list and replacement, enabled.
    fn new(targets_raw: &str, replacement: &str) -> Self {
        Self {
            enabled: true,
            targets_raw: targets_raw.to_string(),
            replacement: replacement.to_string(),
        }
    }
}

/// Default character substitutions: middle dots (`·`, `・`) become a period and
/// the ellipsis character (`…`) becomes three periods.
fn default_char_replacements() -> Vec<CharReplacementRuleUi> {
    vec![
        CharReplacementRuleUi::new("'·', '・'", "."),
        CharReplacementRuleUi::new("'…'", "..."),
    ]
}

impl Default for OcrPanelOptions {
    fn default() -> Self {
        Self {
            engine: OcrEngine::MangaOcr,
            manga_model: "base_onnx".to_string(),
            paddle_lang: "korean_v5".to_string(),
            paddle_show_full_langs: false,
            paddle_vl_script: "auto".to_string(),
            easy_langs: "ko".to_string(),
            easy_lang_to_add: "ko".to_string(),
            easy_show_full_langs: false,
            surya_task_name: "ocr_without_boxes".to_string(),
            surya_recognize_math: false,
            surya_sort_lines: false,
            surya_drop_repeated_text: false,
            surya_max_sliding_window: 0,
            surya_max_tokens: 0,
            ai_api_service: AiApiService::OpenAi,
            ai_api_model: AiApiService::OpenAi.default_model().to_string(),
            ai_api_key_edit: String::new(),
            ai_api_key_configured: None,
            ai_api_models: Vec::new(),
            ai_api_account_status: "Нажмите обновить.".to_string(),
            ai_api_status: String::new(),
            ai_api_system_instruction: "You are an OCR engine for manga and comics. Recognize text exactly as it is written, primarily in the following language: Korean. Pay special attention to the sounds. Do not translate, explain, describe the image, or add captions. Return only the recognized text. If a sound is particularly unclear and you are unsure, list several possible options separated by /".to_string(),
            join_newlines: true,
            reflect_strings: false,
            copy_to_clipboard: true,
            create_bubble: true,
            replace_chars_enabled: true,
            replace_chars_expanded: false,
            char_replacements: default_char_replacements(),
        }
    }
}

impl OcrPanelOptions {
    /// Builds the runtime substitution rules from the current panel state.
    ///
    /// Returns an empty vector when the master toggle is off. Each enabled UI
    /// rule with a non-empty parsed target list contributes one runtime rule.
    #[must_use]
    pub fn runtime_char_replacements(&self) -> Vec<CharReplacementRule> {
        if !self.replace_chars_enabled {
            return Vec::new();
        }
        self.char_replacements
            .iter()
            .filter(|rule| rule.enabled)
            .filter_map(|rule| {
                let targets = parse_replacement_targets(&rule.targets_raw);
                if targets.is_empty() {
                    return None;
                }
                Some(CharReplacementRule {
                    targets,
                    replacement: rule.replacement.clone(),
                })
            })
            .collect()
    }
}

/// Parses a quoted, comma-separated target list into concrete substrings.
///
/// Each comma-separated item may be wrapped in matching single or double quotes
/// (`'·'` / `"·"`); the quotes are stripped. Unquoted items are taken verbatim
/// after trimming. Empty items are skipped, so an empty or quotes-only entry
/// yields no target.
fn parse_replacement_targets(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter_map(|item| {
            let trimmed = item.trim();
            let unquoted = strip_matching_quotes(trimmed);
            if unquoted.is_empty() {
                None
            } else {
                Some(unquoted.to_string())
            }
        })
        .collect()
}

/// Strips a single pair of matching surrounding quotes (`'` or `"`) if present.
fn strip_matching_quotes(text: &str) -> &str {
    let bytes = text.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'\'' || bytes[0] == b'"')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        &text[1..text.len() - 1]
    } else {
        text
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OcrPanelActions {
    pub request_load: bool,
    pub options_changed: bool,
    pub save_ai_api_key: bool,
    pub clear_ai_api_key: bool,
    pub refresh_ai_api_metadata: bool,
}

// Parameters represent distinct required inputs with no natural grouping.
#[allow(clippy::too_many_arguments)]
pub fn draw_ocr_panel(
    ui: &mut egui::Ui,
    state: OcrLoadState,
    options: &mut OcrPanelOptions,
    backend_unavailable: bool,
    torch_available: Option<bool>,
    last_error: Option<&str>,
    last_result: Option<&OcrRecognizeResult>,
    quick_selection_shortcut: Option<&str>,
    quick_selection_active: bool,
    advanced_selection_shortcut: Option<&str>,
    advanced_selection_active: bool,
) -> OcrPanelActions {
    let mut actions = OcrPanelActions::default();
    let torch_available = torch_available.unwrap_or(true);
    let torch_mode_unavailable = selected_ocr_mode_requires_torch(options) && !torch_available;
    ui.heading("Настройки распознавания текста");
    ui.label("Движок:");
    ui.horizontal_wrapped(|ui| {
        let resp = ui.selectable_value(
            &mut options.engine,
            OcrEngine::MangaOcr,
            "MangaOCR",
        ).on_hover_text("Идеален для японского в манге.\nНе требует параметра 'Столбцы справа налево'");

        actions.options_changed |= resp.changed();

        actions.options_changed |= disabled_ocr_engine_choice(
            ui,
            &mut options.engine,
            OcrEngine::EasyOcr,
            "EasyOCR",
            torch_available,
            "Быстрее чем Paddle и поддерживает несколько языков сразу.\nХорошо справляется с английским, но для Китайского и Корейского менее точен.",
        );

        actions.options_changed |= ui
            .selectable_value(&mut options.engine, OcrEngine::PaddleOcr, "PaddleOCR")
            .on_hover_text("Более медленный и продвинутый движок.\nХорош для Китайского, Корейского и горизонтального Японского.")
            .changed();

        actions.options_changed |= disabled_ocr_engine_choice(
            ui,
            &mut options.engine,
            OcrEngine::Surya,
            "Surya",
            torch_available,
            "Самый жирный и продвинутый OCR, соответственно самый медленный.\nНе требует выбора языка, поддерживает 90 языков из коробки.",
        );

        actions.options_changed |= ui
            .selectable_value(&mut options.engine, OcrEngine::AiApi, "AI API")
            .on_hover_text("Мультимодальные облачные модели через genai. API key хранится в системном хранилище секретов.")
            .changed();

    });
    // Second engine row keeps wider engines off the first line so the side panel
    // does not grow horizontally when a new engine is added.
    ui.horizontal_wrapped(|ui| {
        actions.options_changed |= disabled_ocr_engine_choice(
            ui,
            &mut options.engine,
            OcrEngine::PaddleVl,
            "PaddleOCR-VL",
            torch_available,
            "Vision-language OCR (Transformers).\nНе требует детекта текста и выбора языка, распознаёт текст сразу из изображения.",
        );
    });
    match options.engine {
        OcrEngine::MangaOcr => {
            normalize_selected_manga_model(options);
            ui.label("Модель MangaOCR");
            WheelComboBox::from_id_salt("translation_ocr_manga_model")
                .selected_text(selected_manga_model_label(options))
                .show_ui(ui, |ui| {
                    actions.options_changed |= ui
                        .selectable_value(
                            &mut options.manga_model,
                            "base_onnx".to_string(),
                            "Базовая (onnx)",
                        )
                        .changed();
                    actions.options_changed |= ui
                        .selectable_value(
                            &mut options.manga_model,
                            "2025_onnx".to_string(),
                            "2025 (onnx)",
                        )
                        .changed();
                    actions.options_changed |= disabled_manga_model_choice(
                        ui,
                        &mut options.manga_model,
                        "base_torch",
                        "Базовая (PyTorch)",
                        torch_available,
                    );
                });
        }
        OcrEngine::PaddleOcr => {
            let show_all_changed = ui
                .checkbox(&mut options.paddle_show_full_langs, "Показывать все модели")
                .changed();
            actions.options_changed |= show_all_changed;
            if show_all_changed {
                normalize_selected_paddle_lang(options);
            }
            normalize_selected_paddle_lang(options);
            let available_langs =
                sorted_language_options(paddle_language_options(options.paddle_show_full_langs));
            let selected_lang = selected_paddle_language_label(options, &available_langs);
            WheelComboBox::from_label("Модель PaddleOCR")
                .selected_text(selected_lang)
                .show_ui(ui, |ui| {
                    for (code, title) in &available_langs {
                        actions.options_changed |= ui
                            .selectable_value(
                                &mut options.paddle_lang,
                                (*code).to_string(),
                                format!("{title} ({code})"),
                            )
                            .changed();
                    }
                });
        }
        OcrEngine::EasyOcr => {
            let show_all_changed = ui
                .checkbox(&mut options.easy_show_full_langs, "Показывать все языки")
                .changed();
            actions.options_changed |= show_all_changed;
            if show_all_changed {
                normalize_easy_lang_to_add(options);
            }
            normalize_easy_lang_to_add(options);
            let available_langs =
                sorted_language_options(easy_language_options(options.easy_show_full_langs));
            let selected_lang = selected_easy_language_label(options, &available_langs);
            ui.horizontal_wrapped(|ui| {
                ui.label("Язык:");
                WheelComboBox::from_id_salt("translation_ocr_easy_lang_to_add")
                    .selected_text(selected_lang)
                    .show_ui(ui, |ui| {
                        for (code, title) in &available_langs {
                            actions.options_changed |= ui
                                .selectable_value(
                                    &mut options.easy_lang_to_add,
                                    (*code).to_string(),
                                    format!("{title} ({code})"),
                                )
                                .changed();
                        }
                    });
                if ui.button("Добавить").clicked() {
                    actions.options_changed |= append_selected_easy_lang(options);
                }
            });
            actions.options_changed |= draw_easy_selected_langs(ui, options);
        }
        OcrEngine::PaddleVl => {
            // PaddleOCR-VL auto-detects script; an optional hard-restriction mode
            // constrains decoding to one writing system to curb hallucination on
            // messy/handwritten text (no separate language model is selected).
            normalize_paddle_vl_script(options);
            WheelComboBox::from_label("Ограничение письменности")
                .selected_text(paddle_vl_script_label(&options.paddle_vl_script))
                .show_ui(ui, |ui| {
                    for (key, label) in PADDLE_VL_SCRIPTS {
                        actions.options_changed |= ui
                            .selectable_value(
                                &mut options.paddle_vl_script,
                                (*key).to_string(),
                                *label,
                            )
                            .changed();
                    }
                });
            if options.paddle_vl_script != "auto" {
                ui.small(
                    "Жёсткий режим: модель выдаёт только выбранную письменность, цифры и знаки.",
                );
            }
        }
        // Surya auto-detects language and needs no selection UI here.
        OcrEngine::Surya => {}
        OcrEngine::AiApi => {
            draw_ai_api_options(ui, options, &mut actions);
        }
    }
    if torch_mode_unavailable {
        ui.colored_label(
            egui::Color32::from_rgb(240, 102, 102),
            PYTORCH_UNAVAILABLE_HINT,
        );
    }
    ui.separator();

    let (status_color, status_text) = match state {
        OcrLoadState::NotLoaded => (egui::Color32::GRAY, "Не загружен"),
        OcrLoadState::DownloadingModel => (
            egui::Color32::from_rgb(255, 172, 66),
            "Скачивание модели...",
        ),
        OcrLoadState::Loading => (egui::Color32::from_rgb(255, 172, 66), "Загрузка"),
        OcrLoadState::Ready => (egui::Color32::from_rgb(42, 168, 88), "Готов"),
        OcrLoadState::Error => (egui::Color32::from_rgb(208, 84, 62), "Ошибка"),
    };
    ui.horizontal(|ui| {
        ui.label("Статус:");
        ui.colored_label(status_color, status_text);
    });

    if let Some(err) = last_error
        && !err.is_empty()
    {
        ui.label(format!("Ошибка: {err}"));
    }

    ui.separator();
    actions.options_changed |= ui
        .checkbox(&mut options.join_newlines, "Сохранять переносы строк")
        .changed();
    actions.options_changed |= ui
        .checkbox(
            &mut options.reflect_strings,
            "Столбцы справа налево (манга)",
        )
        .changed();
    actions.options_changed |= ui
        .checkbox(
            &mut options.copy_to_clipboard,
            "Копировать полученный текст в буфер",
        )
        .changed();
    actions.options_changed |= ui
        .checkbox(&mut options.create_bubble, "Создавать пузырь")
        .changed();
    draw_char_replacements(ui, options, &mut actions);
    let quick_label = match quick_selection_shortcut {
        Some(shortcut) if !shortcut.is_empty() => {
            format!("Быстрое распознавание: {shortcut}+ЛКМ")
        }
        _ => "Быстрое распознавание".to_string(),
    };
    let advanced_label = match advanced_selection_shortcut {
        Some(shortcut) if !shortcut.is_empty() => {
            format!("Продвинутое распознавание: {shortcut}+ЛКМ")
        }
        _ => "Продвинутое распознавание".to_string(),
    };
    if quick_selection_active {
        ui.small(format!("{quick_label}. Активно: выделите область мышью."));
    } else {
        ui.small(format!(
            "{quick_label}. Удерживайте модификатор и выделите область."
        ));
    }
    if advanced_selection_active {
        ui.small(format!(
            "{advanced_label}. Активно: выделите область мышью для открытия окна."
        ));
    } else {
        ui.small(format!(
            "{advanced_label}. Удерживайте модификатор и выделите область."
        ));
    }

    let button_label = if state == OcrLoadState::Ready {
        "Перезагрузить движок"
    } else {
        "Загрузить движок"
    };
    ui.horizontal_wrapped(|ui| {
        let button = ui.add_enabled(
            !state.is_busy() && !backend_unavailable && !torch_mode_unavailable,
            egui::Button::new(button_label),
        );
        if button.clicked() {
            actions.request_load = true;
        }
        if backend_unavailable {
            ui.colored_label(
                egui::Color32::from_rgb(240, 102, 102),
                "ИИ бэкенд недоступен",
            );
        } else if torch_mode_unavailable {
            ui.colored_label(
                egui::Color32::from_rgb(240, 102, 102),
                PYTORCH_UNAVAILABLE_HINT,
            );
        }
    });

    if let Some(result) = last_result
        && !result.text.trim().is_empty()
    {
        ui.separator();
        ui.label(format!("Последний OCR: {} строк", result.lines.len()));
        let mut preview = result.text.clone();
        ui.add(
            egui::TextEdit::multiline(&mut preview)
                .desired_rows(8)
                .interactive(false),
        );
    }

    actions
}

/// Renders the "Заменять символы" toggle plus its expandable rule editor.
///
/// The master checkbox enables substitution; clicking the label expands an
/// indented list where each row edits one rule (per-row enable, quoted target
/// list, replacement text, delete) and a button appends a new empty rule.
fn draw_char_replacements(
    ui: &mut egui::Ui,
    options: &mut OcrPanelOptions,
    actions: &mut OcrPanelActions,
) {
    ui.horizontal(|ui| {
        actions.options_changed |= ui
            .checkbox(&mut options.replace_chars_enabled, "")
            .on_hover_text("Заменять отдельные символы в распознанном тексте.")
            .changed();
        let arrow = if options.replace_chars_expanded {
            "⏷"
        } else {
            "⏵"
        };
        let header = ui
            .add(egui::Label::new(format!("{arrow} Заменять символы")).sense(egui::Sense::click()));
        if header.clicked() {
            options.replace_chars_expanded = !options.replace_chars_expanded;
        }
    });

    if !options.replace_chars_expanded {
        return;
    }

    ui.indent("ocr_char_replacements", |ui| {
        let mut to_remove: Option<usize> = None;
        for (idx, rule) in options.char_replacements.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                actions.options_changed |= ui
                    .checkbox(&mut rule.enabled, "")
                    .on_hover_text("Включить эту строку замены")
                    .changed();
                actions.options_changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut rule.targets_raw)
                            .desired_width(96.0)
                            .hint_text("'·', '…'"),
                    )
                    .on_hover_text("Что заменять: значения в кавычках через запятую.")
                    .changed();
                ui.label("→");
                actions.options_changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut rule.replacement)
                            .desired_width(64.0)
                            .hint_text("."),
                    )
                    .on_hover_text("На что заменять.")
                    .changed();
                if ui
                    .small_button("🗑")
                    .on_hover_text("Удалить строку")
                    .clicked()
                {
                    to_remove = Some(idx);
                }
            });
        }
        if let Some(idx) = to_remove {
            options.char_replacements.remove(idx);
            actions.options_changed = true;
        }
        if ui.button("Добавить замену").clicked() {
            options.char_replacements.push(CharReplacementRuleUi {
                enabled: true,
                targets_raw: String::new(),
                replacement: String::new(),
            });
            actions.options_changed = true;
        }
    });
}

fn draw_ai_api_options(
    ui: &mut egui::Ui,
    options: &mut OcrPanelOptions,
    actions: &mut OcrPanelActions,
) {
    let max_width = ui.available_width().min(300.0);
    let max_height = ai_api_options_max_height(ui);
    let old_service = options.ai_api_service;
    egui::ScrollArea::vertical()
        .max_height(max_height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_max_width(max_width);
                ui.label("Сервис");
                WheelComboBox::from_id_salt("translation_ocr_ai_api_service")
                    .selected_text(options.ai_api_service.label())
                    .show_ui(ui, |ui| {
                        for service in AiApiService::ALL {
                            actions.options_changed |= ui
                                .selectable_value(
                                    &mut options.ai_api_service,
                                    service,
                                    service.label(),
                                )
                                .changed();
                        }
                    });
                if old_service != options.ai_api_service {
                    options.ai_api_model = options.ai_api_service.default_model().to_string();
                    options.ai_api_key_edit.clear();
                    options.ai_api_key_configured = None;
                    options.ai_api_models.clear();
                    options.ai_api_account_status = "Нажмите обновить.".to_string();
                    options.ai_api_status.clear();
                    actions.options_changed = true;
                    actions.refresh_ai_api_metadata = true;
                }

                ui.horizontal_wrapped(|ui| {
                    let key_state = match options.ai_api_key_configured {
                        Some(true) => "key сохранен",
                        Some(false) => "key не задан",
                        None => "key не проверен",
                    };
                    ui.small(key_state);
                    if ui.small_button("Обновить").clicked() {
                        actions.refresh_ai_api_metadata = true;
                    }
                });

                ui.label("API key");
                ui.add(
                    egui::TextEdit::singleline(&mut options.ai_api_key_edit)
                        .password(true)
                        .desired_width(max_width),
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button("Сохранить").clicked() {
                        actions.save_ai_api_key = true;
                    }
                    if ui.small_button("Удалить").clicked() {
                        actions.clear_ai_api_key = true;
                    }
                });
                if !options.ai_api_status.trim().is_empty() {
                    ui.small(options.ai_api_status.clone());
                }

                ui.label("Модель");
                let selected_model = compact_middle(&options.ai_api_model, 42);
                WheelComboBox::from_id_salt("translation_ocr_ai_api_model")
                    .selected_text(selected_model)
                    .show_ui(ui, |ui| {
                        let models = if options.ai_api_models.is_empty() {
                            vec![options.ai_api_service.default_model().to_string()]
                        } else {
                            options.ai_api_models.clone()
                        };
                        for model in models {
                            actions.options_changed |= ui
                                .selectable_value(&mut options.ai_api_model, model.clone(), model)
                                .changed();
                        }
                    });
                actions.options_changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut options.ai_api_model)
                            .desired_width(max_width)
                            .hint_text("model id"),
                    )
                    .changed();

                ui.label("Баланс и лимиты");
                ui.small(options.ai_api_account_status.clone());

                ui.label("Системная инструкция");
                actions.options_changed |= ui
                    .add(
                        egui::TextEdit::multiline(&mut options.ai_api_system_instruction)
                            .desired_width(max_width)
                            .desired_rows(4),
                    )
                    .changed();
            });
        });
}

fn ai_api_options_max_height(ui: &egui::Ui) -> f32 {
    (ui.ctx().content_rect().height() * 0.7 - OCR_AI_API_PANEL_OUTSIDE_HEIGHT_RESERVE).max(140.0)
}

fn disabled_ocr_engine_choice(
    ui: &mut egui::Ui,
    selected_engine: &mut OcrEngine,
    engine: OcrEngine,
    label: &str,
    torch_available: bool,
    hover_text: &str,
) -> bool {
    let selected = *selected_engine == engine;
    let response = ui
        .add_enabled(torch_available, egui::Button::new(label).selected(selected))
        .on_hover_text(hover_text);
    let response = if torch_available {
        response
    } else {
        response.on_disabled_hover_text(
            egui::RichText::new(PYTORCH_UNAVAILABLE_HINT)
                .color(egui::Color32::from_rgb(240, 102, 102)),
        )
    };
    if response.clicked() {
        *selected_engine = engine;
        return true;
    }
    false
}

fn compact_middle(text: &str, max_chars: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars || max_chars < 8 {
        return text.to_string();
    }
    let keep = (max_chars - 1) / 2;
    let start = chars.iter().take(keep).collect::<String>();
    let end = chars
        .iter()
        .skip(chars.len().saturating_sub(keep))
        .collect::<String>();
    format!("{start}...{end}")
}

fn disabled_manga_model_choice(
    ui: &mut egui::Ui,
    selected_model: &mut String,
    model_key: &str,
    label: &str,
    torch_available: bool,
) -> bool {
    let selected = selected_model == model_key;
    let response = ui.add_enabled(torch_available, egui::Button::new(label).selected(selected));
    let response = if torch_available {
        response
    } else {
        response.on_disabled_hover_text(
            egui::RichText::new(PYTORCH_UNAVAILABLE_HINT)
                .color(egui::Color32::from_rgb(240, 102, 102)),
        )
    };
    if response.clicked() {
        *selected_model = model_key.to_string();
        return true;
    }
    false
}

fn selected_ocr_mode_requires_torch(options: &OcrPanelOptions) -> bool {
    match options.engine {
        OcrEngine::EasyOcr | OcrEngine::PaddleVl | OcrEngine::Surya => true,
        OcrEngine::MangaOcr => options
            .manga_model
            .trim()
            .eq_ignore_ascii_case("base_torch"),
        OcrEngine::PaddleOcr | OcrEngine::AiApi => false,
    }
}

// PaddleOCR-VL writing-system restriction modes: (wire key, UI label). `auto`
// keeps the model's native multilingual detection; the others hard-restrict
// decoding to that script. Keys must match `script_constraint.normalize_script`.
const PADDLE_VL_SCRIPTS: &[(&str, &str)] = &[
    ("auto", "Авто (без ограничения)"),
    ("korean", "Только корейский"),
    ("chinese", "Только китайский"),
    ("japanese", "Только японский"),
];

fn normalize_paddle_vl_script(options: &mut OcrPanelOptions) {
    let normalized = options.paddle_vl_script.trim().to_ascii_lowercase();
    if !PADDLE_VL_SCRIPTS.iter().any(|(key, _)| *key == normalized) {
        options.paddle_vl_script = "auto".to_string();
    } else {
        options.paddle_vl_script = normalized;
    }
}

fn paddle_vl_script_label(script: &str) -> &'static str {
    PADDLE_VL_SCRIPTS
        .iter()
        .find(|(key, _)| *key == script)
        .map(|(_, label)| *label)
        .unwrap_or("Авто (без ограничения)")
}

fn paddle_language_options(show_full: bool) -> &'static [(&'static str, &'static str)] {
    if show_full {
        PADDLEOCR_FULL_LANGUAGES
    } else {
        PADDLEOCR_MAIN_LANGUAGES
    }
}

fn normalize_selected_manga_model(options: &mut OcrPanelOptions) {
    let normalized = options.manga_model.trim().to_ascii_lowercase();
    options.manga_model = match normalized.as_str() {
        "2025" | "2025_onnx" | "mangaocr_2025" | "manga_ocr_2025" => "2025_onnx".to_string(),
        "base_torch" | "torch" | "pytorch" | "base_pytorch" => "base_torch".to_string(),
        "base" | "base_onnx" | "basic" | "default" | "mangaocr_base" | "manga_ocr_base" => {
            "base_onnx".to_string()
        }
        _ => "base_onnx".to_string(),
    };
}

fn selected_manga_model_label(options: &OcrPanelOptions) -> String {
    match options.manga_model.as_str() {
        "2025_onnx" => "2025 (onnx)".to_string(),
        "base_torch" => "Базовая (PyTorch)".to_string(),
        _ => "Базовая (onnx)".to_string(),
    }
}

fn normalize_selected_paddle_lang(options: &mut OcrPanelOptions) {
    let langs = paddle_language_options(options.paddle_show_full_langs);
    if langs.is_empty() {
        return;
    }
    let normalized = match options.paddle_lang.trim().to_ascii_lowercase().as_str() {
        "japan_v5" | "chinese_cht_v5" => "chinese_v5".to_string(),
        "cyrillic_v3" => "eslav_v5".to_string(),
        "devanagari_v3" => "hindi_v3".to_string(),
        "korean" | "ko" => "korean_v5".to_string(),
        "ch" | "japan" => "chinese_v5".to_string(),
        "en" => "english_v5".to_string(),
        "latin" => "latin_v5".to_string(),
        "eslav" => "eslav_v5".to_string(),
        "thai" => "thai_v5".to_string(),
        "greek" => "greek_v5".to_string(),
        "arabic" => "arabic_v3".to_string(),
        "hindi" => "hindi_v3".to_string(),
        "telugu" => "telugu_v3".to_string(),
        "tamil" => "tamil_v3".to_string(),
        other => other.to_string(),
    };
    if langs.iter().any(|(code, _)| *code == normalized.as_str()) {
        options.paddle_lang = normalized;
        return;
    }
    if langs.iter().any(|(code, _)| *code == "korean_v5") {
        options.paddle_lang = "korean_v5".to_string();
        return;
    }
    options.paddle_lang = langs[0].0.to_string();
}

fn selected_paddle_language_label(options: &OcrPanelOptions, langs: &[(&str, &str)]) -> String {
    langs
        .iter()
        .find(|(code, _)| *code == options.paddle_lang.as_str())
        .map(|(code, title)| format!("{title} ({code})"))
        .unwrap_or_else(|| options.paddle_lang.clone())
}

fn easy_language_options(show_full: bool) -> &'static [(&'static str, &'static str)] {
    if show_full {
        EASYOCR_FULL_LANGUAGES
    } else {
        EASYOCR_MAIN_LANGUAGES
    }
}

fn normalize_easy_lang_to_add(options: &mut OcrPanelOptions) {
    let langs = easy_language_options(options.easy_show_full_langs);
    if langs.is_empty() {
        return;
    }
    if langs
        .iter()
        .any(|(code, _)| *code == options.easy_lang_to_add.as_str())
    {
        return;
    }
    if langs.iter().any(|(code, _)| *code == "ko") {
        options.easy_lang_to_add = "ko".to_string();
        return;
    }
    options.easy_lang_to_add = langs[0].0.to_string();
}

fn selected_easy_language_label(options: &OcrPanelOptions, langs: &[(&str, &str)]) -> String {
    langs
        .iter()
        .find(|(code, _)| *code == options.easy_lang_to_add.as_str())
        .map(|(code, title)| format!("{title} ({code})"))
        .unwrap_or_else(|| options.easy_lang_to_add.clone())
}

fn append_selected_easy_lang(options: &mut OcrPanelOptions) -> bool {
    let code = options.easy_lang_to_add.trim();
    if code.is_empty() {
        return false;
    }
    let normalized_code = code.to_ascii_lowercase();
    let mut langs = parse_lang_codes(&options.easy_langs);
    if langs.iter().any(|item| item == &normalized_code) {
        return false;
    }
    langs.push(normalized_code);
    options.easy_langs = langs.join(", ");
    true
}

fn draw_easy_selected_langs(ui: &mut egui::Ui, options: &mut OcrPanelOptions) -> bool {
    let langs = parse_lang_codes(&options.easy_langs);
    ui.label("Активные языки EasyOCR: (можно выбрать несколько)");
    if langs.is_empty() {
        ui.small("Не выбраны.");
        return false;
    }

    let mut to_remove: Option<String> = None;
    for code in &langs {
        let label = easy_lang_chip_label(code);
        ui.horizontal(|ui| {
            ui.label(label);
            if ui.small_button("x").on_hover_text("Удалить язык").clicked() {
                to_remove = Some(code.clone());
            }
        });
    }

    match to_remove {
        Some(code) => remove_easy_lang(options, &code),
        None => false,
    }
}

fn easy_lang_chip_label(code: &str) -> String {
    match easy_lang_title_by_code(code) {
        Some(title) => format!("{title} ({code})"),
        None => code.to_string(),
    }
}

fn easy_lang_title_by_code(code: &str) -> Option<&'static str> {
    EASYOCR_FULL_LANGUAGES
        .iter()
        .find(|(lang_code, _)| lang_code.eq_ignore_ascii_case(code))
        .map(|(_, title)| *title)
}

fn remove_easy_lang(options: &mut OcrPanelOptions, code_to_remove: &str) -> bool {
    let code_to_remove = code_to_remove.to_ascii_lowercase();
    let before = parse_lang_codes(&options.easy_langs);
    let after = before
        .iter()
        .filter(|code| **code != code_to_remove)
        .cloned()
        .collect::<Vec<_>>();
    if after.len() == before.len() {
        return false;
    }
    options.easy_langs = after.join(", ");
    true
}

fn sorted_language_options(
    langs: &[(&'static str, &'static str)],
) -> Vec<(&'static str, &'static str)> {
    let mut out = langs.to_vec();
    out.sort_by(|(code_a, title_a), (code_b, title_b)| {
        title_a
            .to_lowercase()
            .cmp(&title_b.to_lowercase())
            .then_with(|| code_a.cmp(code_b))
    });
    out
}

fn parse_lang_codes(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for code in raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let normalized = code.to_ascii_lowercase();
        if out.iter().any(|item| item == &normalized) {
            continue;
        }
        out.push(normalized);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{OcrPanelOptions, parse_replacement_targets};

    #[test]
    fn parses_quoted_comma_separated_targets() {
        assert_eq!(parse_replacement_targets("'·', '・'"), vec!["·", "・"]);
        assert_eq!(parse_replacement_targets("\"…\""), vec!["…"]);
        assert_eq!(parse_replacement_targets("·, ・"), vec!["·", "・"]);
    }

    #[test]
    fn skips_empty_and_quotes_only_targets() {
        assert!(parse_replacement_targets("").is_empty());
        assert!(parse_replacement_targets("'', \"\"").is_empty());
        assert_eq!(parse_replacement_targets("'·', ''").to_vec(), vec!["·"]);
    }

    #[test]
    fn default_options_yield_dot_substitutions() {
        let options = OcrPanelOptions::default();
        let runtime = options.runtime_char_replacements();
        assert_eq!(runtime.len(), 2);
        assert_eq!(runtime[0].targets, vec!["·", "・"]);
        assert_eq!(runtime[0].replacement, ".");
        assert_eq!(runtime[1].targets, vec!["…"]);
        assert_eq!(runtime[1].replacement, "...");
    }

    #[test]
    fn master_toggle_off_disables_all_rules() {
        let options = OcrPanelOptions {
            replace_chars_enabled: false,
            ..Default::default()
        };
        assert!(options.runtime_char_replacements().is_empty());
    }

    #[test]
    fn disabled_rule_is_skipped() {
        let mut options = OcrPanelOptions::default();
        options.char_replacements[0].enabled = false;
        let runtime = options.runtime_char_replacements();
        assert_eq!(runtime.len(), 1);
        assert_eq!(runtime[0].targets, vec!["…"]);
    }
}
