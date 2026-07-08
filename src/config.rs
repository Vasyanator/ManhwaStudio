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
- `AiRuntime`: selected AI runtime (`backend`/`native`) recorded under `General.ai_runtime`.
- `OrtLoadGuard` / `OrtLoadDecision` / `ort_load_decision` / `ort_load_scope_key` /
  `read_ort_load_guard`: per-scope ONNX Runtime SIGILL load-guard model and its pure
  decision logic (state persisted under `General.ort_load_state`).
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
/// `General` key selecting which AI runtime the app drives: the external Python
/// backend process (`"backend"`) or the in-process native ONNX Runtime path
/// (`"native"`). Parsed by [`AiRuntime::from_user_settings`].
// Phase 0 config plumbing; the runtime selector reads it in Phase 1 (not yet wired).
#[allow(dead_code)]
pub const GENERAL_AI_RUNTIME_KEY: &str = "ai_runtime";
/// `General` key holding the ORT execution-provider TOKEN shared by the Python
/// backend AND the native in-process ONNX path (e.g. `"CPUExecutionProvider"` /
/// `"DmlExecutionProvider"` / `"CUDAExecutionProvider"` / `"CoreMLExecutionProvider"`).
/// Read by [`ai_onnx_provider_token_from_user_settings`]; the native path maps the
/// token to `ms_onnx::ExecutionProvider` in `native_runtime`.
pub const GENERAL_AI_ONNX_PROVIDER_KEY: &str = "ai_onnx_provider";
/// `General` key holding the ONNX accelerator adapter index (a string like `"0"`),
/// shared by the backend and the native path. Read by
/// [`ai_onnx_device_id_from_user_settings`].
pub const GENERAL_AI_ONNX_DEVICE_ID_KEY: &str = "ai_onnx_device_id";
/// `General` boolean marking the ONNX provider as an explicit user choice, matching
/// the Python backend's `ai_onnx_provider_configured` flag so an offline selection is
/// honored once the backend starts.
pub const GENERAL_AI_ONNX_PROVIDER_CONFIGURED_KEY: &str = "ai_onnx_provider_configured";
/// `General` boolean marking the ONNX device id as an explicit user choice, matching
/// the Python backend's `ai_onnx_device_id_configured` flag.
pub const GENERAL_AI_ONNX_DEVICE_ID_CONFIGURED_KEY: &str = "ai_onnx_device_id_configured";
/// `General` key holding the maximum number of simultaneously resident AI models
/// (used by BOTH the backend LRU and the native engine LRU). Read by
/// [`ai_max_loaded_models_from_user_settings`].
pub const GENERAL_AI_MAX_LOADED_MODELS_KEY: &str = "ai_max_loaded_models";
/// `General` key holding the per-scope ONNX Runtime SIGILL load-guard map (a JSON
/// object; entries keyed by [`ort_load_scope_key`]). Read by [`read_ort_load_guard`].
// Phase 0 config plumbing; the ORT load path reads/writes it in Phase 1 (not yet wired).
#[allow(dead_code)]
pub const GENERAL_ORT_LOAD_STATE_KEY: &str = "ort_load_state";
pub const GENERAL_MEMORY_PROFILE_KEY: &str = "memory_profile";
pub const TEXT_TAB_HANGING_PUNCTUATION_KEY: &str = "hanging_punctuation";
pub const TEXT_TAB_ROTATION_CTRL_WHEEL_MODE_KEY: &str = "rotation_ctrl_wheel_mode";
/// `TextTab` key holding the user-imported system font FILE paths (a JSON array of
/// strings). Seeds the typing tab's imported-fonts store at startup.
pub const TEXT_TAB_IMPORTED_SYSTEM_FONTS_KEY: &str = "imported_system_fonts";

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

/// Which AI runtime the application drives, persisted under
/// `General.ai_runtime` in `user_config.json`.
///
/// `Backend` routes inference through the external Python `ai_backend.py`
/// process (the historical default). `Native` uses the in-process ONNX Runtime
/// path (`crate`-native, loaded lazily). Unknown or missing values resolve to
/// `Backend` so an unrecognized config never silently enables native loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRuntime {
    Backend,
    // The native ONNX path is selectable only once Phase 1 wires the runtime
    // switch; the variant exists now for the persisted-config contract.
    #[allow(dead_code)]
    Native,
}

impl AiRuntime {
    /// Config token stored under `General.ai_runtime`.
    ///
    /// Stable string contract: `Backend` -> `"backend"`, `Native` -> `"native"`.
    #[must_use]
    pub fn as_key(self) -> &'static str {
        match self {
            Self::Backend => "backend",
            Self::Native => "native",
        }
    }

    /// Reads `General.ai_runtime` from a raw user-settings tree.
    ///
    /// Returns `Backend` when the key is absent, non-string, or holds an
    /// unrecognized token, so an invalid config never enables the native path.
    // Consumed by the Phase 1 runtime selector; unused in non-test code until then.
    #[allow(dead_code)]
    #[must_use]
    pub fn from_user_settings(cfg: &Value) -> Self {
        cfg.get("General")
            .and_then(Value::as_object)
            .and_then(|general| general.get(GENERAL_AI_RUNTIME_KEY))
            .and_then(Value::as_str)
            .map(str::trim)
            .map(|value| match value {
                "native" => Self::Native,
                // Any other token (including "backend" and unknown values)
                // falls back to the safe default.
                _ => Self::Backend,
            })
            .unwrap_or(Self::Backend)
    }
}

/// Reads the shared ONNX execution-provider TOKEN from `General.ai_onnx_provider`.
///
/// Returns the trimmed ORT token (e.g. `"DmlExecutionProvider"`) or `None` when the
/// key is absent, empty, non-string, or the `"not-selected"` sentinel. The token is
/// mapped to `ms_onnx::ExecutionProvider` by `native_runtime`; the backend uses the
/// same key, so one selection drives both runtimes.
#[must_use]
pub fn ai_onnx_provider_token_from_user_settings(cfg: &Value) -> Option<String> {
    cfg.get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_AI_ONNX_PROVIDER_KEY))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("not-selected"))
        .map(str::to_string)
}

/// Reads the ONNX accelerator adapter index from `General.ai_onnx_device_id`.
///
/// Accepts either a JSON string (the backend stores `str(device_id)`) or a JSON
/// number and returns the trimmed value as a string, or `None` when absent, empty,
/// or the `"not-selected"` sentinel. The native path parses it to an `i32` adapter
/// index; the UI keeps it as an option id.
#[must_use]
pub fn ai_onnx_device_id_from_user_settings(cfg: &Value) -> Option<String> {
    cfg.get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_AI_ONNX_DEVICE_ID_KEY))
        .and_then(|value| match value {
            Value::String(text) => Some(text.trim().to_string()),
            Value::Number(number) => Some(number.to_string()),
            Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
        })
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("not-selected"))
}

/// Reads `General.ai_max_loaded_models` as a UI-clamped model limit (1..=10).
///
/// Accepts a JSON number or numeric string (the backend stores it as a string) and
/// clamps to `1..=10`; anything absent, non-numeric (e.g. `"not-selected"`), or out
/// of range resolves to `3`, matching the config default and the backend/native
/// LRU defaults.
#[must_use]
pub fn ai_max_loaded_models_from_user_settings(cfg: &Value) -> u32 {
    cfg.get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_AI_MAX_LOADED_MODELS_KEY))
        .and_then(|value| match value {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.trim().parse::<u64>().ok(),
            Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
        })
        .and_then(|value| u32::try_from(value).ok())
        .map(|value| value.clamp(1, 10))
        .unwrap_or(3)
}

/// Persisted per-scope state of an ONNX Runtime dynamic-library load attempt,
/// used to survive an uncatchable SIGILL on CPUs lacking required instructions.
///
/// The pair is written to disk BEFORE the load (`attempted = true`,
/// `succeeded = false`) and flipped to `succeeded = true` only after the load
/// returns normally. A crash between those two writes leaves `attempted &&
/// !succeeded` on disk, which the next launch reads to avoid re-triggering the
/// fault. Missing entries default to both fields `false`.
// Phase 0 model for the SIGILL guard; constructed by the Phase 1 ORT load path.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrtLoadGuard {
    /// A load was started for this scope and the flag was flushed before the load.
    pub attempted: bool,
    /// The load returned normally after `attempted` was set.
    pub succeeded: bool,
}

/// Decision derived from an [`OrtLoadGuard`] about whether it is safe to touch
/// the ONNX Runtime library for the corresponding scope on this launch.
// Phase 0 decision type; the Phase 1 ORT load path branches on it.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrtLoadDecision {
    /// No aborted attempt recorded: loading ORT for this scope is allowed.
    Safe,
    /// A previous attempt started but never confirmed success (likely crashed
    /// the process): do NOT touch ORT for this scope.
    Suspect,
}

/// Pure decision for whether ONNX Runtime is safe to load for a given scope.
///
/// Returns [`OrtLoadDecision::Suspect`] iff `attempted && !succeeded` (a prior
/// load began but never confirmed success, so it most likely aborted the
/// process via SIGILL); otherwise [`OrtLoadDecision::Safe`].
// Pure logic invoked by the Phase 1 ORT load path; unused in non-test code until then.
#[allow(dead_code)]
#[must_use]
pub fn ort_load_decision(guard: OrtLoadGuard) -> OrtLoadDecision {
    match (guard.attempted, guard.succeeded) {
        (true, false) => OrtLoadDecision::Suspect,
        (false, false) | (false, true) | (true, true) => OrtLoadDecision::Safe,
    }
}

/// Builds the load-guard scope key for a provider + adapter index + onnxruntime
/// version.
///
/// Format is `"{provider_id}@{ort_version}"` when `device_id` is `None` (e.g.
/// `"cpu@1.20.1"`) and `"{provider_id}:{device_id}@{ort_version}"` when a specific
/// accelerator adapter is targeted (e.g. `"directml:1@1.20.1"`). Scoping by provider
/// AND device prevents a failed accelerator attempt (a specific CUDA/DirectML
/// adapter) from blocking a working CPU path or a different, healthy adapter;
/// scoping by onnxruntime version auto-resets the guard when the library is
/// upgraded. `provider_id` is the `ms_onnx::ExecutionProvider::id` string, accepted
/// as `&str` to keep `config` free of an `ms-onnx` dependency.
#[must_use]
pub fn ort_load_scope_key(provider_id: &str, device_id: Option<i32>, ort_version: &str) -> String {
    match device_id {
        Some(device_id) => format!("{provider_id}:{device_id}@{ort_version}"),
        None => format!("{provider_id}@{ort_version}"),
    }
}

/// Reads the [`OrtLoadGuard`] for `scope_key` from `General.ort_load_state`.
///
/// A missing map, missing entry, or non-boolean fields default to `false`, so an
/// absent or malformed entry reads as "no attempt recorded" ([`OrtLoadGuard`]
/// with both fields `false`).
// Read by the Phase 1 launch-time guard check; unused in non-test code until then.
#[allow(dead_code)]
#[must_use]
pub fn read_ort_load_guard(cfg: &Value, scope_key: &str) -> OrtLoadGuard {
    let entry = cfg
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_ORT_LOAD_STATE_KEY))
        .and_then(Value::as_object)
        .and_then(|state| state.get(scope_key))
        .and_then(Value::as_object);
    let Some(entry) = entry else {
        return OrtLoadGuard {
            attempted: false,
            succeeded: false,
        };
    };
    let read_bool = |field: &str| {
        entry
            .get(field)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    OrtLoadGuard {
        attempted: read_bool("attempted"),
        succeeded: read_bool("succeeded"),
    }
}

fn dir_has_program_markers(dir: &Path) -> bool {
    dir.join("ai_backend.py").exists()
        || dir.join("installer_files").exists()
        || dir.join("modules").exists()
}

/// macOS-only: resolve the writable runtime root when the executable runs from
/// inside a `*.app` bundle.
///
/// Under Gatekeeper quarantine the `.app` bundle (including `Contents/Resources`)
/// is read-only, so the portable "data next to the binary" layout used on
/// Linux/Windows cannot write config/models/logs there. This redirects the root
/// to `~/Library/Application Support/ManhwaStudio` (created if missing).
///
/// Returns `None` when the executable is NOT inside an `.app` bundle (a plain
/// unpacked folder, like the Linux distribution) or when `HOME`/the exe path
/// cannot be resolved, so the caller keeps the unchanged portable behavior.
#[cfg(target_os = "macos")]
fn macos_app_bundle_data_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if !is_inside_macos_app_bundle(&exe) {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    let root = PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("ManhwaStudio");
    // Create the writable root eagerly. A creation failure is logged but the path
    // is still returned so a later write surfaces a precise, actionable error
    // instead of silently falling back to the read-only bundle directory.
    if let Err(err) = fs::create_dir_all(&root) {
        eprintln!(
            "ManhwaStudio: failed to create macOS data root {}: {err}",
            root.display()
        );
    }
    Some(root)
}

/// Pure structural check: is `exe_path` located directly inside a macOS
/// application bundle, i.e. `<name>.app/Contents/MacOS/<exe>`?
///
/// Only the directory names are inspected; the path need not exist on disk. This
/// is the sole signal used to decide whether the bundle-safe data root applies.
#[cfg(target_os = "macos")]
fn is_inside_macos_app_bundle(exe_path: &Path) -> bool {
    // parent must be `MacOS`, grandparent `Contents`, great-grandparent `*.app`.
    let Some(macos_dir) = exe_path.parent() else {
        return false;
    };
    if macos_dir.file_name().and_then(|n| n.to_str()) != Some("MacOS") {
        return false;
    }
    let Some(contents_dir) = macos_dir.parent() else {
        return false;
    };
    if contents_dir.file_name().and_then(|n| n.to_str()) != Some("Contents") {
        return false;
    }
    contents_dir
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.ends_with(".app"))
}

/// Resolve the app's runtime root.
fn resolve_runtime_root() -> PathBuf {
    // macOS: inside a signed/quarantined `.app` the bundle is read-only, so the
    // writable runtime root moves to Application Support. Outside a bundle (a
    // plain unpacked folder) this returns None and the portable logic below runs
    // unchanged, keeping Linux/Windows behavior byte-identical.
    #[cfg(target_os = "macos")]
    {
        if let Some(bundle_root) = macos_app_bundle_data_root() {
            return bundle_root;
        }
    }

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
            "ai_runtime": AiRuntime::Backend.as_key(),
            "ort_load_state": {},
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
        "Tutorials": {
            "completed": [],
            "autoplay": true
        },
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
            "hanging_punctuation": crate::text_punctuation::DEFAULT_HANGING_PUNCTUATION,
            "rotation_ctrl_wheel_mode":
                crate::tabs::typing::rotation_ctrl_wheel::DEFAULT_ROTATION_CTRL_WHEEL_MODE
                    .as_config_str(),
            "effect_defaults": {},
            "imported_system_fonts": [],
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
    fn ai_runtime_parses_user_settings_values() {
        // Missing key -> safe default.
        assert_eq!(AiRuntime::from_user_settings(&json!({})), AiRuntime::Backend);
        assert_eq!(
            AiRuntime::from_user_settings(&json!({"General": {}})),
            AiRuntime::Backend
        );
        // Explicit values.
        assert_eq!(
            AiRuntime::from_user_settings(&json!({"General": {"ai_runtime": "backend"}})),
            AiRuntime::Backend
        );
        assert_eq!(
            AiRuntime::from_user_settings(&json!({"General": {"ai_runtime": "native"}})),
            AiRuntime::Native
        );
        // Whitespace is trimmed.
        assert_eq!(
            AiRuntime::from_user_settings(&json!({"General": {"ai_runtime": " native "}})),
            AiRuntime::Native
        );
        // Unknown token -> safe default.
        assert_eq!(
            AiRuntime::from_user_settings(&json!({"General": {"ai_runtime": "onnx"}})),
            AiRuntime::Backend
        );
    }

    #[test]
    fn ai_runtime_as_key_round_trips() {
        assert_eq!(AiRuntime::Backend.as_key(), "backend");
        assert_eq!(AiRuntime::Native.as_key(), "native");
        for runtime in [AiRuntime::Backend, AiRuntime::Native] {
            assert_eq!(
                AiRuntime::from_user_settings(
                    &json!({"General": {"ai_runtime": runtime.as_key()}})
                ),
                runtime
            );
        }
    }

    #[test]
    fn ai_onnx_provider_token_reads_and_filters() {
        // Absent / empty / not-selected -> None.
        assert_eq!(ai_onnx_provider_token_from_user_settings(&json!({})), None);
        assert_eq!(
            ai_onnx_provider_token_from_user_settings(&json!({"General": {}})),
            None
        );
        assert_eq!(
            ai_onnx_provider_token_from_user_settings(
                &json!({"General": {"ai_onnx_provider": "not-selected"}})
            ),
            None
        );
        // A real token is trimmed and returned verbatim.
        assert_eq!(
            ai_onnx_provider_token_from_user_settings(
                &json!({"General": {"ai_onnx_provider": " DmlExecutionProvider "}})
            )
            .as_deref(),
            Some("DmlExecutionProvider")
        );
    }

    #[test]
    fn ai_onnx_device_id_reads_string_or_number() {
        assert_eq!(ai_onnx_device_id_from_user_settings(&json!({})), None);
        assert_eq!(
            ai_onnx_device_id_from_user_settings(
                &json!({"General": {"ai_onnx_device_id": "not-selected"}})
            ),
            None
        );
        assert_eq!(
            ai_onnx_device_id_from_user_settings(&json!({"General": {"ai_onnx_device_id": " 1 "}}))
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            ai_onnx_device_id_from_user_settings(&json!({"General": {"ai_onnx_device_id": 2}}))
                .as_deref(),
            Some("2")
        );
    }

    #[test]
    fn ai_max_loaded_models_clamps_and_defaults() {
        assert_eq!(ai_max_loaded_models_from_user_settings(&json!({})), 3);
        assert_eq!(
            ai_max_loaded_models_from_user_settings(
                &json!({"General": {"ai_max_loaded_models": "not-selected"}})
            ),
            3
        );
        assert_eq!(
            ai_max_loaded_models_from_user_settings(
                &json!({"General": {"ai_max_loaded_models": 0}})
            ),
            1
        );
        assert_eq!(
            ai_max_loaded_models_from_user_settings(
                &json!({"General": {"ai_max_loaded_models": 99}})
            ),
            10
        );
        assert_eq!(
            ai_max_loaded_models_from_user_settings(
                &json!({"General": {"ai_max_loaded_models": "5"}})
            ),
            5
        );
    }

    #[test]
    fn ort_load_decision_truth_table() {
        // Suspect only when a load was attempted but never confirmed.
        assert_eq!(
            ort_load_decision(OrtLoadGuard {
                attempted: false,
                succeeded: false
            }),
            OrtLoadDecision::Safe
        );
        assert_eq!(
            ort_load_decision(OrtLoadGuard {
                attempted: false,
                succeeded: true
            }),
            OrtLoadDecision::Safe
        );
        assert_eq!(
            ort_load_decision(OrtLoadGuard {
                attempted: true,
                succeeded: true
            }),
            OrtLoadDecision::Safe
        );
        assert_eq!(
            ort_load_decision(OrtLoadGuard {
                attempted: true,
                succeeded: false
            }),
            OrtLoadDecision::Suspect
        );
    }

    #[test]
    fn ort_load_scope_key_formats_provider_device_and_version() {
        // No adapter index -> provider-only scope.
        assert_eq!(ort_load_scope_key("cpu", None, "1.20.1"), "cpu@1.20.1");
        assert_eq!(ort_load_scope_key("cuda", None, "1.20.1"), "cuda@1.20.1");
        // A specific adapter index is folded into the scope so a bad adapter does
        // not block a different, healthy one.
        assert_eq!(
            ort_load_scope_key("directml", Some(1), "1.19.0"),
            "directml:1@1.19.0"
        );
        assert_eq!(
            ort_load_scope_key("cuda", Some(0), "1.20.1"),
            "cuda:0@1.20.1"
        );
    }

    #[test]
    fn read_ort_load_guard_handles_missing_partial_and_full_entries() {
        let scope = "cpu@1.20.1";
        // Missing map entirely.
        assert_eq!(
            read_ort_load_guard(&json!({}), scope),
            OrtLoadGuard {
                attempted: false,
                succeeded: false
            }
        );
        // Map present but scope missing.
        assert_eq!(
            read_ort_load_guard(&json!({"General": {"ort_load_state": {}}}), scope),
            OrtLoadGuard {
                attempted: false,
                succeeded: false
            }
        );
        // Partial entry: only `attempted` present.
        assert_eq!(
            read_ort_load_guard(
                &json!({"General": {"ort_load_state": {scope: {"attempted": true}}}}),
                scope
            ),
            OrtLoadGuard {
                attempted: true,
                succeeded: false
            }
        );
        // Full entry.
        assert_eq!(
            read_ort_load_guard(
                &json!({"General": {"ort_load_state": {scope: {"attempted": true, "succeeded": true}}}}),
                scope
            ),
            OrtLoadGuard {
                attempted: true,
                succeeded: true
            }
        );
        // Non-boolean fields fall back to false.
        assert_eq!(
            read_ort_load_guard(
                &json!({"General": {"ort_load_state": {scope: {"attempted": "yes", "succeeded": 1}}}}),
                scope
            ),
            OrtLoadGuard {
                attempted: false,
                succeeded: false
            }
        );
        // A different scope is unaffected by an entry for another scope.
        assert_eq!(
            read_ort_load_guard(
                &json!({"General": {"ort_load_state": {"cuda@1.20.1": {"attempted": true}}}}),
                scope
            ),
            OrtLoadGuard {
                attempted: false,
                succeeded: false
            }
        );
    }

    // macOS-only: the bundle detection and its data-root redirect are gated
    // behind `#[cfg(target_os = "macos")]`, so this test compiles and runs only
    // on the macOS build (verified there), keeping Linux/Windows byte-identical.
    #[cfg(target_os = "macos")]
    #[test]
    fn detects_macos_app_bundle_layout() {
        // Canonical installed bundle layout -> inside a bundle.
        assert!(is_inside_macos_app_bundle(Path::new(
            "/Applications/ManhwaStudio.app/Contents/MacOS/manhwastudio_rs"
        )));
        // Plain unpacked folder (portable layout on macOS) -> not a bundle.
        assert!(!is_inside_macos_app_bundle(Path::new(
            "/Users/alice/ManhwaStudio/manhwastudio_rs"
        )));
        // Correct leaf dirs but the top level is not a `.app` -> not a bundle.
        assert!(!is_inside_macos_app_bundle(Path::new(
            "/opt/pkg/Contents/MacOS/manhwastudio_rs"
        )));
        // Missing the `Contents` level -> not a bundle.
        assert!(!is_inside_macos_app_bundle(Path::new(
            "/Applications/ManhwaStudio.app/MacOS/manhwastudio_rs"
        )));
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
