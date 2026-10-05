/*
FILE OVERVIEW: crates/ms-tab-translation/src/panels/machine_translation.rs
UI panel for machine translation options in Translation tab.

Main items:
- `MtPanelOptions`: selected MT service + source/target languages.
- `MtPanelProgress`: transient run progress shown while a translation is active.
- `MtStopNotice`: sticky yellow notice shown when an AI run stopped due to a probable credit/quota or
  usage-limit error, with a toggle that reveals the full provider error.
- AI API MT options: the connection section (source/target languages, then the shared
  `ms_ai_api::draw_connection` widget for provider/base URL/key/model/system prompt), JSON batch
  size, reasoning, context budget, and optional ImageBubble inclusion/visual detail.
- Translation mode toggle (`ai_image_mode`): "Обычный" batched mode vs "Только картинки"
  (per-ImageBubble) mode; the latter adds a chapter-context source switch (`ai_image_context_source`:
  original vs translation). Image modes are blocked only for a model `ms_ai_api::image_input_support`
  lists as text-only (`NotSupported`); `Supported` and `Unknown` models may use them.
- `MtPanelActions`: UI actions requested by the user (`start` + `cancel`).
- `draw_machine_translation_panel`: renders settings and action buttons. Its
  `project_context_available` flag (false in a single-image session, which has no title) hides the
  notes / characters / terms toggles; the prompt builder ignores them there too.

Notes:
- The panel has two tabs: legacy machine translation and AI API translation.
- Source/target languages are selected via dropdowns.
- No log output and no thread controls are shown in UI.
*/

use crate::machine_translation::{
    AiMtContextSource, AiMtImageDetail, AiMtImageMode, AiMtReasoning, AiMtSortMode, MtService,
};
use ms_ai_api::{AiApiConnectionActions, AiApiConnectionState, ImageInputSupport, draw_connection, image_input_support};
use ms_widgets::WheelComboBox;

#[derive(Debug, Clone)]
pub struct MtPanelOptions {
    pub active_tab: MtPanelTab,
    pub service: MtService,
    pub source_lang: String,
    pub target_lang: String,
    /// AI API connection (service, model, system instruction are persisted by the tab).
    pub ai_api: AiApiConnectionState,
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
            ai_api: AiApiConnectionState::new("You are a manga/comic translation engine. Translate faithfully into Russian. Since the text was recognized using OCR, there may be errors, and the English text is often in uppercase. Do not preserve line breaks; write the translation in normal text, not in all caps. Preserve tone, names, honorifics, jokes, and speaker intent. Return only valid JSON with id and translation."),
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
    /// Key save / delete / metadata refresh requested in the AI API connection widget (its
    /// `options_changed` is also folded into `options_changed` above).
    pub ai_api: AiApiConnectionActions,
    /// Debug-only: right-click on "Перевести всё" -> build and show the first AI request that would
    /// be sent for the whole-project scope, without translating. AI API tab only.
    pub preview_request_all: bool,
    /// Debug-only: right-click on "Перевести текущую страницу" -> build and show the first AI
    /// request for the current-page scope, without translating. AI API tab only.
    pub preview_request_page: bool,
}

// `pub`, not crate-private: `MtLanguage`, its wire `code`, its `title()` label and the
// `MT_SOURCE_LANGUAGES` table are all read by the `cleaning` tab's Flux.2 engine, which
// lives in another crate and builds its own source-language picker from them.
#[derive(Debug, Clone, Copy)]
pub struct MtLanguage {
    pub code: &'static str,
    /// Display label source: either a stable i18n catalog key (resolved at render
    /// time) or a plain English literal for languages without a catalog entry.
    /// The wire `code` is the persisted identity, so the label is free to localize.
    title: &'static str,
}

impl MtLanguage {
    /// Localized display label. Runtime (not `const`) because `t!` is not const;
    /// a catalog miss (plain-literal titles) falls back to the stored string.
    #[must_use]
    pub fn title(&self) -> &'static str {
        ms_i18n::lookup(self.title).unwrap_or(self.title)
    }
}

pub const MT_SOURCE_LANGUAGES: &[MtLanguage] = &[
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

/// Draws the MT panel and returns this frame's user actions. `project_context_available` is
/// `false` in a single-image session: the notes / characters / terms toggles are then hidden
/// (their persisted values stay untouched; `machine_translation::project_context_sources`
/// ignores them for such a run).
pub fn draw_machine_translation_panel(
    ui: &mut egui::Ui,
    busy: bool,
    can_cancel: bool,
    progress: Option<MtPanelProgress>,
    stop_notice: &mut Option<MtStopNotice>,
    options: &mut MtPanelOptions,
    project_context_available: bool,
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
        MtPanelTab::AiApi => draw_ai_api_tab(ui, busy, can_cancel, progress, options, project_context_available, &mut actions),
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
        ms_theme::status::WARNING,
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
    project_context_available: bool,
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
                    draw_ai_translation_section(ui, options, project_context_available, actions);
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
        ms_theme::status::INFO,
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

/// The "connection" accordion section: source/target language combos, then the shared AI API
/// connection widget (its `options_changed` folded into `actions.options_changed`).
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

    let connection = draw_connection(ui, "translation_mt_ai_api", max_width, &mut options.ai_api);
    actions.options_changed |= connection.options_changed;
    actions.ai_api = connection;
}

fn draw_ai_translation_section(
    ui: &mut egui::Ui,
    options: &mut MtPanelOptions,
    project_context_available: bool,
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
    // The notes / characters / terms documents belong to a title; a single-image session has
    // none, so the toggles are hidden there.
    if project_context_available {
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
    }
    actions.options_changed |= ui
        .checkbox(
            &mut options.ai_include_existing_translation,
            t!("translation.mt_panel.include_existing_translation_label"),
        )
        .changed();
    // Image modes are offered unless the model is KNOWN to be text-only: an unlisted model
    // (`Unknown`, e.g. a local server's) may well accept images, and the user can tell.
    let images_allowed = image_input_support(&options.ai_api.model) != ImageInputSupport::NotSupported;
    // Per-ImageBubble mode needs image input; coerce back to the normal mode for a text-only model.
    if !images_allowed && options.ai_image_mode != AiMtImageMode::Normal {
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
            images_allowed,
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
    if !images_allowed {
        ui.small(
            t!("translation.mt_panel.images_unsupported_model_hint"),
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
        if !images_allowed && options.ai_include_image_bubbles {
            options.ai_include_image_bubbles = false;
            actions.options_changed = true;
        }
        actions.options_changed |= ui
            .add_enabled(
                images_allowed,
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

