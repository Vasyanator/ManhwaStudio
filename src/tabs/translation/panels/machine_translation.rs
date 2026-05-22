/*
FILE OVERVIEW: src/tabs/translation/panels/machine_translation.rs
UI panel for machine translation options in Translation tab.

Main items:
- `MtPanelOptions`: selected MT service + source/target languages.
- `MtPanelActions`: UI actions requested by the user (`start` + `cancel`).
- `draw_machine_translation_panel`: renders settings and action buttons.

Notes:
- MT service is selected from available `MtService` providers.
- Source/target languages are selected via dropdowns.
- No log output and no thread controls are shown in UI.
*/

use crate::tabs::translation::machine_translation::MtService;
use crate::widgets::WheelComboBox;

#[derive(Debug, Clone)]
pub struct MtPanelOptions {
    pub service: MtService,
    pub source_lang: String,
    pub target_lang: String,
}

impl Default for MtPanelOptions {
    fn default() -> Self {
        Self {
            service: MtService::Google,
            source_lang: "auto".to_string(),
            target_lang: "ru".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MtPanelActions {
    pub start_all: bool,
    pub start_page: bool,
    pub cancel: bool,
    pub options_changed: bool,
}

#[derive(Debug, Clone, Copy)]
struct MtLanguage {
    code: &'static str,
    title: &'static str,
}

const MT_SOURCE_LANGUAGES: &[MtLanguage] = &[
    MtLanguage {
        code: "auto",
        title: "Автоопределение",
    },
    MtLanguage {
        code: "ru",
        title: "Русский",
    },
    MtLanguage {
        code: "en",
        title: "English",
    },
    MtLanguage {
        code: "ko",
        title: "Korean",
    },
    MtLanguage {
        code: "ja",
        title: "Japanese",
    },
    MtLanguage {
        code: "zh-cn",
        title: "Chinese (Simplified)",
    },
    MtLanguage {
        code: "zh-tw",
        title: "Chinese (Traditional)",
    },
    MtLanguage {
        code: "es",
        title: "Spanish",
    },
    MtLanguage {
        code: "fr",
        title: "French",
    },
    MtLanguage {
        code: "de",
        title: "German",
    },
    MtLanguage {
        code: "it",
        title: "Italian",
    },
    MtLanguage {
        code: "pt",
        title: "Portuguese",
    },
    MtLanguage {
        code: "pl",
        title: "Polish",
    },
    MtLanguage {
        code: "tr",
        title: "Turkish",
    },
    MtLanguage {
        code: "uk",
        title: "Ukrainian",
    },
    MtLanguage {
        code: "ar",
        title: "Arabic",
    },
    MtLanguage {
        code: "hi",
        title: "Hindi",
    },
    MtLanguage {
        code: "id",
        title: "Indonesian",
    },
    MtLanguage {
        code: "th",
        title: "Thai",
    },
    MtLanguage {
        code: "vi",
        title: "Vietnamese",
    },
];

const MT_TARGET_LANGUAGES: &[MtLanguage] = &[
    MtLanguage {
        code: "ru",
        title: "Русский",
    },
    MtLanguage {
        code: "en",
        title: "English",
    },
    MtLanguage {
        code: "ko",
        title: "Korean",
    },
    MtLanguage {
        code: "ja",
        title: "Japanese",
    },
    MtLanguage {
        code: "zh-cn",
        title: "Chinese (Simplified)",
    },
    MtLanguage {
        code: "zh-tw",
        title: "Chinese (Traditional)",
    },
    MtLanguage {
        code: "es",
        title: "Spanish",
    },
    MtLanguage {
        code: "fr",
        title: "French",
    },
    MtLanguage {
        code: "de",
        title: "German",
    },
    MtLanguage {
        code: "it",
        title: "Italian",
    },
    MtLanguage {
        code: "pt",
        title: "Portuguese",
    },
    MtLanguage {
        code: "pl",
        title: "Polish",
    },
    MtLanguage {
        code: "tr",
        title: "Turkish",
    },
    MtLanguage {
        code: "uk",
        title: "Ukrainian",
    },
    MtLanguage {
        code: "ar",
        title: "Arabic",
    },
    MtLanguage {
        code: "hi",
        title: "Hindi",
    },
    MtLanguage {
        code: "id",
        title: "Indonesian",
    },
    MtLanguage {
        code: "th",
        title: "Thai",
    },
    MtLanguage {
        code: "vi",
        title: "Vietnamese",
    },
];

pub fn draw_machine_translation_panel(
    ui: &mut egui::Ui,
    busy: bool,
    can_cancel: bool,
    options: &mut MtPanelOptions,
) -> MtPanelActions {
    let mut actions = MtPanelActions::default();

    ui.heading("Настройки машинного перевода");
    actions.options_changed |= draw_service_combo(ui, &mut options.service);

    actions.options_changed |=
        normalize_selected_lang(&mut options.source_lang, MT_SOURCE_LANGUAGES, "auto");
    actions.options_changed |=
        normalize_selected_lang(&mut options.target_lang, MT_TARGET_LANGUAGES, "ru");

    actions.options_changed |= draw_lang_combo(
        ui,
        "Исходный язык",
        &mut options.source_lang,
        MT_SOURCE_LANGUAGES,
    );
    actions.options_changed |= draw_lang_combo(
        ui,
        "Целевой язык",
        &mut options.target_lang,
        MT_TARGET_LANGUAGES,
    );

    ui.separator();
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(!busy, egui::Button::new("Перевести всё"))
            .clicked()
        {
            actions.start_all = true;
        }
        if ui
            .add_enabled(!busy, egui::Button::new("Перевести на текущей странице"))
            .clicked()
        {
            actions.start_page = true;
        }
        if ui
            .add_enabled(can_cancel, egui::Button::new("Отменить перевод"))
            .clicked()
        {
            actions.cancel = true;
        }
    });

    if busy {
        ui.colored_label(
            egui::Color32::from_rgb(255, 172, 66),
            "Перевод выполняется...",
        );
    }

    actions
}

fn draw_service_combo(ui: &mut egui::Ui, selected: &mut MtService) -> bool {
    let mut changed = false;
    WheelComboBox::from_label("Сервис")
        .selected_text(selected.title())
        .show_ui(ui, |ui| {
            for service in MtService::all() {
                changed |= ui
                    .selectable_value(selected, *service, service.title())
                    .changed();
            }
        });
    changed
}

fn draw_lang_combo(
    ui: &mut egui::Ui,
    label: &str,
    selected_code: &mut String,
    langs: &[MtLanguage],
) -> bool {
    let selected_text = language_title(selected_code, langs)
        .map(|title| format!("{title} ({})", selected_code))
        .unwrap_or_else(|| selected_code.clone());

    let mut changed = false;
    WheelComboBox::from_label(label)
        .selected_text(selected_text)
        .show_ui(ui, |ui| {
            for lang in langs {
                changed |= ui
                    .selectable_value(selected_code, lang.code.to_string(), lang.title)
                    .changed();
            }
        });
    changed
}

fn normalize_selected_lang(
    selected_code: &mut String,
    langs: &[MtLanguage],
    fallback: &str,
) -> bool {
    if language_title(selected_code, langs).is_some() {
        return false;
    }
    *selected_code = fallback.to_string();
    true
}

fn language_title<'a>(code: &str, langs: &'a [MtLanguage]) -> Option<&'a str> {
    langs
        .iter()
        .find(|lang| lang.code.eq_ignore_ascii_case(code))
        .map(|lang| lang.title)
}
