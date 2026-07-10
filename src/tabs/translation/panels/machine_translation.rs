/*
FILE OVERVIEW: src/tabs/translation/panels/machine_translation.rs
UI panel for machine translation options in Translation tab.

Main items:
- `MtPanelOptions`: selected MT service + source/target languages.
- `MtPanelProgress`: transient run progress shown while a translation is active.
- `MtStopNotice`: sticky yellow notice shown when an AI run stopped due to a probable credit/quota or
  usage-limit error, with a toggle that reveals the full provider error.
- AI API MT options: provider/model/key prompt, JSON batch size, reasoning, context budget, and
  optional ImageBubble inclusion/visual detail for multimodal models.
- Translation mode toggle (`ai_image_mode`): "Обычный" batched mode vs "Только картинки"
  (per-ImageBubble) mode; the latter adds a chapter-context source switch (`ai_image_context_source`:
  original vs translation) and is gated on a multimodal model.
- `MtPanelActions`: UI actions requested by the user (`start` + `cancel`).
- `draw_machine_translation_panel`: renders settings and action buttons.

Notes:
- The panel has two tabs: legacy machine translation and AI API translation.
- Source/target languages are selected via dropdowns.
- No log output and no thread controls are shown in UI.
*/

use crate::tabs::translation::machine_translation::{
    AiMtContextSource, AiMtImageDetail, AiMtImageMode, AiMtReasoning, AiMtSortMode, MtService,
};
use crate::tabs::translation::ocr::{AiApiService, is_likely_multimodal_model};
use crate::widgets::WheelComboBox;

#[derive(Debug, Clone)]
pub struct MtPanelOptions {
    pub active_tab: MtPanelTab,
    pub service: MtService,
    pub source_lang: String,
    pub target_lang: String,
    pub ai_api_service: AiApiService,
    pub ai_api_model: String,
    pub ai_api_key_edit: String,
    pub ai_api_key_configured: Option<bool>,
    pub ai_api_models: Vec<String>,
    pub ai_api_account_status: String,
    pub ai_api_status: String,
    pub ai_api_system_instruction: String,
    pub ai_sort_mode: AiMtSortMode,
    pub ai_use_character_names: bool,
    pub ai_use_notes_prompt: bool,
    pub ai_include_characters: bool,
    pub ai_include_terms: bool,
    pub ai_batch_size: usize,
    pub ai_reasoning: AiMtReasoning,
    pub ai_context_limit_percent: u8,
    pub ai_include_existing_translation: bool,
    pub ai_include_image_bubbles: bool,
    pub ai_image_detail: AiMtImageDetail,
    pub ai_image_mode: AiMtImageMode,
    pub ai_image_context_source: AiMtContextSource,
    pub ai_open_section: AiMtPanelSection,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct MtPanelProgress {
    pub translated: usize,
    pub errors: usize,
    pub total: usize,
    pub context_used_chars: usize,
    pub context_budget_chars: usize,
    pub pruned_replicas: usize,
}

/// Sticky panel notice shown when an AI translation run stopped because of a probable credit/quota
/// or usage-limit error. The friendly message is fixed; `full_error` holds the original provider
/// error revealed on demand via the "Показать полную ошибку" toggle.
#[derive(Debug, Clone, Default)]
pub struct MtStopNotice {
    pub full_error: String,
    pub expanded: bool,
}

impl Default for MtPanelOptions {
    fn default() -> Self {
        Self {
            active_tab: MtPanelTab::Machine,
            service: MtService::Google,
            source_lang: "auto".to_string(),
            target_lang: "ru".to_string(),
            ai_api_service: AiApiService::OpenAi,
            ai_api_model: AiApiService::OpenAi.default_model().to_string(),
            ai_api_key_edit: String::new(),
            ai_api_key_configured: None,
            ai_api_models: Vec::new(),
            ai_api_account_status: t!("translation.common.press_refresh_status").to_string(),
            ai_api_status: String::new(),
            ai_api_system_instruction: "You are a manga/comic translation engine. Translate faithfully into Russian. Since the text was recognized using OCR, there may be errors, and the English text is often in uppercase. Do not preserve line breaks; write the translation in normal text, not in all caps. Preserve tone, names, honorifics, jokes, and speaker intent. Return only valid JSON with id and translation.".to_string(),
            ai_sort_mode: AiMtSortMode::Height,
            ai_use_character_names: true,
            ai_use_notes_prompt: true,
            ai_include_characters: true,
            ai_include_terms: true,
            ai_batch_size: 10,
            ai_reasoning: AiMtReasoning::None,
            ai_context_limit_percent: 60,
            ai_include_existing_translation: false,
            ai_include_image_bubbles: false,
            ai_image_detail: AiMtImageDetail::Auto,
            ai_image_mode: AiMtImageMode::Normal,
            ai_image_context_source: AiMtContextSource::Translation,
            ai_open_section: AiMtPanelSection::Translation,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum MtPanelTab {
    #[default]
    Machine,
    AiApi,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum AiMtPanelSection {
    Connection,
    #[default]
    Translation,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MtPanelActions {
    pub start_all: bool,
    pub start_page: bool,
    pub cancel: bool,
    pub options_changed: bool,
    pub save_ai_api_key: bool,
    pub clear_ai_api_key: bool,
    pub refresh_ai_api_metadata: bool,
    /// Debug-only: right-click on "Перевести всё" -> build and show the first AI request that would
    /// be sent for the whole-project scope, without translating. AI API tab only.
    pub preview_request_all: bool,
    /// Debug-only: right-click on "Перевести текущую страницу" -> build and show the first AI
    /// request for the current-page scope, without translating. AI API tab only.
    pub preview_request_page: bool,
}

#[derive(Debug, Clone, Copy)]
struct MtLanguage {
    code: &'static str,
    /// Display label source: either a stable i18n catalog key (resolved at render
    /// time) or a plain English literal for languages without a catalog entry.
    /// The wire `code` is the persisted identity, so the label is free to localize.
    title: &'static str,
}

impl MtLanguage {
    /// Localized display label. Runtime (not `const`) because `t!` is not const;
    /// a catalog miss (plain-literal titles) falls back to the stored string.
    #[must_use]
    fn title(&self) -> &'static str {
        ms_i18n::lookup(self.title).unwrap_or(self.title)
    }
}

const MT_SOURCE_LANGUAGES: &[MtLanguage] = &[
    MtLanguage {
        code: "auto",
        title: "translation.mt_panel.auto_detect_lang",
    },
    MtLanguage {
        code: "ru",
        title: "translation.ocr_langs.russian",
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
        title: "translation.ocr_langs.russian",
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
    progress: Option<MtPanelProgress>,
    stop_notice: &mut Option<MtStopNotice>,
    options: &mut MtPanelOptions,
) -> MtPanelActions {
    let mut actions = MtPanelActions::default();

    ui.heading(t!("translation.mt_panel.settings_heading"));
    ui.horizontal_wrapped(|ui| {
        actions.options_changed |= ui
            .selectable_value(
                &mut options.active_tab,
                MtPanelTab::Machine,
                t!("translation.mt_panel.machine_tab"),
            )
            .changed();
        actions.options_changed |= ui
            .selectable_value(&mut options.active_tab, MtPanelTab::AiApi, t!("translation.mt_panel.ai_api_tab"))
            .changed();
    });
    ui.separator();

    match options.active_tab {
        MtPanelTab::Machine => {
            draw_machine_tab(ui, busy, can_cancel, progress, options, &mut actions)
        }
        MtPanelTab::AiApi => draw_ai_api_tab(ui, busy, can_cancel, progress, options, &mut actions),
    }

    draw_mt_stop_notice(ui, stop_notice);

    actions
}

/// Renders the sticky credit/quota stop notice (if any) with a toggle that reveals the full
/// provider error and a dismiss button that clears it.
fn draw_mt_stop_notice(ui: &mut egui::Ui, stop_notice: &mut Option<MtStopNotice>) {
    let Some(notice) = stop_notice.as_ref() else {
        return;
    };
    // Snapshot the state for this frame; button intents are applied after drawing so the mutable
    // reassignment never overlaps the borrows used for rendering.
    let expanded = notice.expanded;
    ui.separator();
    ui.colored_label(
        egui::Color32::from_rgb(240, 200, 60),
        t!("translation.mt_panel.stopped_credits_notice"),
    );
    let mut toggle = false;
    let mut dismiss = false;
    let mut copy = false;
    ui.horizontal_wrapped(|ui| {
        let toggle_label = if expanded {
            t!("translation.mt_panel.hide_full_error_button")
        } else {
            t!("translation.mt_panel.show_full_error_button")
        };
        toggle = ui.button(toggle_label).clicked();
        dismiss = ui.button(t!("translation.mt_panel.hide_notice_button")).clicked();
        if expanded {
            copy = ui.button(t!("translation.common.copy_button")).clicked();
        }
    });
    if expanded {
        egui::ScrollArea::vertical()
            .max_height(160.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(&notice.full_error).monospace().small())
                        .wrap(),
                );
            });
    }
    if copy {
        ui.ctx().copy_text(notice.full_error.clone());
    }
    if dismiss {
        *stop_notice = None;
    } else if toggle && let Some(notice) = stop_notice.as_mut() {
        notice.expanded = !notice.expanded;
    }
}

fn draw_machine_tab(
    ui: &mut egui::Ui,
    busy: bool,
    can_cancel: bool,
    progress: Option<MtPanelProgress>,
    options: &mut MtPanelOptions,
    actions: &mut MtPanelActions,
) {
    actions.options_changed |= draw_service_combo(ui, &mut options.service);

    actions.options_changed |=
        normalize_selected_lang(&mut options.source_lang, MT_SOURCE_LANGUAGES, "auto");
    actions.options_changed |=
        normalize_selected_lang(&mut options.target_lang, MT_TARGET_LANGUAGES, "ru");

    actions.options_changed |= draw_lang_combo(
        ui,
        t!("translation.common.source_lang_label"),
        &mut options.source_lang,
        MT_SOURCE_LANGUAGES,
    );
    actions.options_changed |= draw_lang_combo(
        ui,
        t!("translation.common.target_lang_label"),
        &mut options.target_lang,
        MT_TARGET_LANGUAGES,
    );

    ui.separator();
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(!busy, egui::Button::new(t!("translation.mt_panel.translate_all_button")))
            .clicked()
        {
            actions.start_all = true;
        }
        if ui
            .add_enabled(!busy, egui::Button::new(t!("translation.mt_panel.translate_current_page_button")))
            .clicked()
        {
            actions.start_page = true;
        }
        if ui
            .add_enabled(can_cancel, egui::Button::new(t!("translation.mt_panel.cancel_translation_button")))
            .clicked()
        {
            actions.cancel = true;
        }
    });

    if busy {
        draw_translation_progress_status(ui, progress);
    }
}

fn draw_ai_api_tab(
    ui: &mut egui::Ui,
    busy: bool,
    can_cancel: bool,
    progress: Option<MtPanelProgress>,
    options: &mut MtPanelOptions,
    actions: &mut MtPanelActions,
) {
    actions.options_changed |=
        normalize_selected_lang(&mut options.source_lang, MT_SOURCE_LANGUAGES, "auto");
    actions.options_changed |=
        normalize_selected_lang(&mut options.target_lang, MT_TARGET_LANGUAGES, "ru");

    let max_width = ui.available_width().min(300.0);
    let section_max_height = ai_section_content_max_height(ui);
    ui.vertical(|ui| {
        ui.set_max_width(max_width);
        draw_ai_section_header(
            ui,
            options,
            AiMtPanelSection::Connection,
            t!("translation.mt_panel.service_key_instruction_heading"),
        );
        if options.ai_open_section == AiMtPanelSection::Connection {
            egui::ScrollArea::vertical()
                .max_height(section_max_height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    draw_ai_connection_section(ui, max_width, options, actions);
                });
        }

        draw_ai_section_header(
            ui,
            options,
            AiMtPanelSection::Translation,
            t!("translation.mt_panel.translation_params_heading"),
        );
        if options.ai_open_section == AiMtPanelSection::Translation {
            egui::ScrollArea::vertical()
                .max_height(section_max_height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    draw_ai_translation_section(ui, options, actions);
                });
        }

        ui.separator();
        ui.horizontal_wrapped(|ui| {
            let page_button =
                ui.add_enabled(!busy, egui::Button::new(t!("translation.mt_panel.translate_current_page_short_button")));
            if page_button.clicked() {
                actions.start_page = true;
            }
            // Debug: right-click reveals the exact first request for this scope without sending it.
            page_button.context_menu(|ui| {
                if ui.button(t!("translation.mt_panel.show_full_request_button")).clicked() {
                    actions.preview_request_page = true;
                    ui.close();
                }
            });
            let all_button = ui.add_enabled(!busy, egui::Button::new(t!("translation.mt_panel.translate_all_button")));
            if all_button.clicked() {
                actions.start_all = true;
            }
            all_button.context_menu(|ui| {
                if ui.button(t!("translation.mt_panel.show_full_request_button")).clicked() {
                    actions.preview_request_all = true;
                    ui.close();
                }
            });
            if ui
                .add_enabled(can_cancel, egui::Button::new(t!("translation.mt_panel.cancel_translation_button")))
                .clicked()
            {
                actions.cancel = true;
            }
        });
        if busy {
            draw_translation_progress_status(ui, progress);
        }
    });
}

fn draw_translation_progress_status(ui: &mut egui::Ui, progress: Option<MtPanelProgress>) {
    ui.colored_label(
        egui::Color32::from_rgb(255, 172, 66),
        t!("translation.mt_panel.translating_status"),
    );
    if let Some(progress) = progress {
        ui.small(tf!("translation.mt_panel.progress_status", done = progress.translated, total = progress.total, errors = progress.errors));
        if progress.context_budget_chars > 0 {
            ui.small(tf!("translation.mt_panel.context_status", used = format_context_chars(progress.context_used_chars), budget = format_context_chars(progress.context_budget_chars), pruned = progress.pruned_replicas));
        }
    }
}

fn format_context_chars(chars: usize) -> String {
    if chars >= 1000 {
        format!("{:.1}k", chars as f32 / 1000.0)
    } else {
        chars.to_string()
    }
}

fn ai_section_content_max_height(ui: &egui::Ui) -> f32 {
    (ui.ctx().content_rect().height() * 0.7 - 110.0).max(120.0)
}

fn draw_ai_section_header(
    ui: &mut egui::Ui,
    options: &mut MtPanelOptions,
    section: AiMtPanelSection,
    title: &str,
) {
    let expanded = options.ai_open_section == section;
    let prefix = if expanded { "▼" } else { "▶" };
    if ui.button(format!("{prefix} {title}")).clicked() {
        options.ai_open_section = section;
    }
}

fn draw_ai_connection_section(
    ui: &mut egui::Ui,
    max_width: f32,
    options: &mut MtPanelOptions,
    actions: &mut MtPanelActions,
) {
    actions.options_changed |= draw_lang_combo(
        ui,
        t!("translation.common.source_lang_label"),
        &mut options.source_lang,
        MT_SOURCE_LANGUAGES,
    );
    actions.options_changed |= draw_lang_combo(
        ui,
        t!("translation.common.target_lang_label"),
        &mut options.target_lang,
        MT_TARGET_LANGUAGES,
    );

    let old_service = options.ai_api_service;
    ui.label(t!("translation.common.service_label"));
    WheelComboBox::from_id_salt("translation_mt_ai_api_service")
        .selected_text(options.ai_api_service.label())
        .show_ui(ui, |ui| {
            for service in AiApiService::ALL {
                actions.options_changed |= ui
                    .selectable_value(&mut options.ai_api_service, service, service.label())
                    .changed();
            }
        });
    if old_service != options.ai_api_service {
        options.ai_api_model = options.ai_api_service.default_model().to_string();
        options.ai_api_key_edit.clear();
        options.ai_api_key_configured = None;
        options.ai_api_models.clear();
        options.ai_api_account_status = t!("translation.common.press_refresh_status").to_string();
        options.ai_api_status.clear();
        actions.options_changed = true;
        actions.refresh_ai_api_metadata = true;
    }

    ui.horizontal_wrapped(|ui| {
        let key_state = match options.ai_api_key_configured {
            Some(true) => t!("translation.common.key_saved_status"),
            Some(false) => t!("translation.common.key_not_set_status"),
            None => t!("translation.common.key_unverified_status"),
        };
        ui.small(key_state);
        if ui.small_button(t!("translation.common.refresh_button")).clicked() {
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
        if ui.small_button(t!("translation.common.save_button")).clicked() {
            actions.save_ai_api_key = true;
        }
        if ui.small_button(t!("translation.common.delete_button")).clicked() {
            actions.clear_ai_api_key = true;
        }
    });
    if !options.ai_api_status.trim().is_empty() {
        ui.small(options.ai_api_status.clone());
    }

    ui.label(t!("translation.common.model_label"));
    WheelComboBox::from_id_salt("translation_mt_ai_api_model")
        .selected_text(compact_middle(&options.ai_api_model, 42))
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

    ui.label(t!("translation.common.balance_limits_label"));
    ui.small(options.ai_api_account_status.clone());

    ui.label(t!("translation.common.system_instruction_label"));
    actions.options_changed |= ui
        .add(
            egui::TextEdit::multiline(&mut options.ai_api_system_instruction)
                .desired_width(max_width)
                .desired_rows(4),
        )
        .changed();
}

fn draw_ai_translation_section(
    ui: &mut egui::Ui,
    options: &mut MtPanelOptions,
    actions: &mut MtPanelActions,
) {
    ui.label(t!("translation.mt_panel.bubble_sort_label"));
    ui.horizontal_wrapped(|ui| {
        actions.options_changed |= ui
            .radio_value(&mut options.ai_sort_mode, AiMtSortMode::Height, t!("translation.mt_panel.sort_by_height"))
            .changed();
        actions.options_changed |= ui
            .radio_value(&mut options.ai_sort_mode, AiMtSortMode::Number, t!("translation.mt_panel.sort_by_number"))
            .changed();
    });
    actions.options_changed |= ui
        .checkbox(
            &mut options.ai_use_character_names,
            t!("translation.common.use_character_names_label"),
        )
        .changed();
    actions.options_changed |= ui
        .checkbox(
            &mut options.ai_use_notes_prompt,
            t!("translation.mt_panel.use_notes_prompt_label"),
        )
        .changed();
    if !options.ai_use_notes_prompt {
        actions.options_changed |= ui
            .checkbox(&mut options.ai_include_characters, t!("translation.mt_panel.add_characters_label"))
            .changed();
        actions.options_changed |= ui
            .checkbox(&mut options.ai_include_terms, t!("translation.mt_panel.add_terms_label"))
            .changed();
    }
    actions.options_changed |= ui
        .checkbox(
            &mut options.ai_include_existing_translation,
            t!("translation.mt_panel.include_existing_translation_label"),
        )
        .changed();
    let selected_model_is_multimodal = is_likely_multimodal_model(&options.ai_api_model);
    // Per-ImageBubble mode requires a multimodal model; coerce back to the normal mode otherwise.
    if !selected_model_is_multimodal && options.ai_image_mode != AiMtImageMode::Normal {
        options.ai_image_mode = AiMtImageMode::Normal;
        actions.options_changed = true;
    }

    ui.label(t!("translation.mt_panel.translation_mode_label"));
    ui.horizontal_wrapped(|ui| {
        if ui
            .selectable_label(
                options.ai_image_mode == AiMtImageMode::Normal,
                AiMtImageMode::Normal.title(),
            )
            .clicked()
            && options.ai_image_mode != AiMtImageMode::Normal
        {
            options.ai_image_mode = AiMtImageMode::Normal;
            actions.options_changed = true;
        }
        let images_only_toggle = ui.add_enabled(
            selected_model_is_multimodal,
            egui::Button::selectable(
                options.ai_image_mode == AiMtImageMode::ImagesOnly,
                AiMtImageMode::ImagesOnly.title(),
            ),
        );
        if images_only_toggle.clicked() && options.ai_image_mode != AiMtImageMode::ImagesOnly {
            options.ai_image_mode = AiMtImageMode::ImagesOnly;
            actions.options_changed = true;
        }
    });
    if !selected_model_is_multimodal {
        ui.small(
            t!("translation.mt_panel.images_only_multimodal_hint"),
        );
    }

    let images_only = options.ai_image_mode == AiMtImageMode::ImagesOnly;
    if images_only {
        // Each ImageBubble gets the full chapter context up to it; pick whether that context shows
        // the source originals or the existing translations.
        ui.label(t!("translation.mt_panel.chapter_context_label"));
        ui.horizontal_wrapped(|ui| {
            actions.options_changed |= ui
                .radio_value(
                    &mut options.ai_image_context_source,
                    AiMtContextSource::Original,
                    AiMtContextSource::Original.title(),
                )
                .changed();
            actions.options_changed |= ui
                .radio_value(
                    &mut options.ai_image_context_source,
                    AiMtContextSource::Translation,
                    AiMtContextSource::Translation.title(),
                )
                .changed();
        });
    } else {
        if !selected_model_is_multimodal && options.ai_include_image_bubbles {
            options.ai_include_image_bubbles = false;
            actions.options_changed = true;
        }
        actions.options_changed |= ui
            .add_enabled(
                selected_model_is_multimodal,
                egui::Checkbox::new(
                    &mut options.ai_include_image_bubbles,
                    t!("translation.mt_panel.include_image_bubbles_label"),
                ),
            )
            .changed();
    }

    // Image detail applies whenever images are actually sent: always in ImagesOnly, or when the
    // normal mode includes image bubbles.
    if images_only || options.ai_include_image_bubbles {
        ui.label(t!("translation.mt_panel.image_detail_label"));
        WheelComboBox::from_id_salt("translation_mt_ai_image_detail")
            .selected_text(options.ai_image_detail.title())
            .show_ui(ui, |ui| {
                for detail in AiMtImageDetail::ALL {
                    actions.options_changed |= ui
                        .selectable_value(&mut options.ai_image_detail, detail, detail.title())
                        .changed();
                }
            });
    }

    // Batch size only matters for the normal batched mode (ImagesOnly sends one image per request).
    if !images_only {
        ui.horizontal_wrapped(|ui| {
            ui.label(t!("translation.mt_panel.replicas_per_batch_label"));
            actions.options_changed |= ui
                .add(
                    egui::DragValue::new(&mut options.ai_batch_size)
                        .range(1..=100)
                        .speed(1),
                )
                .changed();
        });
    }
    ui.label(t!("translation.mt_panel.reasoning_label"));
    WheelComboBox::from_id_salt("translation_mt_ai_reasoning")
        .selected_text(options.ai_reasoning.title())
        .show_ui(ui, |ui| {
            for reasoning in AiMtReasoning::ALL {
                actions.options_changed |= ui
                    .selectable_value(&mut options.ai_reasoning, reasoning, reasoning.title())
                    .changed();
            }
        });
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("translation.mt_panel.context_label"));
        actions.options_changed |= ui
            .add(egui::Slider::new(
                &mut options.ai_context_limit_percent,
                10..=100,
            ))
            .changed();
        ui.label(format!("{}%", options.ai_context_limit_percent));
    });
}

fn draw_service_combo(ui: &mut egui::Ui, selected: &mut MtService) -> bool {
    let mut changed = false;
    WheelComboBox::from_label(t!("translation.common.service_label")).id_salt("translation.common.service_label")
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
                    .selectable_value(selected_code, lang.code.to_string(), lang.title())
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
        .map(|lang| lang.title())
}

fn compact_middle(text: &str, max_chars: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars || max_chars < 8 {
        return text.to_string();
    }
    let keep = (max_chars - 3) / 2;
    let start = chars.iter().take(keep).collect::<String>();
    let end = chars
        .iter()
        .skip(chars.len().saturating_sub(keep))
        .collect::<String>();
    format!("{start}...{end}")
}
