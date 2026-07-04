/*
FILE OVERVIEW: src/config.rs
Global JSON config helpers and default payloads.

Main items:
- Path constants for project/user data files, model roots, and folders.
- `program_dir` / `data_dir`: launch working directory root for bundled helpers/assets and
  writable runtime data, with executable directory fallback.
- `default_projects_root` / `projects_root_from_user_settings`: resolve projects directory
  (default `{Documents}/manhwastudio_projects`, override from `user_config.json`).
- `JsonConfig`: load/merge/save wrapper for JSON configs with default backfilling.
- `user_config_defaults` / `project_config_defaults`: default trees for global and project settings.
- `AiInstallType`: installed AI dependency level recorded in `user_config.json`.
- `MemoryProfile`: persisted global image-cache memory policy recorded under `General`.
- `load_user_config`: canonical entry-point for `user_config.json` with persistence.
- `load_raw_user_settings_for_startup`: startup-safe read before default backfilling.
- `load_user_settings_for_startup`: startup-safe read of user settings without creating files.
*/

use crate::bubble_status::default_bubble_status_rules_value;
use crate::memory_manager::MemoryProfile;
use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[allow(dead_code)]
pub const VERSION: &str = "2.11.1";

#[allow(dead_code)]
pub const DEFAULT_PROJECT: &str = "";
#[allow(dead_code)]
pub const DEBUG_CONSOLE: bool = false;

pub const BUBBLES_FILE: &str = "translation_bubbles.json";
pub const NOTES_FILE: &str = "translation_notes.txt";
pub const SRC_DIR: &str = "src";
pub const CLEANED_DIR: &str = "cleaned";
pub const CLEAN_LAYERS_DIR: &str = "clean_layers";
pub const ALT_VERS_DIR: &str = "alt_vers";
pub const SAVED_DIR: &str = "saved";
pub const TEXT_IMAGES_DIR: &str = "text_images";
pub const LAYERS_DIR: &str = "layers";
pub const TEXT_DETECTION_DIR: &str = "text_detection";
pub const CHARACTERS_DIR: &str = "characters";
pub const TERMS_FILE: &str = "terms.json";
pub const PROJECT_SETTINGS_FILE: &str = "settings.json";
pub const USER_CONFIG_FILE: &str = "user_config.json";
pub const GENERAL_PROJECTS_DIR_KEY: &str = "projects_dir";
pub const GENERAL_AI_INSTALL_TYPE_KEY: &str = "ai_install_type";
pub const GENERAL_MEMORY_PROFILE_KEY: &str = "memory_profile";
pub const TEXT_TAB_HANGING_PUNCTUATION_KEY: &str = "hanging_punctuation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiInstallType {
    None,
    Base,
    Full,
}

impl AiInstallType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Base => "Base",
            Self::Full => "Full",
        }
    }

    #[must_use]
    pub fn from_user_settings(user_settings: &Value) -> Self {
        user_settings
            .get("General")
            .and_then(Value::as_object)
            .and_then(|general| general.get(GENERAL_AI_INSTALL_TYPE_KEY))
            .and_then(Value::as_str)
            .map(str::trim)
            .map(|value| match value {
                "Base" => Self::Base,
                "Full" => Self::Full,
                "None" => Self::None,
                _ => Self::None,
            })
            .unwrap_or(Self::None)
    }
}

fn dir_has_program_markers(dir: &Path) -> bool {
    dir.join("ai_backend.py").exists()
        || dir.join("installer_files").exists()
        || dir.join("modules").exists()
}

/// Resolve the app's runtime root.
fn resolve_runtime_root() -> PathBuf {
    let cwd = std::env::current_dir().ok();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));

    if let Some(cwd) = cwd.as_ref()
        && dir_has_program_markers(cwd)
    {
        return cwd.clone();
    }
    if let Some(exe_dir) = exe_dir.as_ref()
        && dir_has_program_markers(exe_dir)
    {
        return exe_dir.clone();
    }
    cwd.or(exe_dir).unwrap_or_else(|| PathBuf::from("."))
}

pub fn data_dir() -> PathBuf {
    resolve_runtime_root()
}

pub fn user_config_path() -> PathBuf {
    data_dir().join(USER_CONFIG_FILE)
}

/// Path to the dedicated SDXL inpainting settings file.
///
/// SDXL tool settings are kept in their own JSON file (not `user_config.json`)
/// so the tool's frequent background saves cannot race the canvas-settings saver
/// that owns `user_config.json`.
#[must_use]
pub fn sdxl_inpaint_settings_path() -> PathBuf {
    data_dir().join("sdxl_inpaint_settings.json")
}

/// Dedicated settings file for the FLUX.1-Fill inpaint tool (kept separate from
/// the `user_config.json` saver, like the SDXL one).
#[must_use]
pub fn flux_fill_inpaint_settings_path() -> PathBuf {
    data_dir().join("flux_fill_inpaint_settings.json")
}

pub fn program_dir() -> PathBuf {
    resolve_runtime_root()
}

#[allow(dead_code)]
pub fn projects_root() -> PathBuf {
    default_projects_root()
}

pub fn default_projects_root() -> PathBuf {
    let base_dir = default_documents_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    base_dir.join("manhwastudio_projects")
}

pub fn projects_root_from_user_settings(user_settings: &Value) -> PathBuf {
    user_settings
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_PROJECTS_DIR_KEY))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_projects_root)
}

#[must_use]
pub fn memory_profile_from_user_settings(user_settings: &Value) -> MemoryProfile {
    user_settings
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_MEMORY_PROFILE_KEY))
        .and_then(Value::as_str)
        .and_then(MemoryProfile::from_config_str)
        .unwrap_or_default()
}

fn default_documents_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(profile).join("Documents"));
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return Some(PathBuf::from(home).join("Documents"));
        }
    }

    None
}

pub fn models_dir() -> PathBuf {
    data_dir().join("ManhwaStudio_AI_Models")
}

pub fn torch_models_dir() -> PathBuf {
    models_dir().join("Torch")
}

pub fn onnx_models_dir() -> PathBuf {
    models_dir().join("ONNX")
}

pub fn lama_dir() -> PathBuf {
    torch_models_dir().join("LaMa")
}

pub fn lama_models_dir() -> PathBuf {
    lama_dir().join("models")
}

pub fn lama_mpe_dir() -> PathBuf {
    torch_models_dir().join("LaMa_MPE")
}

pub fn aot_dir() -> PathBuf {
    torch_models_dir().join("AOT")
}

pub fn torch_text_detector_dir() -> PathBuf {
    torch_models_dir().join("ComicTextDetector")
}

pub fn onnx_text_detector_dir() -> PathBuf {
    onnx_models_dir().join("ComicTextDetector")
}

pub fn paddle_onnx_dir() -> PathBuf {
    onnx_models_dir().join("PaddleOCR")
}

pub fn manga_ocr_onnx_dir() -> PathBuf {
    onnx_models_dir().join("MangaOCR")
}

/// Сторонние крупные модели, скачиваемые по требованию (не из основного репо).
pub fn side_models_dir() -> PathBuf {
    models_dir().join("side_models")
}

/// FLUX.1-Fill-dev: GGUF-трансформер (квант на выбор) лежит здесь, diffusers-компоненты
/// (VAE/CLIP/T5/scheduler) — в подпапке `components/`.
pub fn flux_fill_dir() -> PathBuf {
    side_models_dir().join("FLUX.1-Fill-dev-GGUF")
}

pub fn flux_fill_components_dir() -> PathBuf {
    flux_fill_dir().join("components")
}

pub fn model_folders() -> Vec<PathBuf> {
    vec![
        models_dir(),
        torch_models_dir(),
        onnx_models_dir(),
        lama_dir(),
        lama_models_dir(),
        lama_mpe_dir(),
        aot_dir(),
        torch_text_detector_dir(),
        onnx_text_detector_dir(),
        paddle_onnx_dir(),
        manga_ocr_onnx_dir(),
        side_models_dir(),
        flux_fill_dir(),
        flux_fill_components_dir(),
    ]
}

pub fn ensure_model_dirs() -> Result<()> {
    for folder in model_folders() {
        fs::create_dir_all(&folder)
            .with_context(|| format!("failed to create model dir {}", folder.display()))?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct JsonConfig {
    pub path: PathBuf,
    defaults: Value,
    pub data: Value,
}

#[allow(dead_code)]
impl JsonConfig {
    pub fn new(path: impl Into<PathBuf>, defaults: Value) -> Result<Self> {
        let mut cfg = Self {
            path: path.into(),
            defaults,
            data: Value::Object(Map::new()),
        };
        cfg.load()?;
        cfg.apply_defaults();
        cfg.save()?;
        Ok(cfg)
    }

    pub fn load(&mut self) -> Result<()> {
        // Routed through the storage seam so the web build reads config from its
        // in-memory/IndexedDB store instead of the desktop filesystem.
        let store = crate::storage::storage();
        let path_str = self.path.to_string_lossy();
        if !store.exists(path_str.as_ref()) {
            self.data = Value::Object(Map::new());
            return Ok(());
        }
        let raw = store
            .read_to_string(path_str.as_ref())
            .with_context(|| format!("failed to read config {}", self.path.display()))?;
        self.data = serde_json::from_str::<Value>(&raw)
            .with_context(|| format!("failed to parse config {}", self.path.display()))?;
        if !self.data.is_object() {
            self.data = Value::Object(Map::new());
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let store = crate::storage::storage();
        if let Some(parent) = self.path.parent() {
            let parent_str = parent.to_string_lossy();
            store.create_dir_all(parent_str.as_ref()).with_context(|| {
                format!(
                    "failed to create config parent directory {}",
                    parent.display()
                )
            })?;
        }
        let raw = serde_json::to_string_pretty(&self.data).context("failed to serialize config")?;
        store
            .write(self.path.to_string_lossy().as_ref(), raw.as_bytes())
            .with_context(|| format!("failed to write config {}", self.path.display()))?;
        Ok(())
    }

    pub fn apply_defaults(&mut self) {
        merge_missing(&mut self.data, &self.defaults);
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }

    pub fn get_path<'a>(&'a self, path: &[&str]) -> Option<&'a Value> {
        let mut cur = &self.data;
        for part in path {
            cur = cur.get(*part)?;
        }
        Some(cur)
    }

    pub fn set(&mut self, key: &str, value: Value) -> Result<()> {
        let Some(obj) = self.data.as_object_mut() else {
            self.data = Value::Object(Map::new());
            return self.set(key, value);
        };
        obj.insert(key.to_owned(), value);
        self.save()
    }

    pub fn set_path(&mut self, path: &[&str], value: Value) -> Result<()> {
        if path.is_empty() {
            self.data = value;
            return self.save();
        }
        if !self.data.is_object() {
            self.data = Value::Object(Map::new());
        }
        let mut cur = self.data.as_object_mut().expect("object ensured");
        for part in &path[..path.len() - 1] {
            let entry = cur
                .entry((*part).to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            cur = entry.as_object_mut().expect("object ensured");
        }
        cur.insert(path[path.len() - 1].to_owned(), value);
        self.save()
    }
}

fn merge_missing(dst: &mut Value, defaults: &Value) {
    if let (Value::Object(dst_obj), Value::Object(def_obj)) = (dst, defaults) {
        for (k, v) in def_obj {
            match dst_obj.get_mut(k) {
                Some(existing) => merge_missing(existing, v),
                None => {
                    dst_obj.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

pub fn user_config_defaults() -> Value {
    let default_projects_root = default_projects_root();
    json!({
        "General": {
            "theme": "dark",
            "style": "default",
            "projects_dir": default_projects_root.to_string_lossy().to_string(),
            "ai_backend_autostart": true,
            "ai_device": "not-selected",
            "ai_onnx_provider": "not-selected",
            "ai_onnx_device_id": "not-selected",
            "ai_max_loaded_models": 3,
            "ai_install_type": AiInstallType::None.as_str(),
            "memory_profile": MemoryProfile::default().as_config_str(),
            "typing_panel_layout": "vertical",
            "enabled_tabs": {
                "Перевод": true,
                "Клининг": true,
                "Текст": true,
                "Персонажи": true,
                "Термины": true,
                "Заметки перевода": true,
                "Вики": true
            }
        },
        "Canvas": {
            "scale_bubbles": true,
            "aside_min_width_px": 450,
            "aside_max_width_px": 550,
            "aside_compact_mode": "none",
            "aside_side_mode": "auto",
            "aside_second_column": false,
            "bubble_status_rules": default_bubble_status_rules_value(),
            "spellcheck_original": false,
            "spellcheck_translation": true,
            "cache_pages": true,
            "translation_status_display": "until_next",
            "opengl_enabled": false,
            "opengl_device": "auto"
        },
        "NewProjectWindow": {
            "ImageUrlPrefs": {
                "mto.to": "https://*.mb*.org/media/",
                "Kakao page-edge": "https://page-edge.kakao.com/sdownload/resource*",
                "Naver CDN (generic)": "https://image-comic.pstatic.net/webtoon/*",
                "funbe": "https://funbe*.com/data/file/wtoon/*",
                "rumanhua.com": "https://p*-zhuxiaobang-sign.shimolife.com/*",
                "webtoons.com": "https://webtoon-phinf.pstatic.net/*"
            }
        },
        "Hotkeys": {},
        "TranslarionTab": {
            "TextDetector": {
                "draw_lines": true,
                "draw_mask": true,
                "block_expand_px": 0,
                "merge_close": false,
                "merge_gap_px": 5,
                "params": {
                    "device": "cpu",
                    "detect_size": 1280,
                    "det_rearrange_max_batches": 4,
                    "font size multiplier": 1.0,
                    "font size max": -1.0,
                    "font size min": -1.0,
                    "mask dilate size": 2
                }
            },
            "MachineTranslation": {
                "service": "google",
                "source_lang": "auto",
                "target_lang": "ru"
            }
        },
        "CleaningTab": {},
        "TextTab": {
            "use_system_fonts": false,
            "hanging_punctuation": crate::text_punctuation::DEFAULT_HANGING_PUNCTUATION,
            "formula_presets": {
                "Дуга (мягкая)": {
                    "x_expr": "t * w",
                    "y_expr": "120 * sin((t - 0.5) * pi)",
                    "rotation_expr": "0",
                    "use_tangent_rotation": true,
                    "t_start": 0.0,
                    "t_end": 1.0,
                    "offset_x_px": 0.0,
                    "offset_y_px": 0.0,
                    "scale_x": 1.0,
                    "scale_y": 1.0,
                    "normal_offset_px": 0.0,
                    "letter_spacing_mul": 1.25,
                    "vars": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
                },
                "Наклонная линия": {
                    "x_expr": "t * w",
                    "y_expr": "0.35 * t * w",
                    "rotation_expr": "0",
                    "use_tangent_rotation": false,
                    "t_start": 0.0,
                    "t_end": 1.0,
                    "offset_x_px": 0.0,
                    "offset_y_px": 0.0,
                    "scale_x": 1.0,
                    "scale_y": 1.0,
                    "normal_offset_px": 0.0,
                    "letter_spacing_mul": 1.1,
                    "vars": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
                },
                "Волна": {
                    "x_expr": "t * w",
                    "y_expr": "80 * sin(2 * pi * t)",
                    "rotation_expr": "0.15 * sin(2 * pi * t)",
                    "use_tangent_rotation": false,
                    "t_start": 0.0,
                    "t_end": 1.0,
                    "offset_x_px": 0.0,
                    "offset_y_px": 0.0,
                    "scale_x": 1.0,
                    "scale_y": 1.0,
                    "normal_offset_px": 0.0,
                    "letter_spacing_mul": 1.2,
                    "vars": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
                },
                "Спираль": {
                    "x_expr": "(a + b * t) * cos(c * tau * t)",
                    "y_expr": "(a + b * t) * sin(c * tau * t)",
                    "rotation_expr": "0",
                    "use_tangent_rotation": true,
                    "t_start": 0.0,
                    "t_end": 1.0,
                    "offset_x_px": 0.0,
                    "offset_y_px": 0.0,
                    "scale_x": 1.0,
                    "scale_y": 1.0,
                    "normal_offset_px": 0.0,
                    "letter_spacing_mul": 1.35,
                    "vars": [40.0, 180.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0]
                },
                "Экспонента": {
                    "x_expr": "t * w",
                    "y_expr": "140 * (exp(a * t) - 1) / (exp(a) - 1)",
                    "rotation_expr": "0",
                    "use_tangent_rotation": true,
                    "t_start": 0.0,
                    "t_end": 1.0,
                    "offset_x_px": 0.0,
                    "offset_y_px": 0.0,
                    "scale_x": 1.0,
                    "scale_y": 1.0,
                    "normal_offset_px": 0.0,
                    "letter_spacing_mul": 1.2,
                    "vars": [3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
                }
            }
        }
    })
}

pub fn project_config_defaults() -> Value {
    json!({
        "bubble_type": "hybrid",
        "editable_bubble_type": "aside",
        "readonly_bubble_type": "aside",
        "on_top_focus_mode": "around",
        "page_spacing_px": 200,
        "opengl_enabled": false,
        "opengl_device": "auto",
        "canvas": {
            "bubble_type": "hybrid",
            "editable_bubble_type": "aside",
            "readonly_bubble_type": "aside",
            "on_top_focus_mode": "around",
            "show_bubbles": true,
            "show_bubble_status": false,
            "bubble_opacity": 1.0,
            "page_spacing_px": 200,
            "separate_pages": true,
            "vertical_edge_margin_px": 200,
            "side_margin_px": 20,
            "aside_compact_mode": "none",
            "aside_side_mode": "auto",
            "aside_second_column": false,
            "aside_scale_pct": 100,
            "tabs_autosync_enabled": true,
            "auto_insert_last_character": true,
            "project_custom_spellcheck_words": "",
            "cache_pages": true,
            "translation_status_display": "until_next",
            "opengl_enabled": false,
            "opengl_device": "auto"
        },
        "OCR": {
            "engine": "paddle",
            "params": {
                "easyocr": {"langs": "ko", "gpu": false},
                "paddle": {"langs": "korean", "gpu": false},
                "none": {}
            },
            "join": true,
            "reflect": false,
            "copy": false,
            "bubbles": true
        },
        "composition": {
            "method": "height",
            "source_mode": "original",
            "ignore_translated_lines": true,
            "merge_same_character": true,
            "sep_same_character": "\\n",
            "sep_between": "\\n\\n",
            "replica_prefix": "",
            "nl_replace": " ",
            "nl_replace_enabled": true,
            "wrap_with": "``",
            "wrap_with_enabled": true,
            "limit": 700,
            "limit_enabled": true,
            "use_character_names": true,
            "jinja2_enabled": false,
            "jinja2_template": ""
        },
        "machine_translation": {
            "service": "google",
            "source_lang": "auto",
            "target_lang": "ru"
        }
    })
}

pub fn load_user_config() -> Result<JsonConfig> {
    let mut cfg = JsonConfig {
        path: user_config_path(),
        defaults: user_config_defaults(),
        data: Value::Object(Map::new()),
    };
    cfg.load()?;
    migrate_missing_memory_profile_from_legacy_cache_pages(&mut cfg.data);
    cfg.apply_defaults();
    cfg.save()?;
    Ok(cfg)
}

pub fn load_raw_user_settings_for_startup() -> Result<Value> {
    let user_config_path = user_config_path();
    let data = match fs::read_to_string(&user_config_path) {
        Ok(raw) => serde_json::from_str::<Value>(&raw)
            .with_context(|| format!("failed to parse config {}", user_config_path.display()))?,
        Err(err) if err.kind() == ErrorKind::NotFound => Value::Object(Map::new()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read config {}", user_config_path.display()));
        }
    };
    Ok(data)
}

#[must_use]
pub fn user_settings_has_ai_install_type(user_settings: &Value) -> bool {
    user_settings
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_AI_INSTALL_TYPE_KEY))
        .is_some()
}

pub fn load_user_settings_for_startup() -> Result<Value> {
    let mut data = load_raw_user_settings_for_startup()?;
    if !data.is_object() {
        data = Value::Object(Map::new());
    }
    migrate_missing_memory_profile_from_legacy_cache_pages(&mut data);
    let defaults = user_config_defaults();
    merge_missing(&mut data, &defaults);
    Ok(data)
}

fn migrate_missing_memory_profile_from_legacy_cache_pages(data: &mut Value) {
    if !data.is_object() {
        *data = Value::Object(Map::new());
    }
    let Some(root_obj) = data.as_object_mut() else {
        return;
    };
    let mut general_obj = root_obj
        .get("General")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if general_obj.contains_key(GENERAL_MEMORY_PROFILE_KEY) {
        root_obj.insert("General".to_string(), Value::Object(general_obj));
        return;
    }

    let profile = root_obj
        .get("Canvas")
        .and_then(Value::as_object)
        .and_then(|canvas| canvas.get("cache_pages"))
        .and_then(Value::as_bool)
        .map(|enabled| {
            if enabled {
                MemoryProfile::Medium
            } else {
                MemoryProfile::Low
            }
        })
        .unwrap_or_default();
    general_obj.insert(
        GENERAL_MEMORY_PROFILE_KEY.to_string(),
        Value::String(profile.as_config_str().to_string()),
    );
    root_obj.insert("General".to_string(), Value::Object(general_obj));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_install_type_parses_user_settings_values() {
        assert_eq!(
            AiInstallType::from_user_settings(&json!({"General": {"ai_install_type": "None"}})),
            AiInstallType::None
        );
        assert_eq!(
            AiInstallType::from_user_settings(&json!({"General": {"ai_install_type": "Base"}})),
            AiInstallType::Base
        );
        assert_eq!(
            AiInstallType::from_user_settings(&json!({"General": {"ai_install_type": "Full"}})),
            AiInstallType::Full
        );
        assert_eq!(
            AiInstallType::from_user_settings(&json!({"General": {"ai_install_type": "bad"}})),
            AiInstallType::None
        );
    }

    #[test]
    fn user_settings_has_ai_install_type_detects_missing_key() {
        assert!(!user_settings_has_ai_install_type(&json!({})));
        assert!(!user_settings_has_ai_install_type(&json!({"General": {}})));
        assert!(user_settings_has_ai_install_type(
            &json!({"General": {"ai_install_type": "Base"}})
        ));
    }

    #[test]
    fn missing_memory_profile_migrates_from_user_cache_pages_only() {
        let mut disabled = json!({"Canvas": {"cache_pages": false}});
        migrate_missing_memory_profile_from_legacy_cache_pages(&mut disabled);
        assert_eq!(
            memory_profile_from_user_settings(&disabled),
            MemoryProfile::Low
        );

        let mut enabled = json!({"Canvas": {"cache_pages": true}});
        migrate_missing_memory_profile_from_legacy_cache_pages(&mut enabled);
        assert_eq!(
            memory_profile_from_user_settings(&enabled),
            MemoryProfile::Medium
        );

        let mut existing = json!({
            "General": {"memory_profile": "maximum"},
            "Canvas": {"cache_pages": false}
        });
        migrate_missing_memory_profile_from_legacy_cache_pages(&mut existing);
        assert_eq!(
            memory_profile_from_user_settings(&existing),
            MemoryProfile::Maximum
        );
    }
}
