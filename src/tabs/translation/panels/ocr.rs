/*
FILE OVERVIEW: src/tabs/translation/panels/ocr.rs
UI for Translation panel "Распознавание текста".

Main types:
- `OcrPanelOptions`: user OCR options (engine/langs/model/behavior toggles).
- `OcrPanelActions`: UI actions emitted to tab controller.

Main function:
- `draw_ocr_panel`: renders OCR controls and last OCR preview.

UI specifics:
- Runtime engine-selection buttons are `AiButton`s gated on a per-engine
  `AiRequirement` (optimistic while the capability is unknown) with a runtime
  marker badge; AiApi stays a plain ungated selectable. The selected engine's
  options interface and the load button are disabled together when the selected
  engine+model requirement is known-unavailable. Capabilities are read from the
  process-global `AiCaps::current`, not passed in.
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
    PADDLEOCR_MAIN_LANGUAGES, lang_label,
};
use crate::widgets::{AiButton, AiCaps, AiRequirement, WheelComboBox};

/// Runtime (not `const`) because `t!` is not const; resolves the active catalog value.
#[must_use]
fn pytorch_unavailable_hint() -> &'static str {
    t!("translation.common.pytorch_not_installed_status")
}
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
            ai_api_account_status: t!("translation.common.press_refresh_status").to_string(),
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

/// AiRequirement gating the engine-SELECTION button (permissive: MangaOCR is
/// usable if EITHER torch or onnx is available, so the user can then pick a
/// runnable model). `None` for AiApi (network-only, no local runtime).
fn engine_button_requirement(engine: OcrEngine) -> Option<AiRequirement> {
    match engine {
        OcrEngine::MangaOcr => Some(AiRequirement::TorchOrOnnx),
        OcrEngine::EasyOcr => Some(AiRequirement::Torch),
        OcrEngine::PaddleOcr => Some(AiRequirement::Onnx),
        OcrEngine::PaddleVl => Some(AiRequirement::Torch),
        OcrEngine::Surya => Some(AiRequirement::Torch),
        OcrEngine::AiApi => None,
    }
}

/// AiRequirement of the currently-selected engine+model combo (model-aware),
/// used to gate the engine's options interface and the load button. `None` for
/// AiApi (network-only).
fn selected_mode_requirement(options: &OcrPanelOptions) -> Option<AiRequirement> {
    match options.engine {
        // MangaOCR runs on the runtime of its selected model: the `base_torch`
        // export needs PyTorch, the ONNX exports need onnxruntime.
        OcrEngine::MangaOcr => {
            if options
                .manga_model
                .trim()
                .eq_ignore_ascii_case("base_torch")
            {
                Some(AiRequirement::Torch)
            } else {
                Some(AiRequirement::Onnx)
            }
        }
        OcrEngine::EasyOcr => Some(AiRequirement::Torch),
        OcrEngine::PaddleOcr => Some(AiRequirement::Onnx),
        OcrEngine::PaddleVl => Some(AiRequirement::Torch),
        OcrEngine::Surya => Some(AiRequirement::Torch),
        OcrEngine::AiApi => None,
    }
}

/// Short runtime marker badge for an engine ("Torch"/"ONNX"/"Torch/ONNX"), or
/// `None` when the engine has no local-runtime dependency (AiApi).
fn engine_marker(engine: OcrEngine) -> Option<&'static str> {
    match engine {
        OcrEngine::MangaOcr => Some("Torch/ONNX"),
        OcrEngine::EasyOcr => Some("Torch"),
        OcrEngine::PaddleOcr => Some("ONNX"),
        OcrEngine::PaddleVl => Some("Torch"),
        OcrEngine::Surya => Some("Torch"),
        OcrEngine::AiApi => None,
    }
}

/// Renders one runtime-OCR engine selection button via [`AiButton`], strictly gated
/// on the engine's `requirement` (a known-unavailable OR not-yet-known capability
/// disables it). Native ORT reports as "armed" before its first load, so strict
/// gating does not lock native engines out. Applies the engine's runtime marker
/// badge and its descriptive `hover`. Returns `true` when the click selected this
/// engine.
fn engine_select_button(
    ui: &mut egui::Ui,
    selected: &mut OcrEngine,
    engine: OcrEngine,
    requirement: AiRequirement,
    label: &str,
    hover: &str,
) -> bool {
    let is_selected = *selected == engine;
    // Frameless (like `selectable_value`): transparent at rest, highlighted only on
    // hover/selection — no resting background box, matching the AI API entry.
    let mut btn = AiButton::new(label, requirement)
        .selected(is_selected)
        .frame(false);
    if let Some(marker) = engine_marker(engine) {
        btn = btn.marker(marker);
    }
    let response = btn.draw(ui).response.on_hover_text(hover);
    if response.clicked() {
        *selected = engine;
        return true;
    }
    false
}

/// Renders the OCR settings panel and returns the emitted [`OcrPanelActions`].
///
/// Runtime AI capabilities (backend/torch/onnx) are read from the process-global
/// snapshot via [`AiCaps::current`], so no backend/torch state is passed in. Engine
/// selection buttons gate on a per-engine [`AiRequirement`] (optimistic while the
/// capability is unknown); the selected engine's options interface and the load
/// button are disabled together when the selected engine+model requirement is
/// known-unavailable. AiApi stays ungated (network-only).
// Parameters represent distinct required inputs with no natural grouping.
#[allow(clippy::too_many_arguments)]
pub fn draw_ocr_panel(
    ui: &mut egui::Ui,
    state: OcrLoadState,
    options: &mut OcrPanelOptions,
    last_error: Option<&str>,
    last_result: Option<&OcrRecognizeResult>,
    quick_selection_shortcut: Option<&str>,
    quick_selection_active: bool,
    advanced_selection_shortcut: Option<&str>,
    advanced_selection_active: bool,
) -> OcrPanelActions {
    let mut actions = OcrPanelActions::default();
    let caps = AiCaps::current();
    // Strict gating: the selected engine's interface and load button are enabled
    // only when its runtime requirement is satisfied. Native ORT reports as "armed"
    // before its first load, so native engines are not locked out; a backend-routed
    // engine with the backend down (or torch unavailable) correctly disables.
    let selected_engine_enabled =
        selected_mode_requirement(options).is_none_or(|req| req.satisfied(&caps));
    ui.heading(t!("translation.ocr_panel.settings_heading"));
    ui.label(t!("translation.ocr_panel.engine_label"));
    // Runtime engines gate their selection button on a per-engine AiRequirement
    // (optimistic while unknown); AiApi is network-only and stays a plain
    // selectable_value so it works with the backend offline (the outer
    // `add_enabled_ui(ai_enabled, ...)` in tab.rs still disables it under --no-ai).
    ui.horizontal_wrapped(|ui| {
        if let Some(req) = engine_button_requirement(OcrEngine::MangaOcr) {
            actions.options_changed |= engine_select_button(
                ui,
                &mut options.engine,
                OcrEngine::MangaOcr,
                req,
                "MangaOCR",
                t!("translation.ocr_panel.mangaocr_hint"),
            );
        }
        if let Some(req) = engine_button_requirement(OcrEngine::EasyOcr) {
            actions.options_changed |= engine_select_button(
                ui,
                &mut options.engine,
                OcrEngine::EasyOcr,
                req,
                "EasyOCR",
                t!("translation.ocr_panel.easyocr_hint"),
            );
        }
        if let Some(req) = engine_button_requirement(OcrEngine::PaddleOcr) {
            actions.options_changed |= engine_select_button(
                ui,
                &mut options.engine,
                OcrEngine::PaddleOcr,
                req,
                "PaddleOCR",
                t!("translation.ocr_panel.paddleocr_hint"),
            );
        }
        if let Some(req) = engine_button_requirement(OcrEngine::Surya) {
            actions.options_changed |= engine_select_button(
                ui,
                &mut options.engine,
                OcrEngine::Surya,
                req,
                "Surya",
                t!("translation.ocr_panel.paddleocr_full_hint"),
            );
        }
        actions.options_changed |= ui
            .selectable_value(&mut options.engine, OcrEngine::AiApi, "AI API")
            .on_hover_text(t!("translation.ocr_panel.ai_api_hint"))
            .changed();
    });
    // Second engine row keeps wider engines off the first line so the side panel
    // does not grow horizontally when a new engine is added.
    ui.horizontal_wrapped(|ui| {
        if let Some(req) = engine_button_requirement(OcrEngine::PaddleVl) {
            actions.options_changed |= engine_select_button(
                ui,
                &mut options.engine,
                OcrEngine::PaddleVl,
                req,
                "PaddleOCR-VL",
                t!("translation.ocr_panel.vision_language_hint"),
            );
        }
    });
    ui.add_enabled_ui(selected_engine_enabled, |ui| {
        match options.engine {
            OcrEngine::MangaOcr => {
                normalize_selected_manga_model(options);
                ui.label(t!("translation.ocr_panel.mangaocr_model_label"));
                WheelComboBox::from_id_salt("translation_ocr_manga_model")
                    .selected_text(selected_manga_model_label(options))
                    .show_ui(ui, |ui| {
                        actions.options_changed |= ui
                            .selectable_value(
                                &mut options.manga_model,
                                "base_onnx".to_string(),
                                t!("translation.ocr_panel.model_base_onnx"),
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
                            t!("translation.ocr_panel.model_base_pytorch"),
                            // Strict: the torch model is usable only when PyTorch is
                            // known-available (torch runs solely on the backend).
                            AiRequirement::Torch.satisfied(&caps),
                        );
                    });
            }
            OcrEngine::PaddleOcr => {
                let show_all_changed = ui
                    .checkbox(&mut options.paddle_show_full_langs, t!("translation.ocr_panel.show_all_models_label"))
                    .changed();
                actions.options_changed |= show_all_changed;
                if show_all_changed {
                    normalize_selected_paddle_lang(options);
                }
                normalize_selected_paddle_lang(options);
                let available_langs = sorted_language_options(paddle_language_options(
                    options.paddle_show_full_langs,
                ));
                let selected_lang = selected_paddle_language_label(options, &available_langs);
                WheelComboBox::from_label(t!("translation.ocr_panel.paddleocr_model_label")).id_salt("translation.ocr_panel.paddleocr_model_label")
                    .selected_text(selected_lang)
                    .show_ui(ui, |ui| {
                        for (code, title) in &available_langs {
                            actions.options_changed |= ui
                                .selectable_value(
                                    &mut options.paddle_lang,
                                    (*code).to_string(),
                                    format!("{} ({code})", lang_label(title)),
                                )
                                .changed();
                        }
                    });
            }
            OcrEngine::EasyOcr => {
                let show_all_changed = ui
                    .checkbox(&mut options.easy_show_full_langs, t!("translation.ocr_panel.show_all_langs_label"))
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
                    ui.label(t!("translation.ocr_panel.language_label"));
                    WheelComboBox::from_id_salt("translation_ocr_easy_lang_to_add")
                        .selected_text(selected_lang)
                        .show_ui(ui, |ui| {
                            for (code, title) in &available_langs {
                                actions.options_changed |= ui
                                    .selectable_value(
                                        &mut options.easy_lang_to_add,
                                        (*code).to_string(),
                                        format!("{} ({code})", lang_label(title)),
                                    )
                                    .changed();
                            }
                        });
                    if ui.button(t!("translation.common.add_button")).clicked() {
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
                WheelComboBox::from_label(t!("translation.ocr_panel.script_restriction_label")).id_salt("translation.ocr_panel.script_restriction_label")
                    .selected_text(paddle_vl_script_label(&options.paddle_vl_script))
                    .show_ui(ui, |ui| {
                        for (key, label) in PADDLE_VL_SCRIPTS {
                            actions.options_changed |= ui
                                .selectable_value(
                                    &mut options.paddle_vl_script,
                                    (*key).to_string(),
                                    lang_label(label),
                                )
                                .changed();
                        }
                    });
                if options.paddle_vl_script != "auto" {
                    ui.small(
                    t!("translation.ocr_panel.script_restriction_hint"),
                );
                }
            }
            // Surya auto-detects language and needs no selection UI here.
            OcrEngine::Surya => {}
            OcrEngine::AiApi => {
                draw_ai_api_options(ui, options, &mut actions);
            }
        }
    });
    ui.separator();

    let (status_color, status_text) = match state {
        OcrLoadState::NotLoaded => (egui::Color32::GRAY, t!("translation.ocr_panel.status_not_loaded")),
        OcrLoadState::DownloadingModel => (
            egui::Color32::from_rgb(255, 172, 66),
            t!("translation.common.downloading_model_status"),
        ),
        OcrLoadState::Loading => (egui::Color32::from_rgb(255, 172, 66), t!("translation.ocr_panel.status_loading")),
        OcrLoadState::Ready => (egui::Color32::from_rgb(42, 168, 88), t!("translation.ocr_panel.status_ready")),
        OcrLoadState::Error => (egui::Color32::from_rgb(208, 84, 62), t!("translation.ocr_panel.status_error")),
    };
    ui.horizontal(|ui| {
        ui.label(t!("translation.ocr_panel.status_label"));
        ui.colored_label(status_color, status_text);
    });

    if let Some(err) = last_error
        && !err.is_empty()
    {
        ui.label(tf!("translation.ocr_panel.error_detail", err = err));
    }

    ui.separator();
    actions.options_changed |= ui
        .checkbox(&mut options.join_newlines, t!("translation.ocr_panel.keep_line_breaks_label"))
        .changed();
    actions.options_changed |= ui
        .checkbox(
            &mut options.reflect_strings,
            t!("translation.ocr_panel.rtl_columns_label"),
        )
        .changed();
    actions.options_changed |= ui
        .checkbox(
            &mut options.copy_to_clipboard,
            t!("translation.ocr_panel.copy_text_label"),
        )
        .changed();
    actions.options_changed |= ui
        .checkbox(&mut options.create_bubble, t!("translation.ocr_panel.create_bubble_label"))
        .changed();
    draw_char_replacements(ui, options, &mut actions);
    let quick_label = match quick_selection_shortcut {
        Some(shortcut) if !shortcut.is_empty() => {
            tf!("translation.ocr_panel.quick_recognition_shortcut_hint", shortcut = shortcut)
        }
        _ => t!("translation.recognition.quick_title").to_string(),
    };
    let advanced_label = match advanced_selection_shortcut {
        Some(shortcut) if !shortcut.is_empty() => {
            tf!("translation.ocr_panel.advanced_recognition_shortcut_hint", shortcut = shortcut)
        }
        _ => t!("translation.recognition.advanced_title").to_string(),
    };
    if quick_selection_active {
        ui.small(tf!("translation.ocr_panel.quick_active_hint", quick_label = quick_label));
    } else {
        ui.small(tf!("translation.ocr_panel.quick_inactive_hint", quick_label = quick_label));
    }
    if advanced_selection_active {
        ui.small(tf!("translation.ocr_panel.advanced_active_hint", advanced_label = advanced_label));
    } else {
        ui.small(tf!("translation.ocr_panel.advanced_inactive_hint", advanced_label = advanced_label));
    }

    let button_label = if state == OcrLoadState::Ready {
        t!("translation.ocr_panel.reload_engine_button")
    } else {
        t!("translation.ocr_panel.load_engine_button")
    };
    ui.horizontal_wrapped(|ui| {
        let button = ui.add_enabled(
            !state.is_busy() && selected_engine_enabled,
            egui::Button::new(button_label),
        );
        // Surface the requirement's disabled reason on hover and as a colored hint,
        // both derived from the same `selected_mode_requirement` used to gate it.
        let disabled_reason = if selected_engine_enabled {
            None
        } else {
            selected_mode_requirement(options).map(|req| req.disabled_reason(&caps))
        };
        let button = match disabled_reason {
            Some(reason) => button.on_disabled_hover_text(reason),
            None => button,
        };
        if button.clicked() {
            actions.request_load = true;
        }
        if let Some(reason) = disabled_reason {
            ui.colored_label(egui::Color32::from_rgb(240, 102, 102), reason);
        }
    });

    if let Some(result) = last_result
        && !result.text.trim().is_empty()
    {
        ui.separator();
        ui.label(tf!("translation.ocr_panel.last_ocr_lines_status", lines = result.lines.len()));
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
            .on_hover_text(t!("translation.ocr_panel.char_replace_hint"))
            .changed();
        let arrow = if options.replace_chars_expanded {
            "⏷"
        } else {
            "⏵"
        };
        let header = ui
            .add(egui::Label::new(tf!("translation.ocr_panel.char_replace_header", arrow = arrow)).sense(egui::Sense::click()));
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
                    .on_hover_text(t!("translation.ocr_panel.enable_replace_row_label"))
                    .changed();
                actions.options_changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut rule.targets_raw)
                            .desired_width(96.0)
                            .hint_text("'·', '…'"),
                    )
                    .on_hover_text(t!("translation.ocr_panel.replace_from_hint"))
                    .changed();
                ui.label("→");
                actions.options_changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut rule.replacement)
                            .desired_width(64.0)
                            .hint_text("."),
                    )
                    .on_hover_text(t!("translation.ocr_panel.replace_to_hint"))
                    .changed();
                if ui
                    .small_button("🗑")
                    .on_hover_text(t!("translation.ocr_panel.delete_row_button"))
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
        if ui.button(t!("translation.ocr_panel.add_replace_button")).clicked() {
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
                ui.label(t!("translation.common.service_label"));
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
            });
        });
}

fn ai_api_options_max_height(ui: &egui::Ui) -> f32 {
    (ui.ctx().content_rect().height() * 0.7 - OCR_AI_API_PANEL_OUTSIDE_HEIGHT_RESERVE).max(140.0)
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

/// Renders the `base_torch` MangaOCR model button, disabled when PyTorch is not
/// usable. `torch_usable` should be permissive on an unknown torch capability so
/// the button disables only when PyTorch is known-unavailable.
fn disabled_manga_model_choice(
    ui: &mut egui::Ui,
    selected_model: &mut String,
    model_key: &str,
    label: &str,
    torch_usable: bool,
) -> bool {
    let selected = selected_model == model_key;
    let response = ui.add_enabled(torch_usable, egui::Button::new(label).selected(selected));
    let response = if torch_usable {
        response
    } else {
        response.on_disabled_hover_text(
            egui::RichText::new(pytorch_unavailable_hint())
                .color(egui::Color32::from_rgb(240, 102, 102)),
        )
    };
    if response.clicked() {
        *selected_model = model_key.to_string();
        return true;
    }
    false
}

// PaddleOCR-VL writing-system restriction modes: `(wire key, display i18n key)`.
// `auto` keeps the model's native multilingual detection; the others
// hard-restrict decoding to that script. The wire key is the persisted identity
// (must match `script_constraint.normalize_script`); the display key resolves to
// a localized label via `lang_label`.
const PADDLE_VL_SCRIPTS: &[(&str, &str)] = &[
    ("auto", "translation.ocr_panel.script_auto"),
    ("korean", "translation.ocr_panel.script_korean_only"),
    ("chinese", "translation.ocr_panel.script_chinese_only"),
    ("japanese", "translation.ocr_panel.script_japanese_only"),
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
        .map(|(_, label)| lang_label(label))
        .unwrap_or_else(|| lang_label("translation.ocr_panel.script_auto"))
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
        "base_torch" => t!("translation.ocr_panel.model_base_pytorch").to_string(),
        _ => t!("translation.ocr_panel.model_base_onnx").to_string(),
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

fn selected_paddle_language_label(
    options: &OcrPanelOptions,
    langs: &[(&'static str, &'static str)],
) -> String {
    langs
        .iter()
        .find(|(code, _)| *code == options.paddle_lang.as_str())
        .map(|(code, title)| format!("{} ({code})", lang_label(title)))
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

fn selected_easy_language_label(
    options: &OcrPanelOptions,
    langs: &[(&'static str, &'static str)],
) -> String {
    langs
        .iter()
        .find(|(code, _)| *code == options.easy_lang_to_add.as_str())
        .map(|(code, title)| format!("{} ({code})", lang_label(title)))
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
    ui.label(t!("translation.ocr_panel.easyocr_active_langs_label"));
    if langs.is_empty() {
        ui.small(t!("translation.ocr_panel.no_langs_selected"));
        return false;
    }

    let mut to_remove: Option<String> = None;
    for code in &langs {
        let label = easy_lang_chip_label(code);
        ui.horizontal(|ui| {
            ui.label(label);
            if ui.small_button("x").on_hover_text(t!("translation.ocr_panel.remove_lang_button")).clicked() {
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
        Some(title) => format!("{} ({code})", lang_label(title)),
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
    // Sort by the localized display label so the dropdown is alphabetical in the
    // active UI language, not by the internal catalog key.
    out.sort_by(|(code_a, title_a), (code_b, title_b)| {
        lang_label(title_a)
            .to_lowercase()
            .cmp(&lang_label(title_b).to_lowercase())
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
    use super::{
        AiRequirement, OcrEngine, OcrPanelOptions, engine_button_requirement,
        parse_replacement_targets, selected_mode_requirement,
    };

    #[test]
    fn selected_mode_requirement_is_model_aware() {
        let torch = OcrPanelOptions {
            engine: OcrEngine::MangaOcr,
            manga_model: "base_torch".to_string(),
            ..Default::default()
        };
        assert_eq!(
            selected_mode_requirement(&torch),
            Some(AiRequirement::Torch)
        );

        let onnx = OcrPanelOptions {
            engine: OcrEngine::MangaOcr,
            manga_model: "base_onnx".to_string(),
            ..Default::default()
        };
        assert_eq!(selected_mode_requirement(&onnx), Some(AiRequirement::Onnx));

        let paddle = OcrPanelOptions {
            engine: OcrEngine::PaddleOcr,
            ..Default::default()
        };
        assert_eq!(
            selected_mode_requirement(&paddle),
            Some(AiRequirement::Onnx)
        );

        let ai_api = OcrPanelOptions {
            engine: OcrEngine::AiApi,
            ..Default::default()
        };
        assert_eq!(selected_mode_requirement(&ai_api), None);
    }

    #[test]
    fn engine_button_requirement_is_permissive_for_manga() {
        assert_eq!(
            engine_button_requirement(OcrEngine::MangaOcr),
            Some(AiRequirement::TorchOrOnnx)
        );
        assert_eq!(engine_button_requirement(OcrEngine::AiApi), None);
    }

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
