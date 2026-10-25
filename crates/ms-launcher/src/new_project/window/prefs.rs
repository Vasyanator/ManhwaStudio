/*
File: crates/ms-launcher/src/new_project/window/prefs.rs

Purpose:
Persisted New Project window preferences in `user_config`: image URL-prefix presets, the Reline
UI mode, and the ribbon uniform-width / width-fraction settings.

Key items:
- `load_image_url_presets`, `save_image_url_preset`.
- `load_reline_ui_mode`, `save_reline_ui_mode`.
- `load_ribbon_uniform_width` / `save_ribbon_uniform_width`,
  `load_ribbon_width_fraction` / `save_ribbon_width_fraction`.

Notes:
Reads and writes go through `ms_config`; a failed read falls back to the default, a failed
write is logged or returned to the caller (`save_image_url_preset`).
Part of the `window` module split: methods are `impl NewProjectWindowState` blocks over the
type declared in `mod.rs`; items are `pub(super)` and reach siblings through `mod.rs`.
*/

use super::*;

pub(super) fn load_image_url_presets() -> Vec<(String, String)> {
    let config_result = config::load_user_config();
    let cfg = match config_result {
        Ok(cfg) => cfg,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[new-project] failed to load user config for image url presets: {err}"
            ));
            return Vec::new();
        }
    };
    let mut presets = cfg
        .get_path(&["NewProjectWindow", "ImageUrlPrefs"])
        .and_then(serde_json::Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(name, value)| {
                    value
                        .as_str()
                        .map(|prefix| (name.clone(), prefix.to_string()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    presets.sort_by(|left, right| left.0.cmp(&right.0));
    presets
}

pub(super) fn save_image_url_preset(site_name: &str, prefix: &str) -> anyhow::Result<()> {
    let mut cfg = config::load_user_config()?;
    let mut map = cfg
        .get_path(&["NewProjectWindow", "ImageUrlPrefs"])
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    map.insert(
        site_name.trim().to_string(),
        serde_json::Value::String(prefix.trim().to_string()),
    );
    cfg.set_path(
        &["NewProjectWindow", "ImageUrlPrefs"],
        serde_json::Value::Object(map),
    )?;
    Ok(())
}

/// Load the persisted Reline UI mode from user config.
///
/// Returns `RelineUiMode::Simple` when the key is missing or the config cannot be read, so the
/// guided UI is the default on first run.
pub(super) fn load_reline_ui_mode() -> RelineUiMode {
    let cfg = match config::load_user_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[new-project] failed to load user config for Reline UI mode: {err}"
            ));
            return RelineUiMode::Simple;
        }
    };
    cfg.get_path(&["NewProjectWindow", "RelineUiMode"])
        .and_then(serde_json::Value::as_str)
        .map(RelineUiMode::from_config_str)
        .unwrap_or(RelineUiMode::Simple)
}

/// Persist the Reline UI mode to user config. Called only on toggle change, not per frame.
pub(super) fn save_reline_ui_mode(mode: RelineUiMode) {
    let result = (|| -> anyhow::Result<()> {
        let mut cfg = config::load_user_config()?;
        cfg.set_path(
            &["NewProjectWindow", "RelineUiMode"],
            serde_json::Value::String(mode.as_config_str().to_string()),
        )?;
        Ok(())
    })();
    if let Err(err) = result {
        ms_log::runtime_log::log_warn(format!(
            "[new-project] failed to save Reline UI mode: {err}"
        ));
    }
}

/// Load the persisted ribbon "equal width" preference from user config.
///
/// Returns `true` when the key is missing, holds a non-boolean, or the config cannot be read, so
/// the historical full-width ribbon stays the default.
pub(super) fn load_ribbon_uniform_width() -> bool {
    let cfg = match config::load_user_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[new-project] failed to load user config for ribbon uniform width: {err}"
            ));
            return true;
        }
    };
    cfg.get_path(&["NewProjectWindow", "RibbonUniformWidth"])
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// Persist the ribbon "equal width" preference. Called only on toggle change, not per frame.
pub(super) fn save_ribbon_uniform_width(uniform: bool) {
    let result = (|| -> anyhow::Result<()> {
        let mut cfg = config::load_user_config()?;
        cfg.set_path(
            &["NewProjectWindow", "RibbonUniformWidth"],
            serde_json::Value::Bool(uniform),
        )?;
        Ok(())
    })();
    if let Err(err) = result {
        ms_log::runtime_log::log_warn(format!(
            "[new-project] failed to save ribbon uniform width: {err}"
        ));
    }
}

/// Load the persisted ribbon width fraction from user config.
///
/// The stored value is clamped into `RIBBON_MIN_WIDTH_FRACTION..=1.0`; a missing, non-numeric, or
/// non-finite entry yields `1.0` (full width), which is the historical appearance.
pub(super) fn load_ribbon_width_fraction() -> f32 {
    let cfg = match config::load_user_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[new-project] failed to load user config for ribbon width fraction: {err}"
            ));
            return 1.0;
        }
    };
    cfg.get_path(&["NewProjectWindow", "RibbonWidthFraction"])
        .and_then(serde_json::Value::as_f64)
        // A UI width fraction only ever needs f32 precision, and `clamp_ribbon_width_fraction`
        // rejects the non-finite values an out-of-range `f64` can narrow into.
        .map(|value| clamp_ribbon_width_fraction(value as f32))
        .unwrap_or(1.0)
}

/// Persist the ribbon width fraction. Called when a resize drag ends, not per drag frame.
pub(super) fn save_ribbon_width_fraction(fraction: f32) {
    let clamped = clamp_ribbon_width_fraction(fraction);
    let result = (|| -> anyhow::Result<()> {
        let mut cfg = config::load_user_config()?;
        let number = serde_json::Number::from_f64(f64::from(clamped))
            .ok_or_else(|| anyhow::anyhow!("ribbon width fraction {clamped} is not representable"))?;
        cfg.set_path(
            &["NewProjectWindow", "RibbonWidthFraction"],
            serde_json::Value::Number(number),
        )?;
        Ok(())
    })();
    if let Err(err) = result {
        ms_log::runtime_log::log_warn(format!(
            "[new-project] failed to save ribbon width fraction: {err}"
        ));
    }
}
