/*
File: crates/ms-tab-translation/src/tab/settings_io.rs

Purpose:
The tab's title settings: one-time hydration of the OCR / MT / composition / text-detector
options from the title `settings.json`, the Composition text rebuild, the coalescing settings
saver thread and its `ms_docstore::update` write, and the settings key parsers.

Key functions:
- TranslationTabState::ensure_ocr_settings_loaded / ensure_mt_settings_loaded /
  ensure_composition_settings_loaded / ensure_text_detector_settings_loaded /
  flush_settings_save_if_needed / rebuild_composition_text.
- spawn_translation_settings_saver_thread / save_translation_settings_to_project_file /
  apply_translation_settings_sections / composition_options_from_settings.
- parse_ocr_engine_key / ocr_engine_to_project_key / parse_text_detector_algorithm_key /
  paddle_vl_model_from_settings / parse_ocr_lang_text_setting / parse_single_ocr_lang_setting /
  detect_mask_param / normalized_lang_input.

Notes:
- The saver is started by `TranslationTabState::new` and joined by its `Drop` (`tab.rs`).
*/

use ms_project::ProjectData;
use crate::machine_translation::{AiMtContextSource, AiMtImageDetail, AiMtImageMode, AiMtReasoning, AiMtSortMode, MtService};
use crate::ocr::OcrEngine;
use ms_sysprobe::ai_models::external_catalog::PaddleVlVariant;
use ms_ai_api::AiApiService;
use crate::panels::composition::{CompositionPanelOptions, compose_translation_text, read_composition_options_json, write_composition_options_json};
use crate::panels::composition_presets::{CompositionPresetRef, read_selected_preset, write_selected_preset};
use crate::panels::machine_translation::{MtPanelOptions, MtPanelTab};
use crate::panels::ocr::OcrPanelOptions;
use crate::panels::text_detector::{TextDetectorAlgorithm, TextDetectorPanelOptions};
use crate::neurolate::NeurolateSettings;
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use ms_thread::{self as thread, JoinHandle};
use super::{TranslationSettingsSaveRequest, TranslationTabState};
use super::ocr_flow::parse_char_replacement_rules;

impl TranslationTabState {
    pub(super) fn rebuild_composition_text(&mut self, project: &ProjectData) {
        self.composition_panel_options.normalize();
        self.composition_panel_state.composed_text =
            compose_translation_text(project, &self.composition_panel_options);
        self.composition_rebuild_requested = false;
    }

    pub(super) fn ensure_ocr_settings_loaded(&mut self, project: &ProjectData) {
        let settings_path = project.paths.settings_file.clone();
        if self
            .ocr_settings_loaded_for
            .as_ref()
            .is_some_and(|loaded| *loaded == settings_path)
        {
            return;
        }

        let ocr = project.settings_data.get("OCR").and_then(Value::as_object);
        // A missing or unknown variant key loads as the default variant (saved back as its key).
        self.ocr_panel_options.paddle_vl_model = paddle_vl_model_from_settings(ocr);
        // Absent or unknown values load as the defaults: never the previous title's choices.
        self.neurolate_settings = crate::neurolate::settings_from_json(ocr.and_then(|ocr| ocr.get("params")).and_then(Value::as_object));
        if let Some(ocr_obj) = ocr {
            if let Some(engine_raw) = ocr_obj.get("engine").and_then(Value::as_str) {
                self.ocr_panel_options.engine = parse_ocr_engine_key(engine_raw);
            }
            if let Some(join) = ocr_obj.get("join").and_then(Value::as_bool) {
                self.ocr_panel_options.join_newlines = join;
            }
            if let Some(reflect) = ocr_obj.get("reflect").and_then(Value::as_bool) {
                self.ocr_panel_options.reflect_strings = reflect;
            }
            // Absent in projects saved before the option existed; the `true`
            // default from `OcrPanelOptions::default` then stays in force.
            if let Some(fix_caps) = ocr_obj.get("fix_caps_lock").and_then(Value::as_bool) {
                self.ocr_panel_options.fix_caps_lock = fix_caps;
            }
            if let Some(copy) = ocr_obj.get("copy").and_then(Value::as_bool) {
                self.ocr_panel_options.copy_to_clipboard = copy;
            }
            if let Some(bubbles) = ocr_obj.get("bubbles").and_then(Value::as_bool) {
                self.ocr_panel_options.create_bubble = bubbles;
            }
            if let Some(replace_chars) = ocr_obj.get("replace_chars").and_then(Value::as_bool) {
                self.ocr_panel_options.replace_chars_enabled = replace_chars;
            }
            if let Some(rules) = ocr_obj.get("char_replacements").and_then(Value::as_array) {
                self.ocr_panel_options.char_replacements = parse_char_replacement_rules(rules);
            }

            if let Some(params) = ocr_obj.get("params").and_then(Value::as_object) {
                let manga_obj = params
                    .get("mangaocr")
                    .or_else(|| params.get("manga"))
                    .and_then(Value::as_object);
                if let Some(manga) = manga_obj
                    && let Some(model) = manga.get("model").and_then(Value::as_str)
                {
                    let trimmed = model.trim();
                    if !trimmed.is_empty() {
                        self.ocr_panel_options.manga_model = trimmed.to_string();
                    }
                }
                let paddle_obj = params
                    .get("paddle")
                    .or_else(|| params.get("paddleocr"))
                    .and_then(Value::as_object);
                if let Some(paddle) = paddle_obj {
                    if let Some(lang) = parse_single_ocr_lang_setting(paddle.get("langs")) {
                        self.ocr_panel_options.paddle_lang = lang;
                    }
                    if let Some(show_full) = paddle.get("full_langs").and_then(Value::as_bool) {
                        self.ocr_panel_options.paddle_show_full_langs = show_full;
                    }
                    if let Some(detect_mask) = detect_mask_param(paddle) {
                        self.ocr_panel_options.paddle_detect_mask = detect_mask;
                    }
                }
                let paddle_vl_obj = params
                    .get("paddle_vl")
                    .or_else(|| params.get("paddleocrvl"))
                    .and_then(Value::as_object);
                if let Some(paddle_vl) = paddle_vl_obj
                    && let Some(script) = paddle_vl.get("script").and_then(Value::as_str)
                {
                    let trimmed = script.trim();
                    if !trimmed.is_empty() {
                        self.ocr_panel_options.paddle_vl_script = trimmed.to_ascii_lowercase();
                    }
                }
                let easy_obj = params
                    .get("easyocr")
                    .or_else(|| params.get("easy"))
                    .and_then(Value::as_object);
                if let Some(easy) = easy_obj {
                    if let Some(lang_text) = parse_ocr_lang_text_setting(easy.get("langs")) {
                        self.ocr_panel_options.easy_langs = lang_text;
                    }
                    if let Some(show_full) = easy.get("full_langs").and_then(Value::as_bool) {
                        self.ocr_panel_options.easy_show_full_langs = show_full;
                    }
                }
                let surya_obj = params
                    .get("surya")
                    .or_else(|| params.get("suryaocr"))
                    .and_then(Value::as_object);
                if let Some(surya) = surya_obj {
                    if let Some(task_name) = surya.get("task").and_then(Value::as_str) {
                        let trimmed = task_name.trim();
                        if !trimmed.is_empty() {
                            self.ocr_panel_options.surya_task_name = trimmed.to_string();
                        }
                    }
                    if let Some(recognize_math) =
                        surya.get("recognize_math").and_then(Value::as_bool)
                    {
                        self.ocr_panel_options.surya_recognize_math = recognize_math;
                    }
                    if let Some(sort_lines) = surya.get("sort_lines").and_then(Value::as_bool) {
                        self.ocr_panel_options.surya_sort_lines = sort_lines;
                    }
                    if let Some(drop_repeated_text) =
                        surya.get("drop_repeated_text").and_then(Value::as_bool)
                    {
                        self.ocr_panel_options.surya_drop_repeated_text = drop_repeated_text;
                    }
                    if let Some(max_sliding_window) =
                        surya.get("max_sliding_window").and_then(Value::as_u64)
                        && let Ok(value) = u32::try_from(max_sliding_window)
                    {
                        self.ocr_panel_options.surya_max_sliding_window = value;
                    }
                    if let Some(max_tokens) = surya.get("max_tokens").and_then(Value::as_u64)
                        && let Ok(value) = u32::try_from(max_tokens)
                    {
                        self.ocr_panel_options.surya_max_tokens = value;
                    }
                    if let Some(detect_mask) = detect_mask_param(surya) {
                        self.ocr_panel_options.surya_detect_mask = detect_mask;
                    }
                }
                let ai_api_obj = params
                    .get("ai_api")
                    .or_else(|| params.get("aiapi"))
                    .and_then(Value::as_object);
                if let Some(ai_api) = ai_api_obj {
                    if let Some(service) = ai_api.get("service").and_then(Value::as_str) {
                        let service = AiApiService::from_key(service);
                        self.ocr_panel_options.ai_api.service = service;
                        // A compatible service has no default model: never keep the hosted default
                        // of `AiApiConnectionState::new` under it when no model was saved.
                        self.ocr_panel_options.ai_api.model = service.default_model().to_string();
                    }
                    // Absent in settings written before compatible services existed: the field keeps
                    // its current value (empty in a fresh state, else the previously loaded
                    // settings' URL). A key is bound to its exact URL, so a kept URL never sends
                    // a key saved for another server.
                    if let Some(base_url) = ai_api.get("base_url").and_then(Value::as_str) {
                        self.ocr_panel_options.ai_api.base_url = base_url.trim().to_string();
                    }
                    if let Some(model) = ai_api.get("model").and_then(Value::as_str) {
                        let trimmed = model.trim();
                        if !trimmed.is_empty() {
                            self.ocr_panel_options.ai_api.model = trimmed.to_string();
                        }
                    }
                    if let Some(system_instruction) =
                        ai_api.get("system_instruction").and_then(Value::as_str)
                    {
                        self.ocr_panel_options.ai_api.system_instruction =
                            system_instruction.to_string();
                    }
                }
            }
        }

        self.ocr_settings_loaded_for = Some(settings_path);
        self.ocr_settings_dirty = false;
    }

    pub(super) fn ensure_mt_settings_loaded(&mut self, project: &ProjectData) {
        let settings_path = project.paths.settings_file.clone();
        if self
            .mt_settings_loaded_for
            .as_ref()
            .is_some_and(|loaded| *loaded == settings_path)
        {
            return;
        }

        if let Some(mt_obj) = project
            .settings_data
            .get("machine_translation")
            .and_then(Value::as_object)
        {
            if let Some(service_key) = mt_obj.get("service").and_then(Value::as_str)
                && let Some(service) = MtService::from_key(service_key)
            {
                self.mt_panel_options.service = service;
            }
            if let Some(source_lang) = mt_obj.get("source_lang").and_then(Value::as_str) {
                self.mt_panel_options.source_lang = source_lang.to_string();
            }
            if let Some(target_lang) = mt_obj.get("target_lang").and_then(Value::as_str) {
                self.mt_panel_options.target_lang = target_lang.to_string();
            }
            if let Some(active_tab) = mt_obj.get("active_tab").and_then(Value::as_str) {
                self.mt_panel_options.active_tab = if active_tab.eq_ignore_ascii_case("ai_api") {
                    MtPanelTab::AiApi
                } else {
                    MtPanelTab::Machine
                };
            }
            if let Some(ai_obj) = mt_obj.get("ai_api").and_then(Value::as_object) {
                if let Some(service) = ai_obj.get("service").and_then(Value::as_str) {
                    let service = AiApiService::from_key(service);
                    self.mt_panel_options.ai_api.service = service;
                    // As for OCR: a saved service without a saved model gets its own default.
                    self.mt_panel_options.ai_api.model = service.default_model().to_string();
                }
                // Absent in settings written before compatible services existed: kept as is, as
                // for OCR above (empty only in a fresh state).
                if let Some(base_url) = ai_obj.get("base_url").and_then(Value::as_str) {
                    self.mt_panel_options.ai_api.base_url = base_url.trim().to_string();
                }
                if let Some(model) = ai_obj.get("model").and_then(Value::as_str)
                    && !model.trim().is_empty()
                {
                    self.mt_panel_options.ai_api.model = model.trim().to_string();
                }
                if let Some(system_instruction) =
                    ai_obj.get("system_instruction").and_then(Value::as_str)
                {
                    self.mt_panel_options.ai_api.system_instruction =
                        system_instruction.to_string();
                }
                if let Some(sort_mode) = ai_obj.get("sort_mode").and_then(Value::as_str) {
                    self.mt_panel_options.ai_sort_mode = AiMtSortMode::from_key(sort_mode);
                }
                if let Some(value) = ai_obj.get("use_character_names").and_then(Value::as_bool) {
                    self.mt_panel_options.ai_use_character_names = value;
                }
                if let Some(value) = ai_obj.get("use_notes_prompt").and_then(Value::as_bool) {
                    self.mt_panel_options.ai_use_notes_prompt = value;
                }
                if let Some(value) = ai_obj.get("include_characters").and_then(Value::as_bool) {
                    self.mt_panel_options.ai_include_characters = value;
                }
                if let Some(value) = ai_obj.get("include_terms").and_then(Value::as_bool) {
                    self.mt_panel_options.ai_include_terms = value;
                }
                if let Some(value) = ai_obj
                    .get("include_existing_translation")
                    .and_then(Value::as_bool)
                {
                    self.mt_panel_options.ai_include_existing_translation = value;
                }
                if let Some(value) = ai_obj.get("include_image_bubbles").and_then(Value::as_bool) {
                    self.mt_panel_options.ai_include_image_bubbles = value;
                }
                // Opt-in: a missing key (settings written before the option existed) reads as off.
                self.mt_panel_options.ai_strict_json = ai_obj.get("strict_json").and_then(Value::as_bool).unwrap_or(false);
                if let Some(value) = ai_obj.get("image_detail").and_then(Value::as_str) {
                    self.mt_panel_options.ai_image_detail = AiMtImageDetail::from_key(value);
                } else if let Some(value) =
                    ai_obj.get("image_encoding_quality").and_then(Value::as_str)
                {
                    self.mt_panel_options.ai_image_detail = AiMtImageDetail::from_key(value);
                }
                if let Some(value) = ai_obj.get("image_mode").and_then(Value::as_str) {
                    self.mt_panel_options.ai_image_mode = AiMtImageMode::from_key(value);
                }
                if let Some(value) = ai_obj.get("image_context_source").and_then(Value::as_str) {
                    self.mt_panel_options.ai_image_context_source =
                        AiMtContextSource::from_key(value);
                }
                if let Some(value) = ai_obj.get("batch_size").and_then(Value::as_u64)
                    && let Ok(value) = usize::try_from(value)
                {
                    self.mt_panel_options.ai_batch_size = value.clamp(1, 100);
                }
                if let Some(value) = ai_obj.get("reasoning").and_then(Value::as_str) {
                    self.mt_panel_options.ai_reasoning = AiMtReasoning::from_key(value);
                }
                if let Some(value) = ai_obj.get("context_limit_percent").and_then(Value::as_u64)
                    && let Ok(value) = u8::try_from(value)
                {
                    self.mt_panel_options.ai_context_limit_percent = value.clamp(10, 100);
                }
            }
        }

        self.mt_panel_options.source_lang =
            normalized_lang_input(&self.mt_panel_options.source_lang, "auto");
        self.mt_panel_options.target_lang =
            normalized_lang_input(&self.mt_panel_options.target_lang, "ru");
        self.mt_settings_loaded_for = Some(settings_path);
        self.mt_settings_dirty = false;
    }

    pub(super) fn ensure_composition_settings_loaded(&mut self, project: &ProjectData) {
        let settings_path = project.paths.settings_file.clone();
        if self
            .composition_settings_loaded_for
            .as_ref()
            .is_some_and(|loaded| *loaded == settings_path)
        {
            return;
        }

        // The selected preset is re-selected without re-applying it: the title's own options win.
        let composition = project.settings_data.get("composition").and_then(Value::as_object);
        self.composition_panel_options = composition_options_from_settings(composition);
        self.composition_presets.restore_selection(composition.and_then(read_selected_preset));
        self.composition_settings_loaded_for = Some(settings_path);
        self.composition_settings_dirty = false;
        self.composition_rebuild_requested = true;
    }

    pub(super) fn ensure_text_detector_settings_loaded(&mut self, project: &ProjectData) {
        let settings_path = project.paths.settings_file.clone();
        if self
            .text_detector_settings_loaded_for
            .as_ref()
            .is_some_and(|loaded| *loaded == settings_path)
        {
            return;
        }

        if let Some(det_obj) = project
            .settings_data
            .get("text_detector")
            .and_then(Value::as_object)
        {
            if let Some(value) = det_obj.get("algorithm").and_then(Value::as_str) {
                self.text_detector_options.algorithm = parse_text_detector_algorithm_key(value);
            }
            if let Some(value) = det_obj.get("draw_lines").and_then(Value::as_bool) {
                self.text_detector_options.draw_lines = value;
            }
            if let Some(value) = det_obj.get("draw_mask").and_then(Value::as_bool) {
                self.text_detector_options.draw_mask = value;
            }
            if let Some(value) = det_obj.get("block_expand_px").and_then(Value::as_i64) {
                self.text_detector_options.block_expand_px = (value as i32).clamp(0, 200);
            }
            if let Some(value) = det_obj.get("merge_gap_px").and_then(Value::as_i64) {
                self.text_detector_options.merge_gap_px = (value as i32).clamp(0, 200);
            }
            if let Some(value) = det_obj.get("mask_dilate_size").and_then(Value::as_i64) {
                self.text_detector_options.mask_dilate_size = (value as i32).clamp(0, 30);
            }
            if let Some(params_obj) = det_obj.get("params").and_then(Value::as_object) {
                if let Some(value) = params_obj.get("detect_size").and_then(Value::as_i64) {
                    self.text_detector_options.ai_detect_size = (value as i32).clamp(896, 2048);
                }
                if let Some(value) = params_obj.get("mask dilate size").and_then(Value::as_i64)
                    && det_obj.get("mask_dilate_size").is_none()
                {
                    self.text_detector_options.mask_dilate_size = (value as i32).clamp(0, 30);
                }
            }
        }

        self.text_detector_settings_loaded_for = Some(settings_path);
        self.text_detector_settings_dirty = false;
    }

    pub(super) fn flush_settings_save_if_needed(&mut self, project: &ProjectData) {
        if !self.ocr_settings_dirty
            && !self.mt_settings_dirty
            && !self.composition_settings_dirty
            && !self.text_detector_settings_dirty
        {
            return;
        }
        let settings_file = project.paths.settings_file.clone();
        let request = TranslationSettingsSaveRequest {
            settings_file,
            ocr_options: self.ocr_panel_options.clone(),
            mt_options: self.mt_panel_options.clone(),
            composition_options: self.composition_panel_options.clone(),
            composition_preset: self.composition_presets.selection().cloned(),
            text_detector_options: self.text_detector_options.clone(),
            neurolate: self.neurolate_settings,
        };
        if self.settings_save_tx.send(request).is_ok() {
            self.ocr_settings_dirty = false;
            self.mt_settings_dirty = false;
            self.composition_settings_dirty = false;
            self.text_detector_settings_dirty = false;
        }
    }
}

pub(super) fn spawn_translation_settings_saver_thread()
-> (Sender<TranslationSettingsSaveRequest>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<TranslationSettingsSaveRequest>();
    let handle = thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            if first.settings_file.as_os_str().is_empty() {
                break;
            }
            let mut latest = first;
            while let Ok(next) = rx.try_recv() {
                if next.settings_file.as_os_str().is_empty() {
                    return;
                }
                latest = next;
            }
            if let Err(err) = save_translation_settings_to_project_file(
                &latest.settings_file,
                &latest.ocr_options,
                &latest.mt_options,
                &latest.composition_options,
                latest.composition_preset.as_ref(),
                &latest.text_detector_options,
                &latest.neurolate,
            ) {
                // Structured log: a malformed `settings.json` is now left untouched, so this
                // is where the user-facing trace of a skipped save lives.
                ms_log::runtime_log::log_error(format!(
                    "[translation::settings_saver] failed to persist translation settings; path={} error={err} possible_cause=malformed or unwritable settings.json (the file was left untouched)",
                    latest.settings_file.display()
                ));
            }
        }
    });
    (tx, handle)
}

/// Persists the translation tab's four sections (`OCR`, `machine_translation`, `composition`,
/// `text_detector`) into the title `settings.json` through the serialized
/// `ms_docstore::update`, so concurrent writers of other keys are never reverted.
/// `composition_preset` is the selected composition preset, written as `composition.preset`
/// (`None` removes the key).
///
/// # Errors
/// Returns a technical message when the existing file cannot be read, does not parse, or
/// its root is not a JSON object (in every case it is left untouched), or when the atomic
/// write fails.
pub(super) fn save_translation_settings_to_project_file(
    settings_file: &Path,
    ocr_options: &OcrPanelOptions,
    mt_options: &MtPanelOptions,
    composition_options: &CompositionPanelOptions,
    composition_preset: Option<&CompositionPresetRef>,
    text_detector_options: &TextDetectorPanelOptions,
    neurolate: &NeurolateSettings,
) -> Result<(), String> {
    let doc = ms_docstore::DocRef::new(settings_file, ms_docstore::DocKind::ProjectSettings);
    // Default options = the historical layout (2-space pretty, no trailing newline, parent
    // directories created). A malformed or unreadable file is an error and stays untouched,
    // and so is a parseable non-object root (as in the canvas and spellcheck writers of the
    // same document): replacing it would destroy whatever it holds.
    ms_docstore::update(&doc, ms_docstore::WriteOptions::default(), |root| {
        let Some(root_obj) = root.as_object_mut() else {
            return Err(format!("settings root of '{}' is not a JSON object", settings_file.display()));
        };
        apply_translation_settings_sections(root_obj, ocr_options, mt_options, composition_options, text_detector_options, neurolate);
        // `apply_translation_settings_sections` always leaves `composition` an object; the
        // selected preset is written next to the options it was chosen for.
        if let Some(composition_obj) = root_obj.get_mut("composition").and_then(Value::as_object_mut) {
            write_selected_preset(composition_obj, composition_preset);
        }
        Ok(())
    })
    .map_err(|err| format!("failed to save translation settings '{}': {err}", settings_file.display()))
}

/// Keys of `text_detector.params` that older builds wrote and nothing reads any more; the
/// settings writer removes them (reading ignores them).
const STALE_TEXT_DETECTOR_PARAM_KEYS: [&str; 4] =
    ["det_rearrange_max_batches", "font size multiplier", "font size max", "font size min"];

/// A title's composition options from its `settings.json` `composition` object: every key the
/// object lacks (or the whole object when absent) takes its default, never the value of the
/// title loaded before. The result is normalized.
pub(super) fn composition_options_from_settings(composition: Option<&Map<String, Value>>) -> CompositionPanelOptions {
    let mut options = CompositionPanelOptions::default();
    if let Some(comp_obj) = composition {
        read_composition_options_json(comp_obj, &mut options);
    }
    options.normalize();
    options
}

/// Writes the four sections the translation tab owns (`OCR`, `machine_translation`,
/// `composition`, `text_detector`) into the `settings.json` root; other keys are untouched.
/// The NeuroLate settings go under `OCR.params.neurolate`.
pub(super) fn apply_translation_settings_sections(
    root_obj: &mut Map<String, Value>,
    ocr_options: &OcrPanelOptions,
    mt_options: &MtPanelOptions,
    composition_options: &CompositionPanelOptions,
    text_detector_options: &TextDetectorPanelOptions,
    neurolate: &NeurolateSettings,
) {
    let mut ocr_obj = root_obj
        .get("OCR")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    ocr_obj.insert(
        "engine".to_string(),
        Value::String(ocr_engine_to_project_key(ocr_options.engine).to_string()),
    );
    ocr_obj.insert("join".to_string(), Value::Bool(ocr_options.join_newlines));
    ocr_obj.insert(
        "reflect".to_string(),
        Value::Bool(ocr_options.reflect_strings),
    );
    ocr_obj.insert(
        "fix_caps_lock".to_string(),
        Value::Bool(ocr_options.fix_caps_lock),
    );
    ocr_obj.insert(
        "copy".to_string(),
        Value::Bool(ocr_options.copy_to_clipboard),
    );
    ocr_obj.insert(
        "bubbles".to_string(),
        Value::Bool(ocr_options.create_bubble),
    );
    ocr_obj.insert(
        "replace_chars".to_string(),
        Value::Bool(ocr_options.replace_chars_enabled),
    );
    ocr_obj.insert(
        "char_replacements".to_string(),
        Value::Array(
            ocr_options
                .char_replacements
                .iter()
                .map(|rule| {
                    let mut rule_obj = Map::new();
                    rule_obj.insert("enabled".to_string(), Value::Bool(rule.enabled));
                    rule_obj.insert(
                        "targets".to_string(),
                        Value::String(rule.targets_raw.clone()),
                    );
                    rule_obj.insert(
                        "replacement".to_string(),
                        Value::String(rule.replacement.clone()),
                    );
                    Value::Object(rule_obj)
                })
                .collect(),
        ),
    );

    let mut params_obj = ocr_obj
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut manga_obj = params_obj
        .get("mangaocr")
        .or_else(|| params_obj.get("manga"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    manga_obj.insert(
        "model".to_string(),
        Value::String(ocr_options.manga_model.clone()),
    );
    params_obj.insert("mangaocr".to_string(), Value::Object(manga_obj));
    let mut easy_obj = params_obj
        .get("easyocr")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    easy_obj.insert(
        "langs".to_string(),
        Value::String(ocr_options.easy_langs.clone()),
    );
    easy_obj.insert(
        "full_langs".to_string(),
        Value::Bool(ocr_options.easy_show_full_langs),
    );
    if !easy_obj.contains_key("gpu") {
        easy_obj.insert("gpu".to_string(), Value::Bool(false));
    }
    params_obj.insert("easyocr".to_string(), Value::Object(easy_obj));
    let mut paddle_obj = params_obj
        .get("paddle")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    paddle_obj.insert(
        "langs".to_string(),
        Value::String(ocr_options.paddle_lang.clone()),
    );
    paddle_obj.insert(
        "full_langs".to_string(),
        Value::Bool(ocr_options.paddle_show_full_langs),
    );
    paddle_obj.insert("detect_mask".to_string(), Value::Bool(ocr_options.paddle_detect_mask));
    if !paddle_obj.contains_key("gpu") {
        paddle_obj.insert("gpu".to_string(), Value::Bool(false));
    }
    params_obj.insert("paddle".to_string(), Value::Object(paddle_obj));
    let mut paddle_vl_obj = params_obj
        .get("paddle_vl")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    paddle_vl_obj.insert(
        "script".to_string(),
        Value::String(ocr_options.paddle_vl_script.clone()),
    );
    paddle_vl_obj.insert(
        "model".to_string(),
        Value::String(ocr_options.paddle_vl_model.key().to_string()),
    );
    params_obj.insert("paddle_vl".to_string(), Value::Object(paddle_vl_obj));
    let mut surya_obj = params_obj
        .get("surya")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    surya_obj.insert(
        "task".to_string(),
        Value::String(ocr_options.surya_task_name.clone()),
    );
    surya_obj.insert(
        "recognize_math".to_string(),
        Value::Bool(ocr_options.surya_recognize_math),
    );
    surya_obj.insert(
        "sort_lines".to_string(),
        Value::Bool(ocr_options.surya_sort_lines),
    );
    surya_obj.insert(
        "drop_repeated_text".to_string(),
        Value::Bool(ocr_options.surya_drop_repeated_text),
    );
    surya_obj.insert(
        "max_sliding_window".to_string(),
        Value::Number(serde_json::Number::from(u64::from(
            ocr_options.surya_max_sliding_window,
        ))),
    );
    surya_obj.insert(
        "max_tokens".to_string(),
        Value::Number(serde_json::Number::from(u64::from(
            ocr_options.surya_max_tokens,
        ))),
    );
    surya_obj.insert("detect_mask".to_string(), Value::Bool(ocr_options.surya_detect_mask));
    params_obj.insert("surya".to_string(), Value::Object(surya_obj));
    let mut ai_api_obj = params_obj
        .get("ai_api")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    ai_api_obj.insert(
        "service".to_string(),
        Value::String(ocr_options.ai_api.service.key().to_string()),
    );
    ai_api_obj.insert(
        "base_url".to_string(),
        Value::String(ocr_options.ai_api.base_url.trim().to_string()),
    );
    ai_api_obj.insert(
        "model".to_string(),
        Value::String(ocr_options.ai_api.model.clone()),
    );
    ai_api_obj.insert(
        "system_instruction".to_string(),
        Value::String(ocr_options.ai_api.system_instruction.clone()),
    );
    params_obj.insert("ai_api".to_string(), Value::Object(ai_api_obj));
    neurolate.write_json(&mut params_obj);
    ocr_obj.insert("params".to_string(), Value::Object(params_obj));
    root_obj.insert("OCR".to_string(), Value::Object(ocr_obj));

    let mut mt_obj = root_obj
        .get("machine_translation")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    mt_obj.insert(
        "service".to_string(),
        Value::String(mt_options.service.key().to_string()),
    );
    mt_obj.insert(
        "source_lang".to_string(),
        Value::String(normalized_lang_input(&mt_options.source_lang, "auto")),
    );
    mt_obj.insert(
        "target_lang".to_string(),
        Value::String(normalized_lang_input(&mt_options.target_lang, "ru")),
    );
    mt_obj.insert(
        "active_tab".to_string(),
        Value::String(
            match mt_options.active_tab {
                MtPanelTab::Machine => "machine_translation",
                MtPanelTab::AiApi => "ai_api",
            }
            .to_string(),
        ),
    );
    let mut mt_ai_obj = mt_obj
        .get("ai_api")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    mt_ai_obj.insert(
        "service".to_string(),
        Value::String(mt_options.ai_api.service.key().to_string()),
    );
    mt_ai_obj.insert(
        "base_url".to_string(),
        Value::String(mt_options.ai_api.base_url.trim().to_string()),
    );
    mt_ai_obj.insert(
        "model".to_string(),
        Value::String(mt_options.ai_api.model.clone()),
    );
    mt_ai_obj.insert(
        "system_instruction".to_string(),
        Value::String(mt_options.ai_api.system_instruction.clone()),
    );
    mt_ai_obj.insert(
        "sort_mode".to_string(),
        Value::String(mt_options.ai_sort_mode.key().to_string()),
    );
    mt_ai_obj.insert(
        "use_character_names".to_string(),
        Value::Bool(mt_options.ai_use_character_names),
    );
    mt_ai_obj.insert(
        "use_notes_prompt".to_string(),
        Value::Bool(mt_options.ai_use_notes_prompt),
    );
    mt_ai_obj.insert(
        "include_characters".to_string(),
        Value::Bool(mt_options.ai_include_characters),
    );
    mt_ai_obj.insert(
        "include_terms".to_string(),
        Value::Bool(mt_options.ai_include_terms),
    );
    mt_ai_obj.insert(
        "include_existing_translation".to_string(),
        Value::Bool(mt_options.ai_include_existing_translation),
    );
    mt_ai_obj.insert(
        "include_image_bubbles".to_string(),
        Value::Bool(mt_options.ai_include_image_bubbles),
    );
    mt_ai_obj.insert(
        "strict_json".to_string(),
        Value::Bool(mt_options.ai_strict_json),
    );
    mt_ai_obj.insert(
        "image_detail".to_string(),
        Value::String(mt_options.ai_image_detail.key().to_string()),
    );
    mt_ai_obj.insert(
        "image_mode".to_string(),
        Value::String(mt_options.ai_image_mode.key().to_string()),
    );
    mt_ai_obj.insert(
        "image_context_source".to_string(),
        Value::String(mt_options.ai_image_context_source.key().to_string()),
    );
    mt_ai_obj.insert(
        "batch_size".to_string(),
        Value::Number(serde_json::Number::from(
            mt_options.ai_batch_size.clamp(1, 100),
        )),
    );
    mt_ai_obj.insert(
        "reasoning".to_string(),
        Value::String(mt_options.ai_reasoning.key().to_string()),
    );
    mt_ai_obj.insert(
        "context_limit_percent".to_string(),
        Value::Number(serde_json::Number::from(
            mt_options.ai_context_limit_percent.clamp(10, 100),
        )),
    );
    mt_obj.insert("ai_api".to_string(), Value::Object(mt_ai_obj));
    mt_obj.remove("threads");
    mt_obj.remove("params");
    root_obj.insert("machine_translation".to_string(), Value::Object(mt_obj));

    let mut composition_obj = root_obj
        .get("composition")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    write_composition_options_json(&mut composition_obj, composition_options);
    root_obj.insert("composition".to_string(), Value::Object(composition_obj));

    let mut text_detector_obj = root_obj
        .get("text_detector")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    text_detector_obj.insert(
        "algorithm".to_string(),
        Value::String(text_detector_options.algorithm.key().to_string()),
    );
    text_detector_obj.insert(
        "draw_lines".to_string(),
        Value::Bool(text_detector_options.draw_lines),
    );
    text_detector_obj.insert(
        "draw_mask".to_string(),
        Value::Bool(text_detector_options.draw_mask),
    );
    text_detector_obj.insert(
        "block_expand_px".to_string(),
        Value::Number((text_detector_options.block_expand_px.clamp(0, 200) as i64).into()),
    );
    text_detector_obj.insert(
        "merge_gap_px".to_string(),
        Value::Number((text_detector_options.merge_gap_px.clamp(0, 200) as i64).into()),
    );
    let mut text_detector_params = text_detector_obj
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    text_detector_params.remove("device");
    // Retired CTD params (the backend never used them; removed from the panel): strip them so
    // project settings written before their removal do not keep stale entries.
    for stale in STALE_TEXT_DETECTOR_PARAM_KEYS {
        text_detector_params.remove(stale);
    }
    text_detector_params.insert(
        "detect_size".to_string(),
        Value::Number((text_detector_options.ai_detect_size.clamp(896, 2048) as i64).into()),
    );
    text_detector_params.insert(
        "mask dilate size".to_string(),
        Value::Number((text_detector_options.mask_dilate_size.clamp(0, 30) as i64).into()),
    );
    text_detector_obj.insert(
        "mask_dilate_size".to_string(),
        Value::Number((text_detector_options.mask_dilate_size.clamp(0, 30) as i64).into()),
    );
    text_detector_obj.insert("params".to_string(), Value::Object(text_detector_params));
    root_obj.insert(
        "text_detector".to_string(),
        Value::Object(text_detector_obj),
    );
}

pub(super) fn normalized_lang_input(raw: &str, fallback: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

pub(super) fn parse_text_detector_algorithm_key(raw: &str) -> TextDetectorAlgorithm {
    match raw.trim().to_ascii_lowercase().as_str() {
        "paddleocr" | "paddle_ocr" | "paddle-ocr" | "paddle" | "onnx" => {
            TextDetectorAlgorithm::PaddleOcr
        }
        "ai" | "ai_ctd" | "ctd" | "ml" => TextDetectorAlgorithm::Ai,
        "surya" | "suryaocr" | "surya_ocr" | "surya-det" | "surya_det" => {
            TextDetectorAlgorithm::Surya
        }
        "neurolate" => TextDetectorAlgorithm::NeuroLate,
        _ => TextDetectorAlgorithm::Classic,
    }
}

pub(super) fn parse_ocr_engine_key(engine: &str) -> OcrEngine {
    let key = engine.trim().to_ascii_lowercase();
    match key.as_str() {
        "easyocr" | "easy" => OcrEngine::EasyOcr,
        "paddle" | "paddleocr" => OcrEngine::PaddleOcr,
        "paddle_vl" | "paddlevl" | "paddleocr_vl" | "paddleocrvl" | "paddleocr-vl" => {
            OcrEngine::PaddleVl
        }
        "surya" | "suryaocr" | "surya_ocr" => OcrEngine::Surya,
        "baberu" | "baberu_ocr" | "baberuocr" => OcrEngine::Baberu,
        "aiapi" | "ai_api" | "ai-api" | "genai" => OcrEngine::AiApi,
        "neurolate" => OcrEngine::NeuroLate,
        "paddle_onnx" | "paddleonnx" | "paddle-onnx" | "onnx" => OcrEngine::PaddleOcr,
        "mangaocr" | "manga_ocr" | "manga" | "mocr" => OcrEngine::MangaOcr,
        _ => OcrEngine::MangaOcr,
    }
}

pub(super) fn ocr_engine_to_project_key(engine: OcrEngine) -> &'static str {
    match engine {
        OcrEngine::MangaOcr => "mangaocr",
        OcrEngine::EasyOcr => "easyocr",
        OcrEngine::PaddleOcr => "paddle",
        OcrEngine::PaddleVl => "paddle_vl",
        OcrEngine::Surya => "surya",
        OcrEngine::AiApi => "ai_api",
        OcrEngine::Baberu => "baberu",
        OcrEngine::NeuroLate => "neurolate",
    }
}

/// The PaddleOCR-VL variant persisted in an `OCR` settings section
/// (`params.paddle_vl.model`, legacy section alias `params.paddleocrvl`). Missing, non-string
/// or unknown values give `PaddleVlVariant::default()`.
pub(super) fn paddle_vl_model_from_settings(ocr: Option<&Map<String, Value>>) -> PaddleVlVariant {
    ocr.and_then(|ocr| ocr.get("params"))
        .and_then(Value::as_object)
        .and_then(|params| params.get("paddle_vl").or_else(|| params.get("paddleocrvl")))
        .and_then(Value::as_object)
        .and_then(|paddle_vl| paddle_vl.get("model"))
        .and_then(Value::as_str)
        .and_then(PaddleVlVariant::from_key)
        .unwrap_or_default()
}

fn parse_ocr_lang_text_setting(value: Option<&Value>) -> Option<String> {
    let raw = value?;
    match raw {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Array(arr) => {
            let collected = arr
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            if collected.is_empty() {
                None
            } else {
                Some(collected.join(", "))
            }
        }
        _ => None,
    }
}

fn parse_single_ocr_lang_setting(value: Option<&Value>) -> Option<String> {
    let text = parse_ocr_lang_text_setting(value)?;
    let first = text.split(',').next().map(str::trim).unwrap_or("");
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

/// An engine's "create mask and text blocks" parameter from its `OCR.params.<engine>` section;
/// `None` (keep the default) when absent or not a boolean.
pub(super) fn detect_mask_param(engine_params: &Map<String, Value>) -> Option<bool> {
    engine_params.get("detect_mask").and_then(Value::as_bool)
}
