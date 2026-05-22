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
- Legacy local PaddleOCR engine keys are normalized back to `PaddleOCR`
  by the Translation tab loader.
*/

use crate::tabs::translation::ocr::{OcrEngine, OcrLoadState, OcrRecognizeResult};
use crate::tabs::translation::panels::ocr_langs::{
    EASYOCR_FULL_LANGUAGES, EASYOCR_MAIN_LANGUAGES, PADDLEOCR_FULL_LANGUAGES,
    PADDLEOCR_MAIN_LANGUAGES,
};
use crate::widgets::WheelComboBox;

const PYTORCH_UNAVAILABLE_HINT: &str = "PyTorch не установлен";

#[derive(Debug, Clone)]
pub struct OcrPanelOptions {
    pub engine: OcrEngine,
    pub manga_model: String,
    pub paddle_lang: String,
    pub paddle_show_full_langs: bool,
    pub easy_langs: String,
    pub easy_lang_to_add: String,
    pub easy_show_full_langs: bool,
    pub surya_task_name: String,
    pub surya_recognize_math: bool,
    pub surya_sort_lines: bool,
    pub surya_drop_repeated_text: bool,
    pub surya_max_sliding_window: u32,
    pub surya_max_tokens: u32,
    pub join_newlines: bool,
    pub reflect_strings: bool,
    pub copy_to_clipboard: bool,
    pub create_bubble: bool,
}

impl Default for OcrPanelOptions {
    fn default() -> Self {
        Self {
            engine: OcrEngine::MangaOcr,
            manga_model: "base_onnx".to_string(),
            paddle_lang: "korean_v5".to_string(),
            paddle_show_full_langs: false,
            easy_langs: "ko".to_string(),
            easy_lang_to_add: "ko".to_string(),
            easy_show_full_langs: false,
            surya_task_name: "ocr_without_boxes".to_string(),
            surya_recognize_math: false,
            surya_sort_lines: false,
            surya_drop_repeated_text: false,
            surya_max_sliding_window: 0,
            surya_max_tokens: 0,
            join_newlines: true,
            reflect_strings: false,
            copy_to_clipboard: true,
            create_bubble: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OcrPanelActions {
    pub request_load: bool,
    pub options_changed: bool,
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
        OcrEngine::Surya => {}
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
        OcrEngine::EasyOcr | OcrEngine::Surya => true,
        OcrEngine::MangaOcr => options
            .manga_model
            .trim()
            .eq_ignore_ascii_case("base_torch"),
        OcrEngine::PaddleOcr => false,
    }
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
