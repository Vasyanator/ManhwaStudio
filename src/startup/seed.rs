/*
FILE OVERVIEW: src/startup/seed.rs
Seeds config-free process-global settings of lower crates from `user_config` at startup (the app
owns the config, those crates do not read it), and persists an auto-detected
`General.ai_install_type` when that key is absent. Every function is best-effort: a config error
keeps the compiled-in default and is logged where it matters. Called by `services::init_full`
(its `PRE_SETTINGS_STEPS`) in a fixed order right after the first-run language marker. Each seed
reads `config::load_user_config` itself on purpose (not the settings snapshot; see
"Seeds and the settings snapshot" in `MODULE_README.md`).
*/

use crate::{ai_install_probe, config, language, runtime_log, tabs, text_punctuation};

/// Seeds the config-free `ms_text_util::text_punctuation` set from user config at
/// startup. The util crate defaults to `DEFAULT_HANGING_PUNCTUATION`; here the app
/// (which owns config) overrides it with `TextTab.hanging_punctuation` when present
/// and non-blank. Best-effort: on any config error the default set is kept.
pub(super) fn seed_hanging_punctuation_from_config() {
    let Ok(cfg) = config::load_user_config() else {
        return;
    };
    if let Some(text) = cfg
        .get_path(&["TextTab", "hanging_punctuation"])
        .and_then(serde_json::Value::as_str)
        .filter(|text| text.chars().any(|ch| !ch.is_whitespace()))
    {
        text_punctuation::set_hanging_punctuation(text);
    }
}

/// Seeds the config-free `ms_text_util::language` typesetting language from user
/// config at startup. The util crate defaults to `TextLanguage::Ru` (preserving
/// the historical Russian-only behavior); here the app (which owns config)
/// overrides it with the `TextTab.text_language` tag when present and recognized.
/// An absent or unrecognized tag keeps `Ru` and logs a warning — never a panic.
pub(super) fn seed_text_language_from_config() {
    let Ok(cfg) = config::load_user_config() else {
        return;
    };
    let Some(raw) = cfg
        .get_path(&["TextTab", config::TEXT_TAB_TEXT_LANGUAGE_KEY])
        .and_then(serde_json::Value::as_str)
    else {
        // Key absent (older config): keep the default, no warning needed.
        return;
    };
    match language::TextLanguage::from_tag(raw.trim()) {
        Some(text_language) => language::set_text_language(text_language),
        None => runtime_log::log_warn(format!(
            "[startup] unrecognized TextTab.text_language {raw:?}; keeping default 'ru'"
        )),
    }
}

/// Seeds the runtime `rotation_ctrl_wheel` global from user config at startup.
/// The module defaults to `DEFAULT_ROTATION_CTRL_WHEEL_MODE` (`Vector`); here the app
/// overrides it with `TextTab.rotation_ctrl_wheel_mode` when present and recognized.
/// Best-effort: on any config error or unknown value the default mode is kept.
pub(super) fn seed_rotation_ctrl_wheel_from_config() {
    let Ok(cfg) = config::load_user_config() else {
        return;
    };
    use tabs::typing::rotation_ctrl_wheel;
    if let Some(mode) = cfg
        .get_path(&["TextTab", "rotation_ctrl_wheel_mode"])
        .and_then(serde_json::Value::as_str)
        .and_then(rotation_ctrl_wheel::RotationCtrlWheelMode::from_config_str)
    {
        rotation_ctrl_wheel::set_rotation_ctrl_wheel_mode(mode);
    }
}

/// Seeds the runtime-global advanced text-form search knobs from user config at
/// startup. The module defaults every knob; here the app overrides the fields
/// `TextTab.advanced_form_search` actually names.
///
/// Best-effort and PARTIAL-TOLERANT: a config error, a non-object value or a
/// missing/invalid field keeps that field's compiled-in default (the decoding rule
/// belongs to `AdvancedFormParams::from_config_value`, which also clamps the result
/// into the supported ranges).
pub(super) fn seed_advanced_form_search_from_config() {
    let Ok(cfg) = config::load_user_config() else {
        return;
    };
    use tabs::typing::advanced_form_params;
    if let Some(stored) = cfg.get_path(&["TextTab", config::TEXT_TAB_ADVANCED_FORM_SEARCH_KEY]) {
        advanced_form_params::set_advanced_form_params(
            advanced_form_params::AdvancedFormParams::from_config_value(stored),
        );
    }
}

/// When `General.ai_install_type` is absent, probes the Python AI environment
/// (`ai_install_probe`) and persists the detected type. Best-effort: every failure is logged as a
/// warning and leaves the key absent.
pub(super) fn auto_detect_missing_ai_install_type_for_startup() {
    let raw_settings = match config::load_raw_user_settings_for_startup() {
        Ok(settings) => settings,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[startup] failed to read user config before AI install-type detection: {err:#}"
            ));
            return;
        }
    };
    if config::user_settings_has_ai_install_type(&raw_settings) {
        return;
    }

    runtime_log::log_info("[startup] General.ai_install_type is missing; probing AI environment");
    let app_dir = config::program_dir();
    let report = match ai_install_probe::collect_ai_computations_report(&app_dir) {
        Ok(report) => report,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[startup] failed to auto-detect AI install type: {err}"
            ));
            return;
        }
    };
    let install_type = ai_install_probe::detect_ai_install_type_from_report(&report);
    let mut cfg = match config::load_user_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[startup] failed to load user config for AI install-type persistence: {err:#}"
            ));
            return;
        }
    };
    if let Err(err) = cfg.set_path(
        &["General", config::GENERAL_AI_INSTALL_TYPE_KEY],
        serde_json::Value::String(install_type.as_str().to_string()),
    ) {
        runtime_log::log_warn(format!(
            "[startup] failed to persist auto-detected AI install type: {err:#}"
        ));
        return;
    }
    runtime_log::log_info(format!(
        "[startup] auto-detected General.ai_install_type={}",
        install_type.as_str()
    ));
}
