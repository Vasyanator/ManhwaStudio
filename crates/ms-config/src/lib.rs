/*
FILE OVERVIEW: crates/ms-config/src/lib.rs
Crate root of `ms-config`: global JSON config helpers and default payloads.
Re-exported by the binary as `crate::config`, so every `crate::config::...` call site
keeps working unchanged.

Main items:
- Path constants for project/user data files, model roots, and folders.
- `program_dir` / `data_dir`: runtime root for bundled helpers/assets and writable runtime
  data: the launch working directory, else the executable directory, else a repository build's
  checkout (`<repo>/target/<profile>/<exe>`), whichever first holds the program markers
  (`dir_has_program_markers`); pure precedence and `repo_build_root` in module `runtime_root`.
- `default_projects_root` / `projects_root_from_user_settings`: resolve projects directory
  (default `{Documents}/manhwastudio_projects`, override from `user_config.json`).
- `JsonConfig`: load/merge/set wrapper for JSON configs with default backfilling that
  preserves an already-complete file without rewriting it (all I/O via `ms_docstore`).
- `user_config_doc` / `project_settings_doc`: the `ms_docstore::DocRef`s of the two documents.
- `update_user_config_file`: serialized read-modify-write boundary for `user_config.json`
  (`ms_docstore::update` under the per-document lock).
- `merge_missing`: the default-backfill rule (insert absent keys, never replace values).
- `user_config_defaults` / `project_config_defaults`: default trees for global and project settings.
- `AiInstallType`: installed AI dependency level recorded in `user_config.json`.
- `Flux2Variant`: which FLUX.2 klein checkpoint an «ИИ-редактор области» engine works with
  (9B / 4B). It keys the model directory, the three component directories, the engine's
  settings file below, the engine id and the two capability gates; its PRESENTATION (picker
  caption, Hugging Face repository) is the `Flux2VariantPresentation` extension trait in
  `crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux2_klein/variant_presentation.rs`.
- `AiRuntime`: selected AI runtime (`backend`/`native`) recorded under `General.ai_runtime`.
- `OrtLoadGuard` / `OrtLoadDecision` / `ort_load_decision` / `ort_load_scope_key` /
  `read_ort_load_guard`: per-scope ONNX Runtime SIGILL load-guard model and its pure
  decision logic (state persisted under `General.ort_load_state`).
- `MemoryProfile`: persisted global image-cache memory policy recorded under `General`.
- `StorageMode` (module `storage_mode`): Dev (`.json`) / Prod (`.db`) document storage mode
  under `General.storage_mode`, plus the startup probe seeding the docstore default format.
- `ui_scale_percent_from_user_settings` / `clamp_ui_scale_percent` / `ui_scale_factor_from_percent`:
  global interface scale (`General.ui_scale_percent`) and its conversion to the egui zoom factor.
- `autosave_policy_from_user_settings`: the clamped autosave interval / action threshold
  (`General.autosave_interval_minutes` / `General.autosave_action_threshold`); the runtime
  global lives in module `autosave_policy`.
- `load_user_config`: canonical entry-point for `user_config.json` with persistence.
- `mark_first_run_languages_if_needed` / `user_settings_first_run_languages_pending`:
  fresh-install detection and gate for the launcher's first-run language modal via the
  tri-state marker `General.first_run_languages_confirmed` (never in the defaults tree).
- `load_raw_user_settings_for_startup`: startup-safe read before default backfilling.
- `load_user_settings_for_startup`: startup-safe read of user settings without creating files.
- `single_image` (module): single-image mode input types, scratch root and `SingleImage` section.
*/

#![warn(clippy::all)]

// `t!` / `tf!` for the localized labels of the bubble-status rule model.
#[macro_use]
extern crate ms_i18n;

// The set of editor tabs. It lives here because the `General.enabled_tabs` default
// tree is built from `AppTab::key()`; the binary re-exports it as `crate::app_tab`
// and `tabs::mod` re-exports it again, so both existing paths keep working.
pub mod app_tab;
// GUI-free half of the bubble status rule model (the default preset is part of
// `Canvas.bubble_status_rules`). The border PAINTING stays in the binary.
pub mod bubble_status;
// Runtime selection for the typing tab's Ctrl+wheel rotation. It lives here because
// the `TextTab` default names `DEFAULT_ROTATION_CTRL_WHEEL_MODE`.
pub mod rotation_ctrl_wheel;
// Process-global autosave policy (interval + action threshold) read live by
// `ms_models::autosave_gate`. It lives here because its keys and defaults are part of the
// `General` default tree, and both its seeder (the binary) and its editor (the shared
// general-settings pane) already depend on this crate.
pub mod autosave_policy;

// The crash-safe `General.ort_load_state` guard writers. They live here because the
// WRITER is the native ONNX Runtime loader (crate `ms-native-runtime`) and the READER
// (`read_ort_load_guard` / `ort_load_decision`) is already in this crate; the settings
// tab, where they used to sit, only offers the retry control and re-exports them.
pub mod ort_load_guard;

// The settings-surface deep-link target enum. It lives here because its REQUESTERS (the
// typing tab, a crate of its own) and its CONSUMER (the binary's settings tab) may not
// depend on each other; the binary re-exports it from `settings_shared.rs`.
pub mod settings_deep_link;

// The global Dev/Prod document storage mode (`General.storage_mode`) and its startup
// probe. The conversion driver lives in `ms-project` (it enumerates titles); this crate
// owns the key, the typed value, and the seed of `ms_docstore::set_default_format`.
pub mod storage_mode;
pub use storage_mode::{GENERAL_STORAGE_MODE_KEY, StorageMode, storage_mode_from_user_settings};

// Single-image editing mode: the readable input-type table, the scratch-root resolver and
// the `SingleImage` section of `user_config`. It lives here because the table is pure data
// read by crates that do not know each other (launcher, installer, project, binary) and
// the section's default belongs to the `user_config` default tree below.
pub mod single_image;

// The pure runtime-root rules (precedence over a marker oracle, and the repository-build
// rule). A module of its own so every row is unit-tested without a file system;
// `repo_build_root` is re-exported because `ms-os-integration` writes records with it.
pub mod runtime_root;
pub use runtime_root::repo_build_root;

// The debouncing, retrying writer thread every self-owned section of `user_config.json`
// is written through. It sits here because its write step IS this crate's
// `update_user_config_file` border; its feeders (window geometry, panel-dock layout)
// stay above. The binary re-exports it as `crate::config_saver`.
pub mod config_saver;
// On-disk editable UI locale catalog (`data_dir()/locale`): unpack + reconcile the
// catalogs `ms-i18n` embeds, then install the active one. It reads the active locale
// out of `user_config.json`, so it belongs to this layer. Native-only, exactly as in
// the binary: there is no folder next to an executable on wasm, where `web_entry.rs`
// installs the embedded catalog directly.
#[cfg(not(target_arch = "wasm32"))]
pub mod locale_store;

// Pure composition/stripping of the application version string. It lives in this crate
// because it is std-only, dependency-free, and read from both ends of the app (the
// launcher's update check and the installer's release comparison). The root `build.rs`
// pulls the SAME file in with `include!("crates/ms-config/src/version_format.rs")`, so
// the code the build script runs is the code `cargo test` covers; keep it std-only.
pub mod version_format;

use crate::bubble_status::default_bubble_status_rules_value;
use anyhow::{Context, Result};
use ms_log::runtime_log;
use ms_memory::MemoryProfile;
use ms_docstore::{DocKind, DocRef, DocStoreError, WriteOptions};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};

#[allow(dead_code)]
pub const VERSION: &str = "2.11.1";

#[allow(dead_code)]
pub const DEFAULT_PROJECT: &str = "";
#[allow(dead_code)]
pub const DEBUG_CONSOLE: bool = false;

pub const BUBBLES_FILE: &str = "translation_bubbles.json";
pub const NOTES_FILE: &str = "translation_notes.txt";
/// Title-scoped favorite characters of the typing tab's character table. Lives
/// next to the title's other shared files, so every chapter sees one list.
pub const CHAR_FAVORITES_FILE: &str = "char_favorites.json";
/// Title-scoped color presets of the typing tab's color pickers. Title- rather
/// than chapter-scoped for the same reason as the favorites above: every chapter
/// of one manga is typeset with one set of colors.
pub const COLOR_PRESETS_FILE: &str = "color_presets.json";
pub const SRC_DIR: &str = "src";
pub const CLEANED_DIR: &str = "cleaned";
pub const CLEAN_LAYERS_DIR: &str = "clean_layers";
pub const ALT_VERS_DIR: &str = "alt_vers";
pub const SAVED_DIR: &str = "saved";
pub const TEXT_IMAGES_DIR: &str = "text_images";
pub const LAYERS_DIR: &str = "layers";
pub const TEXT_DETECTION_DIR: &str = "text_detection";
pub const CHARACTERS_DIR: &str = "characters";
/// The title's characters document inside [`CHARACTERS_DIR`].
pub const CHARACTERS_FILE: &str = "characters.json";
pub const TERMS_FILE: &str = "terms.json";
pub const PROJECT_SETTINGS_FILE: &str = "settings.json";
pub const USER_CONFIG_FILE: &str = "user_config.json";
pub const GENERAL_PROJECTS_DIR_KEY: &str = "projects_dir";
pub const GENERAL_AI_INSTALL_TYPE_KEY: &str = "ai_install_type";
/// `General` key selecting which AI runtime the app drives: the external Python
/// backend process (`"backend"`) or the in-process native ONNX Runtime path
/// (`"native"`). Parsed by [`AiRuntime::from_user_settings`], but only honored when
/// [`GENERAL_AI_RUNTIME_CONFIGURED_KEY`] marks it as an explicit user choice.
pub const GENERAL_AI_RUNTIME_KEY: &str = "ai_runtime";
/// `General` boolean marking [`GENERAL_AI_RUNTIME_KEY`] as an explicit user choice,
/// mirroring the ONNX `*_configured` flags. Set to `true` by [`save_ai_runtime`]
/// when the user picks a runtime via the AI-backend switch. When absent or not
/// `true`, [`AiRuntime::from_user_settings`] treats the stored `ai_runtime` token as
/// a pre-flag default (not a decision) and applies the native default instead.
pub const GENERAL_AI_RUNTIME_CONFIGURED_KEY: &str = "ai_runtime_configured";
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
/// `General` key holding the selected ONNX Runtime BUILD slug (e.g. `"cuda13"`,
/// `"openvino"`, `"webgpu"`), the `slug` of an `onnx_runtime::builds` catalog row. A
/// build is a concrete onnxruntime binary of a specific version exposing a specific
/// execution-provider set; the native path resolves the dylib for this build and
/// validates the selected provider belongs to it. Read by
/// [`ai_onnx_build_from_user_settings`]; kept as a plain string so `config` stays free
/// of an `onnx_runtime` dependency (the caller applies the per-OS default).
pub const GENERAL_AI_ONNX_BUILD_KEY: &str = "ai_onnx_build";
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
/// `General.ui_language`: the interface language tag (an `ms_i18n::LocaleTag`, or
/// a custom on-disk locale tag). Read at startup by `locale_store::install_ui_locale`
/// and written by the shared general-settings widget's UI-language selector.
pub const GENERAL_UI_LANGUAGE_KEY: &str = "ui_language";
/// `General.ui_scale_percent`: the global interface scale in PERCENT (`100` = native
/// size). Applied to an egui context as `Context::set_zoom_factor(percent / 100)`, so
/// it scales every point-sized element at once (fonts, spacing, widget sizes) without
/// touching the OS window size. Read by [`ui_scale_percent_from_user_settings`] and
/// written by the shared general-settings widget's scale slider.
pub const GENERAL_UI_SCALE_PERCENT_KEY: &str = "ui_scale_percent";
/// Smallest selectable interface scale, in percent. Below this the UI stops being
/// legible on a normal-DPI display; egui itself would accept down to 10 %.
pub const UI_SCALE_PERCENT_MIN: u32 = 50;
/// Largest selectable interface scale, in percent.
pub const UI_SCALE_PERCENT_MAX: u32 = 200;
/// Default interface scale: native size, i.e. `zoom_factor == 1.0`.
pub const UI_SCALE_PERCENT_DEFAULT: u32 = 100;
/// `General.autosave_interval_minutes`: how long, in minutes, pending edits may stay in
/// memory — counted from the FIRST pending action — before the background writers flush
/// them to the chapter's `_unsaved` folder. Read by [`autosave_policy_from_user_settings`].
pub const GENERAL_AUTOSAVE_INTERVAL_MINUTES_KEY: &str = "autosave_interval_minutes";
/// `General.autosave_action_threshold`: number of pending save gestures that forces a
/// flush before the interval elapses. `1` flushes on every action (immediate mode); there
/// is deliberately no `0` meaning.
pub const GENERAL_AUTOSAVE_ACTION_THRESHOLD_KEY: &str = "autosave_action_threshold";
/// Smallest selectable autosave interval, in minutes.
pub const AUTOSAVE_INTERVAL_MINUTES_MIN: u32 = 1;
/// Largest selectable autosave interval, in minutes.
pub const AUTOSAVE_INTERVAL_MINUTES_MAX: u32 = 60;
/// Default autosave interval, in minutes.
pub const AUTOSAVE_INTERVAL_MINUTES_DEFAULT: u32 = 3;
/// Smallest selectable autosave action threshold (`1` = flush on every action).
pub const AUTOSAVE_ACTION_THRESHOLD_MIN: u32 = 1;
/// Largest selectable autosave action threshold.
pub const AUTOSAVE_ACTION_THRESHOLD_MAX: u32 = 500;
/// Default autosave action threshold.
pub const AUTOSAVE_ACTION_THRESHOLD_DEFAULT: u32 = 30;
/// `General.first_run_languages_confirmed`: tri-state marker gating the launcher's
/// first-run language modal.
///
/// Deliberately ABSENT from [`user_config_defaults`] — [`JsonConfig::apply_defaults`]
/// must NEVER materialize it, because its very absence is a meaningful state:
/// - key absent: the feature never triggered (an existing install whose language keys
///   were already set, or a pre-feature install) — the modal must never show;
/// - `false`: a fresh install was detected but the user has not confirmed yet — show
///   the modal;
/// - `true`: the user confirmed the languages — never show the modal again.
///
/// Written to `false` by [`mark_first_run_languages_if_needed`] and to `true` by the
/// launcher modal's confirm path. Gate the modal with
/// [`user_settings_first_run_languages_pending`].
pub const GENERAL_FIRST_RUN_LANGUAGES_CONFIRMED_KEY: &str = "first_run_languages_confirmed";
/// Schema version of the `PanelLayout` section of `user_config.json`. The config file
/// has no global version, so the section carries its own (precedent: `fonts_data.rs`).
/// The section itself is owned by `widgets::panel_dock::persist`, which re-exports this
/// constant; bump it there and here together with the shape.
pub const PANEL_LAYOUT_SECTION_VERSION: u32 = 1;
pub const TEXT_TAB_HANGING_PUNCTUATION_KEY: &str = "hanging_punctuation";
pub const TEXT_TAB_ROTATION_CTRL_WHEEL_MODE_KEY: &str = "rotation_ctrl_wheel_mode";
/// `TextTab` key holding the advanced text-form search knobs as ONE JSON object
/// (`evenness`, `aspect_max`, `hyphen_ratio`, …). The shape is owned by
/// `tabs::typing::advanced_form_params` (`to_config_value` / `from_config_value`);
/// nothing else may spell its field names.
///
/// Deliberately ABSENT from [`user_config_defaults`]: every field already has a
/// compiled-in default and a partial object is a supported input, so materializing
/// the whole object would only add eight keys to every user's config for nothing.
/// It is written once the user actually changes a knob.
pub const TEXT_TAB_ADVANCED_FORM_SEARCH_KEY: &str = "advanced_form_search";
/// `TextTab` key holding the selected typesetting language as a BCP-47-style tag
/// (e.g. `"ru"`). Selects the hyphenation/segmentation engine; seeds
/// `ms_text_util::language` at startup. Default `"ru"` preserves prior behavior.
pub const TEXT_TAB_TEXT_LANGUAGE_KEY: &str = "text_language";
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

/// Which AI runtime the application drives, persisted under `General.ai_runtime`
/// in `user_config.json`.
///
/// `Backend` routes inference through the external Python `ai_backend.py` process.
/// `Native` uses the in-process ONNX Runtime path (`crate`-native, loaded lazily
/// behind the SIGILL load guard). `Native` is the default runtime; `Backend`
/// applies only when the user explicitly selects it via the AI-backend switch. See
/// [`AiRuntime::from_user_settings`] for the effective-runtime contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRuntime {
    Backend,
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

    /// Resolves the EFFECTIVE AI runtime from a raw user-settings tree.
    ///
    /// The native ONNX path is the default. `Backend` is returned only when the
    /// user made an EXPLICIT choice, recorded by [`save_ai_runtime`] via the
    /// `General.ai_runtime_configured` boolean:
    ///
    /// - `ai_runtime_configured` missing or not `true`: return [`AiRuntime::Native`],
    ///   IGNORING any stored `ai_runtime` token. Such a config either is a fresh
    ///   install or predates explicit runtime tracking, so its stored token is a
    ///   default rather than a user decision. This migrates both fresh installs and
    ///   upgraders to the native default in a single step.
    /// - `ai_runtime_configured == true`: honor the stored `ai_runtime` token
    ///   (`"native"` -> `Native`, `"backend"` -> `Backend`, whitespace trimmed). A
    ///   missing or unparseable token while configured is `true` falls back to the
    ///   native default.
    ///
    /// Panic-free and allocation-light: reads borrowed JSON without cloning. This
    /// resolves the DEFAULT selection only; the native path still falls back to the
    /// backend through the separate SIGILL load guard when native loading is unsafe.
    #[must_use]
    pub fn from_user_settings(cfg: &Value) -> Self {
        let general = cfg.get("General").and_then(Value::as_object);
        let configured = general
            .and_then(|general| general.get(GENERAL_AI_RUNTIME_CONFIGURED_KEY))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !configured {
            // No explicit choice recorded: apply the native default regardless of any
            // stored token (fresh install or pre-flag upgrade).
            return Self::Native;
        }
        general
            .and_then(|general| general.get(GENERAL_AI_RUNTIME_KEY))
            .and_then(Value::as_str)
            .map(str::trim)
            .map(parse_ai_runtime_token)
            // Explicit choice but missing/unparseable token: fall back to native.
            .unwrap_or(Self::Native)
    }
}

/// Maps a trimmed `General.ai_runtime` token to a runtime.
///
/// Recognizes `"backend"`; every other token (including `"native"` and unknown
/// values) resolves to the native default.
fn parse_ai_runtime_token(token: &str) -> AiRuntime {
    match token {
        "backend" => AiRuntime::Backend,
        _ => AiRuntime::Native,
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

/// Reads the selected ONNX Runtime build slug from `General.ai_onnx_build`.
///
/// Returns the trimmed build slug (e.g. `"cuda13"`) or `None` when the key is absent,
/// empty, non-string, or the `"not-selected"` sentinel. On `None` the caller applies
/// `onnx_runtime::builds::default_build_for_current_os()`; the slug is validated
/// against the build catalog by `native_runtime`. Kept slug-agnostic here so `config`
/// stays free of an `onnx_runtime` dependency.
#[must_use]
pub fn ai_onnx_build_from_user_settings(cfg: &Value) -> Option<String> {
    cfg.get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_AI_ONNX_BUILD_KEY))
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

/// True when `dir` holds a program-files marker (`ai_backend.py`, `installer_files` or
/// `modules`): the one rule that decides whether a directory is a ManhwaStudio runtime root.
/// [`program_dir`] / [`data_dir`] pick the launch directory, the executable directory or a
/// repository build's checkout by it ([`runtime_root`]);
/// OS-integration code uses it to tell an installed or unpacked copy from a bare build output.
/// Blocking filesystem probe (three `exists` checks); a missing or unreadable `dir` is `false`.
pub fn dir_has_program_markers(dir: &Path) -> bool {
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

/// Resolve the app's runtime root: the macOS `.app` bundle data root when inside a bundle, else
/// the portable rule of [`runtime_root::resolve_runtime_root_with`] over the real working
/// directory, executable path and marker probe. Blocking (a few `exists` probes).
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

    // An unreadable working directory or executable path only removes that candidate; the
    // precedence itself (cwd, exe dir, repository root, fallback) is the pure
    // `runtime_root::resolve_runtime_root_with`.
    let cwd = std::env::current_dir().ok();
    let exe = std::env::current_exe().ok();
    runtime_root::resolve_runtime_root_with(cwd.as_deref(), exe.as_deref(), &dir_has_program_markers)
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

/// Dedicated settings file for the «Удаление водяных знаков» cleaning tool
/// (model, mode, tiling and mask parameters), kept out of `user_config.json`
/// for the same reason as the SDXL and FLUX ones: its background saves must not
/// race the canvas-settings saver.
#[must_use]
pub fn watermark_removal_settings_path() -> PathBuf {
    data_dir().join("watermark_removal_settings.json")
}

/// Dedicated settings file of one «FLUX.2 klein» region-edit engine, kept out of
/// `user_config.json` for the same reason as the SDXL, FLUX.1-Fill and watermark
/// ones: its background saves must not race the canvas-settings saver.
///
/// ONE FILE PER VARIANT: the two checkpoints have different model paths, different
/// memory behaviour and their own prompt, so a shared file would make selecting the
/// other variant rewrite the first one's configuration. [`Flux2Variant::Klein9B`] keeps
/// the historic name unchanged, because users already have that file on disk.
#[must_use]
pub fn flux2_klein_settings_path(variant: Flux2Variant) -> PathBuf {
    data_dir().join(variant.settings_file_name())
}

/// Dedicated settings file of the «Lama» region-edit engine (the selected model and
/// the parameters of both backend methods), kept out of `user_config.json` for the
/// same reason as the SDXL, FLUX.1-Fill and watermark ones: its background saves must
/// not race the canvas-settings saver.
///
/// ONE file for all catalog entries, unlike `flux2_klein_settings_path`: the selected
/// model IS one of the persisted fields here, so a per-model file could not record
/// which model to restore.
#[must_use]
pub fn lama_engine_settings_path() -> PathBuf {
    data_dir().join("lama_engine_settings.json")
}

/// Dedicated settings file of the «ИИ редактирование (API)» cleaning tool (the selected
/// cloud provider, each provider's model and endpoint, the prompt and the mask blend), kept
/// out of `user_config.json` for the same reason as the engine files above: its background
/// saves must not race the canvas-settings saver.
///
/// It never holds an API key: keys live only in the OS credential store.
#[must_use]
pub fn ai_api_edit_settings_path() -> PathBuf {
    data_dir().join("ai_api_edit_settings.json")
}

/// Root of the reusable watermark LIBRARY: one self-contained directory per entry
/// (metadata JSON, the `c`/`s` planes, the correlation template and the calibration
/// crops that produced it).
///
/// The library belongs to the installation, not to a project: an entry measured on
/// one chapter is what makes the next chapter of the same publisher instant, and an
/// entry directory is meant to be copyable and shareable as a folder.
///
/// An entry's own directory is this root joined with its id. The id is persisted
/// literal identity and must be validated as a single safe path segment first
/// (`watermark_library::is_valid_entry_id`), or a hand-edited one could escape the
/// root.
#[must_use]
pub fn watermark_library_dir() -> PathBuf {
    data_dir().join("watermark_library")
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

/// Reads `General.ui_scale_percent`, clamped to
/// [`UI_SCALE_PERCENT_MIN`]..=[`UI_SCALE_PERCENT_MAX`].
///
/// A missing, non-numeric, or out-of-range value resolves to
/// [`UI_SCALE_PERCENT_DEFAULT`] / the nearest bound, so a hand-edited config can never
/// make the interface unusable. Accepts a float too (an older/hand-written `90.0`),
/// rounding to the nearest percent.
#[must_use]
pub fn ui_scale_percent_from_user_settings(user_settings: &Value) -> u32 {
    let raw = user_settings
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_UI_SCALE_PERCENT_KEY))
        .and_then(Value::as_f64);
    let Some(raw) = raw else {
        return UI_SCALE_PERCENT_DEFAULT;
    };
    if !raw.is_finite() {
        return UI_SCALE_PERCENT_DEFAULT;
    }
    clamp_ui_scale_percent(raw.round())
}

/// Clamps a raw percent value into the supported range as a `u32`.
///
/// Takes `f64` because the JSON value is read as one; a non-finite input resolves to
/// [`UI_SCALE_PERCENT_DEFAULT`]. Rust has no `TryFrom<f64> for u32`, so the final
/// conversion is an `as` cast — it is proven safe here because the value is clamped
/// into `50..=200` first, where truncation and wrap-around are both impossible.
#[must_use]
pub fn clamp_ui_scale_percent(percent: f64) -> u32 {
    if !percent.is_finite() {
        return UI_SCALE_PERCENT_DEFAULT;
    }
    if percent <= f64::from(UI_SCALE_PERCENT_MIN) {
        return UI_SCALE_PERCENT_MIN;
    }
    if percent >= f64::from(UI_SCALE_PERCENT_MAX) {
        return UI_SCALE_PERCENT_MAX;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "value is finite and strictly inside 50..200 by the guards above"
    )]
    let percent = percent.round() as u32;
    percent
}

/// Converts an interface-scale percent into the egui zoom factor
/// (`Context::set_zoom_factor`): `100` → `1.0`. Out-of-range input is clamped first.
#[must_use]
pub fn ui_scale_factor_from_percent(percent: u32) -> f32 {
    let percent = percent.clamp(UI_SCALE_PERCENT_MIN, UI_SCALE_PERCENT_MAX);
    // Clamped into 50..=200, so `u16` always fits; the fallback only keeps this total.
    f32::from(u16::try_from(percent).unwrap_or(100)) / 100.0
}

/// Reads the autosave policy from `General.autosave_interval_minutes` and
/// `General.autosave_action_threshold`, each clamped into its `AUTOSAVE_*_MIN..=MAX`
/// range.
///
/// A missing or non-numeric value resolves to its `AUTOSAVE_*_DEFAULT`; an out-of-range
/// one (including negative) to the nearest bound; a float is rounded. Pure: the runtime
/// global is seeded by `autosave_policy::seed_autosave_policy_from_user_settings`.
#[must_use]
pub fn autosave_policy_from_user_settings(user_settings: &Value) -> autosave_policy::AutosavePolicy {
    let general = user_settings.get("General").and_then(Value::as_object);
    let read = |key: &str, min: u32, max: u32, default: u32| -> u32 {
        general.and_then(|general| general.get(key)).map_or(default, |value| clamp_u32_setting(value, min, max, default))
    };
    let minutes = read(GENERAL_AUTOSAVE_INTERVAL_MINUTES_KEY, AUTOSAVE_INTERVAL_MINUTES_MIN, AUTOSAVE_INTERVAL_MINUTES_MAX, AUTOSAVE_INTERVAL_MINUTES_DEFAULT);
    let threshold = read(GENERAL_AUTOSAVE_ACTION_THRESHOLD_KEY, AUTOSAVE_ACTION_THRESHOLD_MIN, AUTOSAVE_ACTION_THRESHOLD_MAX, AUTOSAVE_ACTION_THRESHOLD_DEFAULT);
    autosave_policy::AutosavePolicy { interval: std::time::Duration::from_secs(u64::from(minutes) * 60), action_threshold: threshold }
}

/// Clamps a JSON number into `min..=max` (`min <= max`); a non-number or non-finite value
/// yields `default`. Integers are compared exactly; a float is rounded first.
fn clamp_u32_setting(value: &Value, min: u32, max: u32, default: u32) -> u32 {
    if let Some(unsigned) = value.as_u64() {
        return u32::try_from(unsigned.clamp(u64::from(min), u64::from(max))).unwrap_or(max);
    }
    if value.as_i64().is_some() {
        // `as_u64` failed, so this integer is negative: below every bound.
        return min;
    }
    let Some(raw) = value.as_f64().filter(|raw| raw.is_finite()) else {
        return default;
    };
    let rounded = raw.round();
    if rounded <= f64::from(min) {
        return min;
    }
    if rounded >= f64::from(max) {
        return max;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "value is finite, integral and strictly inside min..max by the guards above"
    )]
    let clamped = rounded as u32;
    clamped
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

/// Which FLUX.2 klein checkpoint an «ИИ-редактор области» engine instance works with.
///
/// The two variants are separate ENGINES in the picker sharing one implementation, and
/// this is the key that parameterizes it: it names the model directory under
/// [`side_models_dir`], the engine's own settings file, and the token the download
/// methods carry on the wire (`"9b"` / `"4b"`).
///
/// It lives here because those are runtime PATH decisions, which belong in this file
/// (`src/MODULE_README.md`), together with the facts that key an id or a capability
/// GATE and are therefore needed wherever the enum travels: [`Self::engine_id`],
/// [`Self::supports_uncensored_encoder`] and [`Self::requires_hf_token`]. Its
/// PRESENTATION — the localized picker caption and the Hugging Face repository the
/// download names — belongs to the single crate that consumes it and is supplied there
/// by the `Flux2VariantPresentation` extension trait
/// (`crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux2_klein/variant_presentation.rs`),
/// because an inherent `impl` is only legal in the defining crate. Either way there is
/// exactly ONE enum and no mapping table that could drift from it.
///
/// The backend keeps only one FLUX.2 pipeline and one text encoder resident, keyed by
/// the three component paths, so selecting the other variant unloads the previous one.
/// `Default` is [`Flux2Variant::Klein9B`] for the same reason [`Flux2Variant::from_wire`]
/// falls back to it: a document or a wire frame that names no variant predates the second
/// one and describes the 9B model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Flux2Variant {
    /// FLUX.2 klein 9B — the original engine. Gated repository, licence `other`.
    #[default]
    Klein9B,
    /// FLUX.2 klein 4B — the small checkpoint. Ungated, apache-2.0.
    Klein4B,
}

impl Flux2Variant {
    /// The stable token this variant is spelled with on the wire and in the settings
    /// file. A literal by design (`dev-docs/i18n_exclusions.md` §A5).
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::Klein9B => "9b",
            Self::Klein4B => "4b",
        }
    }

    /// Reads a persisted or received token.
    ///
    /// Anything unrecognized — INCLUDING an absent field, which is what every settings
    /// file written before the second variant existed looks like — reads as
    /// [`Self::Klein9B`]: those files describe the 9B model, and loading them as 4B
    /// would point a working configuration at a directory that does not exist.
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value.trim() {
            "4b" => Self::Klein4B,
            _ => Self::Klein9B,
        }
    }

    /// Name of this variant's model directory under [`side_models_dir`]. It mirrors the
    /// repository name the backend downloads from and must not drift from
    /// `modules/ai_backend/inpaint/flux2_download.py`.
    #[must_use]
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Klein9B => "FLUX.2-klein-9B",
            Self::Klein4B => "FLUX.2-klein-4B",
        }
    }

    /// File name of this variant's settings document under [`data_dir`].
    ///
    /// The 9B name is FROZEN: users have that file on disk with hand-entered model
    /// paths in it, and renaming it would silently reset their configuration.
    #[must_use]
    pub fn settings_file_name(self) -> &'static str {
        match self {
            Self::Klein9B => "flux2_klein_settings.json",
            Self::Klein4B => "flux2_klein_4b_settings.json",
        }
    }

    /// Both variants, in the order the engine picker offers them.
    #[must_use]
    pub fn all() -> [Self; 2] {
        [Self::Klein9B, Self::Klein4B]
    }

    /// The engine's `AiEngine::id` — a widget-id stem and a log token, never shown to the
    /// user.
    ///
    /// The 9B id is FROZEN at its historic value: it is what the host's picker and every
    /// widget id derived from it already use.
    #[must_use]
    pub fn engine_id(self) -> &'static str {
        match self {
            Self::Klein9B => "flux2_klein",
            Self::Klein4B => "flux2_klein_4b",
        }
    }

    /// Whether an UNCENSORED text encoder is published for this variant.
    ///
    /// Only the 9B one has it (`ponpoke/flux2-klein-9b-uncensored-text-encoder`). For the
    /// 4B model the toggle is not drawn at all and the flag is forced off before it can
    /// reach a path or the wire ([`Flux2KleinSettings::normalized`]) — the backend refuses
    /// `variant = "4b"` together with `uncensored = true`, and offering a control that
    /// produces a refusal is worse than not offering it.
    #[must_use]
    pub fn supports_uncensored_encoder(self) -> bool {
        match self {
            Self::Klein9B => true,
            Self::Klein4B => false,
        }
    }

    /// Whether downloading this variant needs a Hugging Face access token.
    ///
    /// The 9B repository is GATED (licence `other`), so without an accepted token the
    /// download cannot even be priced. The 4B one is apache-2.0 and ungated: the token
    /// block is not drawn for it, and the empty token the request still carries is legal.
    #[must_use]
    pub fn requires_hf_token(self) -> bool {
        match self {
            Self::Klein9B => true,
            Self::Klein4B => false,
        }
    }
}

/// FLUX.2 klein: root of the diffusers tree the panel's Hugging Face download fills
/// (`transformer/`, `text_encoder/`, `text_encoder_uncensored/`, `tokenizer/`, `vae/`,
/// `scheduler/`).
///
/// The backend creates and populates it; this side needs the path because the
/// «Расцензуренный энкодер» toggle repoints `text_encoder_path` between the two encoder
/// directories with no download at all when both are already on disk.
#[must_use]
pub fn flux2_klein_dir(variant: Flux2Variant) -> PathBuf {
    side_models_dir().join(variant.dir_name())
}

/// Directory of the FLUX.2 klein transformer the download fills.
///
/// The subdirectory name mirrors `TRANSFORMER_SUBDIR` in
/// `modules/ai_backend/inpaint/flux2_download.py`; the two must not drift.
#[must_use]
pub fn flux2_klein_transformer_dir(variant: Flux2Variant) -> PathBuf {
    flux2_klein_dir(variant).join("transformer")
}

/// Directory of the FLUX.2 klein VAE the download fills.
///
/// The subdirectory name mirrors `VAE_SUBDIR` in
/// `modules/ai_backend/inpaint/flux2_download.py`; the two must not drift.
#[must_use]
pub fn flux2_klein_vae_dir(variant: Flux2Variant) -> PathBuf {
    flux2_klein_dir(variant).join("vae")
}

/// Directory of the FLUX.2 klein text encoder the toggle selects.
///
/// `uncensored` picks `text_encoder_uncensored/` over the official `text_encoder/`.
/// Both may sit on disk at once, which is what lets the toggle switch between them
/// without transferring anything. Only [`Flux2Variant::Klein9B`] has an uncensored
/// encoder published for it; the caller is what keeps `uncensored` false for the other
/// variant (`Flux2Variant::supports_uncensored_encoder`).
#[must_use]
pub fn flux2_klein_text_encoder_dir(variant: Flux2Variant, uncensored: bool) -> PathBuf {
    flux2_klein_dir(variant).join(if uncensored {
        "text_encoder_uncensored"
    } else {
        "text_encoder"
    })
}

/// Visible-watermark-removal models. Each network gets its own subdirectory
/// (`slbr/`, `wdnet/`, `splitnet/`) holding its weights and its runtime-fetched
/// source; mirrors `WATERMARK_DIR` in the Python backend's `config.py`.
pub fn watermark_removal_dir() -> PathBuf {
    side_models_dir().join("WatermarkRemoval")
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
        watermark_removal_dir(),
    ]
}

pub fn ensure_model_dirs() -> Result<()> {
    for folder in model_folders() {
        fs::create_dir_all(&folder)
            .with_context(|| format!("failed to create model dir {}", folder.display()))?;
    }
    Ok(())
}

/// A JSON configuration document with a defaults tree: `user_config.json` of a data root,
/// or a title's `settings.json`.
///
/// Every disk access goes through `ms_docstore` (atomic temp+rename writes under the
/// per-document lock). `data` is this instance's in-memory snapshot; `set` / `set_path`
/// re-read the document under the lock and edit only the named key, so a stale snapshot
/// never overwrites another writer's keys. The per-document lock is the ONLY lock of the
/// document: every writer of it, in this crate and above, serializes on it.
#[derive(Debug, Clone)]
pub struct JsonConfig {
    pub path: PathBuf,
    /// The document `path` names (kind: see [`JsonConfig::with_kind`]).
    doc: DocRef,
    defaults: Value,
    pub data: Value,
}

#[allow(dead_code)]
impl JsonConfig {
    /// Loads `path`, backfills missing defaults, and persists only a semantic change.
    ///
    /// The document kind is inferred from the file name: [`PROJECT_SETTINGS_FILE`] is
    /// [`DocKind::ProjectSettings`], anything else [`DocKind::UserConfig`]; use
    /// [`JsonConfig::with_kind`] to name it explicitly. See `with_kind` for the contract.
    ///
    /// # Errors
    /// As [`JsonConfig::with_kind`].
    pub fn new(path: impl Into<PathBuf>, defaults: Value) -> Result<Self> {
        let path = path.into();
        let kind = if path.file_name().is_some_and(|name| name == PROJECT_SETTINGS_FILE) {
            DocKind::ProjectSettings
        } else {
            DocKind::UserConfig
        };
        Self::with_kind(path, kind, defaults)
    }

    /// Loads the document at `path` of kind `kind`, backfills missing defaults, and writes
    /// it back ONLY when a default was genuinely missing: a semantically complete document
    /// is left byte-identical (formatting-only differences are never normalized).
    ///
    /// The read / decide / write transaction runs under the document lock, so a concurrent update cannot be lost.
    /// A non-object root is treated as `{}` (and rewritten when defaults are non-empty).
    ///
    /// # Errors
    /// Fails when the existing document cannot be read, does not parse (it is then left
    /// untouched), or the backfilled document cannot be written.
    pub fn with_kind(path: impl Into<PathBuf>, kind: DocKind, defaults: Value) -> Result<Self> {
        let path = path.into();
        let mut cfg = Self {
            doc: DocRef::new(&path, kind),
            path,
            defaults,
            data: Value::Object(Map::new()),
        };
        ms_docstore::with_lock(&cfg.doc.clone(), |locked| -> Result<()> {
            cfg.data = config_root_or_empty(locked.read_value(), &cfg.path)?;
            if cfg.apply_defaults() {
                locked
                    .write_value(&cfg.data, WriteOptions::default())
                    .with_context(|| format!("failed to write config {}", cfg.path.display()))?;
            }
            Ok(())
        })?;
        Ok(cfg)
    }

    /// The document this config reads and writes.
    #[must_use]
    pub fn doc(&self) -> &DocRef {
        &self.doc
    }

    /// Replaces `data` with the document on disk (`{}` when absent or not an object).
    ///
    /// # Errors
    /// Fails when the existing document cannot be read or does not parse.
    pub fn load(&mut self) -> Result<()> {
        self.data = config_root_or_empty(ms_docstore::read_value(&self.doc), &self.path)?;
        Ok(())
    }

    /// Merges absent defaults and returns whether the in-memory JSON changed.
    ///
    /// Callers use this result to avoid rewriting files that already contain the
    /// complete semantic configuration, including files with different whitespace.
    pub fn apply_defaults(&mut self) -> bool {
        merge_missing(&mut self.data, &self.defaults)
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

    /// Sets one top-level `key` in ONE serialized read-modify-write of the document and
    /// refreshes `data` from the result. Every other key on disk survives.
    ///
    /// # Errors
    /// As [`JsonConfig::set_path`].
    pub fn set(&mut self, key: &str, value: Value) -> Result<()> {
        self.set_path(&[key], value)
    }

    /// Sets the value at `path` (intermediate non-objects are replaced by objects; an
    /// empty `path` replaces the root) in ONE serialized read-modify-write of the
    /// document, then refreshes `data` from what was written. A non-object root on disk is
    /// treated as `{}`.
    ///
    /// # Errors
    /// Fails when the existing document cannot be read or does not parse (it is then left
    /// untouched), or the result cannot be written.
    pub fn set_path(&mut self, path: &[&str], value: Value) -> Result<()> {
        let written = ms_docstore::update(&self.doc, WriteOptions::default(), |root| {
            if !root.is_object() {
                *root = Value::Object(Map::new());
            }
            set_value_at_path(root, path, value);
            Ok(root.clone())
        })
        .with_context(|| format!("failed to update config {}", self.path.display()))?;
        self.data = written;
        Ok(())
    }
}

/// Places `value` at `path` inside `root`, replacing any non-object on the way by an empty
/// object. An empty `path` replaces `root` itself.
fn set_value_at_path(root: &mut Value, path: &[&str], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        *root = value;
        return;
    };
    let mut cur = root;
    for part in parents {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        let Value::Object(obj) = cur else { return };
        cur = obj.entry((*part).to_owned()).or_insert_with(|| Value::Object(Map::new()));
    }
    if !cur.is_object() {
        *cur = Value::Object(Map::new());
    }
    // `cur` was made an object on the line above, so this branch always inserts.
    if let Value::Object(obj) = cur {
        obj.insert((*last).to_owned(), value);
    }
}

/// Converts a docstore read of a config document into its object root: absent or
/// non-object ⇒ `{}`; read/parse failures become `anyhow` errors naming `path`.
fn config_root_or_empty(read: ms_docstore::Result<Option<Value>>, path: &Path) -> Result<Value> {
    match read {
        Ok(Some(value)) if value.is_object() => Ok(value),
        Ok(Some(_) | None) => Ok(Value::Object(Map::new())),
        Err(err) => Err(config_read_error(err, path)),
    }
}

/// Wraps a failed docstore READ of the config document at `path` in the historical
/// `anyhow` context: "failed to parse config …" for a malformed document, "failed to read
/// config …" otherwise.
fn config_read_error(err: DocStoreError, path: &Path) -> anyhow::Error {
    let step = match &err {
        DocStoreError::Malformed { .. } => "parse",
        DocStoreError::Io { .. }
        | DocStoreError::Storage(_)
        | DocStoreError::Write(_)
        | DocStoreError::Serialize { .. }
        | DocStoreError::Conflict { .. }
        | DocStoreError::Ambiguous { .. }
        | DocStoreError::Unsupported { .. }
        | DocStoreError::Mutator(_)
        | DocStoreError::Quarantine { .. } => "read",
    };
    anyhow::Error::new(err).context(format!("failed to {step} config {}", path.display()))
}

/// The current data root's `user_config.json` document.
#[must_use]
pub fn user_config_doc() -> DocRef {
    DocRef::new(user_config_path(), DocKind::UserConfig)
}

/// The `settings.json` document of the title directory `title_dir`.
#[must_use]
pub fn project_settings_doc(title_dir: &Path) -> DocRef {
    DocRef::new(title_dir.join(PROJECT_SETTINGS_FILE), DocKind::ProjectSettings)
}

/// Inserts every key of `defaults` absent from `dst`, recursing into objects present on
/// both sides; existing values (including non-object values where `defaults` has an object)
/// are never replaced. Returns whether at least one value was inserted. This is THE
/// default-backfill rule of `JsonConfig` and of every other writer that must produce the
/// same document shape (the installer's install-target config).
pub fn merge_missing(dst: &mut Value, defaults: &Value) -> bool {
    let mut changed = false;
    if let (Value::Object(dst_obj), Value::Object(def_obj)) = (dst, defaults) {
        for (k, v) in def_obj {
            match dst_obj.get_mut(k) {
                Some(existing) => changed |= merge_missing(existing, v),
                None => {
                    dst_obj.insert(k.clone(), v.clone());
                    changed = true;
                }
            }
        }
    }
    changed
}

pub fn user_config_defaults() -> Value {
    let default_projects_root = default_projects_root();
    // `enabled_tabs` object keys are the stable English `AppTab::key()` ids (the
    // persistence contract). Building the default from `key()` keeps the config and
    // the enum in lockstep: adding or renaming a tab id updates both at once, and the
    // keys can never drift back to a localized `title()`. The Settings and PS-editor
    // tabs are intentionally omitted (they are always shown).
    let enabled_tabs: Value = [
        crate::app_tab::AppTab::PageManager,
        crate::app_tab::AppTab::Translation,
        crate::app_tab::AppTab::Cleaning,
        crate::app_tab::AppTab::Typing,
        crate::app_tab::AppTab::Characters,
        crate::app_tab::AppTab::Terms,
        crate::app_tab::AppTab::Notes,
        crate::app_tab::AppTab::Wiki,
    ]
    .into_iter()
    .map(|tab| (tab.key().to_owned(), Value::Bool(true)))
    .collect::<Map<String, Value>>()
    .into();
    json!({
        "General": {
            "theme": "dark",
            "style": "default",
            "ui_language": "ru",
            "ui_scale_percent": UI_SCALE_PERCENT_DEFAULT,
            "autosave_interval_minutes": AUTOSAVE_INTERVAL_MINUTES_DEFAULT,
            "autosave_action_threshold": AUTOSAVE_ACTION_THRESHOLD_DEFAULT,
            "projects_dir": default_projects_root.to_string_lossy().to_string(),
            "ai_backend_autostart": true,
            "ai_device": "not-selected",
            "ai_onnx_provider": "not-selected",
            "ai_onnx_device_id": "not-selected",
            "ai_onnx_build": "not-selected",
            "ai_max_loaded_models": 3,
            "ai_install_type": AiInstallType::None.as_str(),
            "ai_runtime": AiRuntime::Native.as_key(),
            "ai_runtime_configured": false,
            "ort_load_state": {},
            "memory_profile": MemoryProfile::default().as_config_str(),
            "storage_mode": StorageMode::default().as_config_str(),
            "typing_panel_layout": "vertical",
            // Built above from `AppTab::key()`. Older configs may still carry the
            // legacy Russian keys next to these; see the `enabled_tabs` note in
            // `src/MODULE_README.md` for why those stale keys are left inert.
            "enabled_tabs": enabled_tabs
        },
        "Canvas": {
            "scale_bubbles": true,
            "aside_min_width_px": 450,
            "aside_max_width_px": 550,
            "aside_compact_mode": "none",
            "aside_side_mode": "auto",
            "aside_second_column": true,
            "bubble_status_rules": default_bubble_status_rules_value(),
            "spellcheck_original": false,
            "spellcheck_translation": true,
            "cache_pages": true,
            "translation_status_display": "marks",
            "opengl_enabled": false,
            "opengl_device": "auto"
        },
        // Startup monitor + main-window geometry, owned by `src/window_geometry.rs`
        // (self-versioned: the file has no global schema version). The nulls are the
        // "nothing known yet" state `merge_missing` materializes for existing users:
        // `monitor` is the user's explicit choice, `auto_monitor` the largest monitor the
        // last run saw, `main` the restored (non-maximized) geometry of the studio window.
        // `maximized: true` preserves the historical hardcoded `with_maximized(true)`.
        // The version literal mirrors `window_geometry::WINDOW_SECTION_VERSION` (that module
        // is native-only, so it cannot be referenced here); a drift test in `window_geometry`
        // asserts they stay equal.
        "Window": {
            "version": 1,
            "monitor": null,
            "auto_monitor": null,
            "main": null,
            "maximized": true
        },
        // Dockable-panel arrangement, owned by `crates/ms-widgets/src/panel_dock/persist.rs`
        // (self-versioned, like `Window`, because the file has no global schema
        // version). `tabs` is keyed by `AppTab::key()` and materialized by the dock's
        // writer as soon as the user reorganises a panel; `sub_windows` lists the OS
        // windows panels were detached into, and a panel's `host` addresses one of them
        // by index. The version comes from the owning module, so the two cannot drift.
        "PanelLayout": {
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": {},
            "sub_windows": []
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
        // Section and key names are `single_image::SINGLE_IMAGE_SECTION` /
        // `SINGLE_IMAGE_JPEG_QUALITY_KEY`; a test there asserts this default reads back.
        "SingleImage": {
            "jpeg_quality": single_image::JPEG_QUALITY_DEFAULT
        },
        "TextTab": {
            "hanging_punctuation": ms_text_util::text_punctuation::DEFAULT_HANGING_PUNCTUATION,
            "text_language": ms_text_util::language::TextLanguage::Ru.tag(),
            "rotation_ctrl_wheel_mode":
                crate::rotation_ctrl_wheel::DEFAULT_ROTATION_CTRL_WHEEL_MODE
                    .as_config_str(),
            "effect_defaults": {},
            // NOTE: `imported_system_fonts` is deliberately NOT defaulted here any more.
            // Imported system fonts live in `fonts/fonts_data.json`; the legacy key is
            // read once (migration) and then DELETED by the preset migration
            // (`panel/presets_store::drop_migrated_user_config_keys`). Materializing an
            // empty default would resurrect the key on every launch and defeat that.
            // Character table ("Таблица символов"): the global favorite characters
            // (single-character strings, user order), the character cell size in
            // points, and the last selected tab (a stable, non-localized group key
            // from `panel/char_table/charset.rs`).
            "char_table_global_favorites": [],
            "char_table_font_size": 30,
            "char_table_last_group": "arrows",
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
            "aside_second_column": true,
            "aside_scale_pct": 100,
            "tabs_autosync_enabled": true,
            "auto_insert_last_character": true,
            "project_custom_spellcheck_words": "",
            "cache_pages": true,
            "translation_status_display": "marks",
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
            "include_hint_bubbles": true,
            "hint_wrap": "()",
            "hint_wrap_enabled": true,
            "hint_extra_sep": "",
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

/// Updates one `user_config.json` document in ONE serialized read-modify-write
/// (`ms_docstore::update`: document lock, atomic replace).
///
/// A missing file starts as an empty object; a non-object root is replaced by `{}` before
/// the mutator runs. Existing malformed JSON is returned as an error and is never
/// overwritten. The mutator's own error is returned unchanged and nothing is written. The
/// mutator must not call another user-config writer: the document lock is non-reentrant.
///
/// # Errors
/// The mutator's error, or a read/parse/write failure with the path in its context.
pub fn update_user_config_file(
    path: &Path,
    mutator: impl FnOnce(&mut Value) -> Result<()>,
) -> Result<()> {
    let doc = DocRef::new(path, DocKind::UserConfig);
    // The docstore mutator speaks `String`; the caller's typed `anyhow` error is kept
    // aside so it can be returned unchanged (context chain and downcasts intact).
    let mut mutator_error: Option<anyhow::Error> = None;
    let outcome = ms_docstore::update(&doc, WriteOptions::default(), |root| {
        if !root.is_object() {
            *root = Value::Object(Map::new());
        }
        mutator(root).map_err(|err| {
            let message = format!("{err:#}");
            mutator_error = Some(err);
            message
        })
    });
    match outcome {
        Ok(()) => Ok(()),
        Err(DocStoreError::Mutator(message)) => Err(mutator_error.take().unwrap_or_else(|| anyhow::anyhow!(message))),
        Err(err @ DocStoreError::Malformed { .. }) => Err(anyhow::Error::new(err).context(format!("failed to parse config {}", path.display()))),
        Err(err @ DocStoreError::Storage(_)) => Err(anyhow::Error::new(err).context(format!("failed to read config {}", path.display()))),
        Err(
            err @ (DocStoreError::Io { .. }
            | DocStoreError::Write(_)
            | DocStoreError::Serialize { .. }
            | DocStoreError::Conflict { .. }
            | DocStoreError::Ambiguous { .. }
            | DocStoreError::Unsupported { .. }
            | DocStoreError::Quarantine { .. }),
        ) => Err(anyhow::Error::new(err).context(format!("failed to write config {}", path.display()))),
    }
}

/// Section-writer core: ONE serialized read-modify-write of the user-config document at
/// `user_settings_file` with `durability`. A missing document starts as `{}`, a non-object root is replaced by
/// `{}`, and `edit` receives the root object. Every key `edit` does not touch survives.
///
/// # Errors
/// The raw [`DocStoreError`]: `Malformed` (file untouched), read or write failures.
/// Callers turn it into their message with [`user_config_error_message`].
pub(crate) fn update_user_config_root(
    user_settings_file: &Path,
    durability: ms_docstore::Durability,
    edit: impl FnOnce(&mut Map<String, Value>),
) -> ms_docstore::Result<()> {
    let doc = DocRef::new(user_settings_file, DocKind::UserConfig);
    let opts = WriteOptions { durability, ..WriteOptions::default() };
    ms_docstore::update(&doc, opts, |root| {
        if !root.is_object() {
            *root = Value::Object(Map::new());
        }
        let Value::Object(root_obj) = root else {
            return Err(t!("settings.config_io.prepare_root_error").to_string());
        };
        edit(root_obj);
        Ok(())
    })
}

/// Runs `edit` on the object stored under `root_obj[section]` (a missing or non-object
/// value starts as `{}`) and stores the result back. Every other key survives.
pub(crate) fn edit_section(root_obj: &mut Map<String, Value>, section: &str, edit: impl FnOnce(&mut Map<String, Value>)) {
    let mut section_obj = match root_obj.remove(section) {
        Some(Value::Object(obj)) => obj,
        Some(_) | None => Map::new(),
    };
    edit(&mut section_obj);
    root_obj.insert(section.to_owned(), Value::Object(section_obj));
}

/// User-facing message (and a structured log line) for a failed user-config section
/// write. Read/parse failures use the localized `settings.config_io.*` texts; write
/// failures keep the technical text the savers always returned (the store has already
/// logged them with path and cause).
pub(crate) fn user_config_error_message(user_settings_file: &Path, err: &DocStoreError) -> String {
    match err {
        DocStoreError::Malformed { cause, .. } => {
            runtime_log::log_warn(format!(
                "[config] user_config.json is malformed; left untouched, update not applied. Path: {}; Error: {cause}",
                user_settings_file.display()
            ));
            tf!("settings.config_io.parse_error", user_settings_file = user_settings_file.display(), err = cause)
        }
        DocStoreError::Storage(source) => {
            runtime_log::log_warn(format!(
                "[config] cannot read user_config.json; update not applied. Path: {}; Error: {source}",
                user_settings_file.display()
            ));
            tf!("settings.config_io.read_error", user_settings_file = user_settings_file.display(), err = source)
        }
        DocStoreError::Mutator(message) => message.clone(),
        DocStoreError::Io { .. }
        | DocStoreError::Write(_)
        | DocStoreError::Serialize { .. }
        | DocStoreError::Conflict { .. }
        | DocStoreError::Ambiguous { .. }
        | DocStoreError::Unsupported { .. }
        | DocStoreError::Quarantine { .. } => err.to_string(),
    }
}

/// Persists the advanced text-form search knobs under `TextTab.advanced_form_search`
/// as ONE JSON object, preserving every unrelated key.
///
/// `params_value` is the already-serialized knob object — its SHAPE belongs to the
/// typing tab (`AdvancedFormParams::to_config_value`), which owns the field names for
/// both the writer and the startup reader; this function only PLACES it. Splitting the
/// two is what lets the typing tab (crate `ms-tab-typing`) persist its knobs without
/// depending on the binary's settings tab, where this placement used to live.
///
/// File I/O — meant to run OFF the GUI thread (the advanced-form window spawns it on a
/// named thread). It goes through [`update_user_config_file`], which takes the document
/// lock itself, so the caller must NOT hold it (`ms_docstore::with_lock` on the user
/// config): it is not reentrant.
///
/// # Errors
/// Returns the user-facing failure of reading, parsing, serializing or writing
/// `user_settings_file`. A parse failure means the file is malformed and NOTHING was
/// written — the caller's knobs stay in effect for the session, and the user's other
/// settings survive.
pub fn save_advanced_form_search_params(
    user_settings_file: &Path,
    params_value: Value,
) -> Result<(), String> {
    update_user_config_file(user_settings_file, |root| {
        let root_obj = root
            .as_object_mut()
            .context("the user config root is not a JSON object")?;
        let mut text_tab_obj = root_obj
            .get("TextTab")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        text_tab_obj.insert(TEXT_TAB_ADVANCED_FORM_SEARCH_KEY.to_string(), params_value);
        root_obj.insert("TextTab".to_string(), Value::Object(text_tab_obj));
        Ok(())
    })
    // `{:#}` spells out the anyhow context chain (which file, which step).
    .map_err(|error| format!("{error:#}"))
}


// ---------------------------------------------------------------------------
// Targeted `user_config.json` section writers of the settings surfaces.
//
// They live HERE and not in the binary's settings tab because two of their callers
// are crates that may not depend on it: `ms-settings-ui`'s AI-backend panel (the AI
// runtime / ONNX provider / model-limit controls) and its general pane (the
// typesetting language), both of which are shared by the studio settings tab AND the
// launcher's settings page. `crate::tabs::settings` re-exports each one, so the
// settings tab's own call sites are unchanged.
//
// Shape of every writer below: ONE `update_user_config_root` transaction (serialized
// read-modify-write, atomic replace) that edits ONE section, so every unrelated key
// survives and a background saver never clobbers the ORT load-guard marker. A malformed
// document is reported, never overwritten. Contents-only durability: these are ordinary
// preferences. All of them do synchronous disk I/O, so they must run OFF the GUI thread.
// ---------------------------------------------------------------------------

/// Persists the hanging-punctuation set under `TextTab.hanging_punctuation`.
///
/// One serialized read-modify-write of the `TextTab` section; every other key survives.
/// Synchronous disk I/O: do not call from the GUI thread.
///
/// # Errors
/// A user-facing message when the document cannot be read, does not parse (it is then
/// left untouched) or cannot be written.
pub fn save_hanging_punctuation(
    user_settings_file: &Path,
    punctuation: &str,
) -> Result<(), String> {
    update_user_config_root(user_settings_file, ms_docstore::Durability::Contents, |root_obj| {
        edit_section(root_obj, "TextTab", |text_tab_obj| {
            text_tab_obj.insert(
                TEXT_TAB_HANGING_PUNCTUATION_KEY.to_string(),
                Value::String(punctuation.to_string()),
            );
        });
    })
    .map_err(|err| user_config_error_message(user_settings_file, &err))
}

/// Persists the selected typesetting language tag under `TextTab.text_language`.
///
/// `tag` must be a stable `TextLanguage` tag (`ms_text_util::language::TextLanguage::tag`).
/// One serialized read-modify-write of the `TextTab` section, so a background/GUI-thread
/// saver never clobbers the ORT load-guard marker and every unrelated key survives. Meant
/// to run off the GUI thread; spawned from the "Тайп" pane and from the shared
/// `crate::general_settings_panel` widget, which offers the same selector.
///
/// # Errors
/// A user-facing message when the document cannot be read, does not parse (it is then
/// left untouched) or cannot be written.
pub fn save_text_language(user_settings_file: &Path, tag: &str) -> Result<(), String> {
    update_user_config_root(user_settings_file, ms_docstore::Durability::Contents, |root_obj| {
        edit_section(root_obj, "TextTab", |text_tab_obj| {
            text_tab_obj.insert(
                TEXT_TAB_TEXT_LANGUAGE_KEY.to_string(),
                Value::String(tag.to_string()),
            );
        });
    })
    .map_err(|err| user_config_error_message(user_settings_file, &err))
}

/// Persists the selected AI runtime under `General.ai_runtime` in
/// `user_config.json`.
///
/// Also sets the `ai_runtime_configured` boolean to `true`, recording that the runtime is
/// now an EXPLICIT user choice so [`AiRuntime::from_user_settings`] honors the stored
/// token instead of applying the native default. One serialized read-modify-write of the
/// `General` section. Safe to call from a background thread; never on the GUI thread
/// (synchronous disk I/O).
///
/// # Errors
/// A user-facing message when the document cannot be read, does not parse (it is then
/// left untouched) or cannot be written.
// Wired into the AI runtime selector in `ai_backend_panel`.
pub fn save_ai_runtime(
    user_settings_file: &Path,
    runtime: AiRuntime,
) -> Result<(), String> {
    update_user_config_root(user_settings_file, ms_docstore::Durability::Contents, |root_obj| {
        edit_section(root_obj, "General", |general_obj| {
            general_obj.insert(
                GENERAL_AI_RUNTIME_KEY.to_string(),
                Value::String(runtime.as_key().to_string()),
            );
            // Mark the runtime as an explicit user decision so the effective-runtime
            // resolver stops applying the native default and honors this token.
            general_obj.insert(
                GENERAL_AI_RUNTIME_CONFIGURED_KEY.to_string(),
                Value::Bool(true),
            );
        });
    })
    .map_err(|err| user_config_error_message(user_settings_file, &err))
}

/// Persists the UNIFIED ONNX selection (`General.ai_onnx_provider` ORT token +
/// `General.ai_onnx_device_id` adapter index) in `user_config.json`.
///
/// These are the SAME keys the Python backend reads, so one selection drives both
/// the native path (which reads them on load) and the backend (which also honors
/// them at startup). The `*_configured` flags are set to `true` to match the
/// backend's own `device.set` write, so an offline selection is honored once the
/// backend later starts instead of being treated as "not chosen".
///
/// One serialized read-modify-write of the `General` section (mirroring
/// [`save_ai_runtime`]). Synchronous disk I/O: do not call from the GUI thread.
///
/// # Errors
/// A user-facing message when the document cannot be read, does not parse (it is then
/// left untouched) or cannot be written.
// Wired into the ONNX provider/device selector in `ai_backend_panel`.
pub fn save_onnx_provider_device(
    user_settings_file: &Path,
    provider_token: &str,
    device_id: &str,
) -> Result<(), String> {
    update_user_config_root(user_settings_file, ms_docstore::Durability::Contents, |root_obj| {
        edit_section(root_obj, "General", |general_obj| {
            general_obj.insert(
                GENERAL_AI_ONNX_PROVIDER_KEY.to_string(),
                Value::String(provider_token.to_string()),
            );
            general_obj.insert(
                GENERAL_AI_ONNX_DEVICE_ID_KEY.to_string(),
                Value::String(device_id.to_string()),
            );
            general_obj.insert(
                GENERAL_AI_ONNX_PROVIDER_CONFIGURED_KEY.to_string(),
                Value::Bool(true),
            );
            general_obj.insert(
                GENERAL_AI_ONNX_DEVICE_ID_CONFIGURED_KEY.to_string(),
                Value::Bool(true),
            );
        });
    })
    .map_err(|err| user_config_error_message(user_settings_file, &err))
}

/// Persists the selected ONNX Runtime BUILD slug under `General.ai_onnx_build` in
/// `user_config.json`.
///
/// `build_slug` is a stable slug from the `onnx_runtime::builds` catalog (e.g. `"cpu"`,
/// `"cuda13"`). It selects which onnxruntime binary the native runtime downloads/loads;
/// the native path validates it against the catalog on load (unknown → per-OS default).
/// One serialized read-modify-write of the `General` section. Synchronous disk I/O: do
/// not call from the GUI thread.
///
/// # Errors
/// A user-facing message when the document cannot be read, does not parse (it is then
/// left untouched) or cannot be written.
// Wired into the "Билд" selector in `ai_backend_panel` (native runtime only).
pub fn save_onnx_build(user_settings_file: &Path, build_slug: &str) -> Result<(), String> {
    update_user_config_root(user_settings_file, ms_docstore::Durability::Contents, |root_obj| {
        edit_section(root_obj, "General", |general_obj| {
            general_obj.insert(
                GENERAL_AI_ONNX_BUILD_KEY.to_string(),
                Value::String(build_slug.to_string()),
            );
        });
    })
    .map_err(|err| user_config_error_message(user_settings_file, &err))
}

/// Persists the maximum-loaded-models limit under `General.ai_max_loaded_models` in
/// `user_config.json`.
///
/// Stored as a JSON integer (matching the config default and the native LRU reader).
/// The value is clamped to `1..=10` by the caller/UI; the backend picks it up via
/// `device.set` when connected and from config on the next start. One serialized
/// read-modify-write of the `General` section. Synchronous disk I/O: do not call from
/// the GUI thread.
///
/// # Errors
/// A user-facing message when the document cannot be read, does not parse (it is then
/// left untouched) or cannot be written.
// Wired into the model-limit slider in `ai_backend_panel`.
pub fn save_max_loaded_models(
    user_settings_file: &Path,
    max_loaded_models: u32,
) -> Result<(), String> {
    update_user_config_root(user_settings_file, ms_docstore::Durability::Contents, |root_obj| {
        edit_section(root_obj, "General", |general_obj| {
            general_obj.insert(
                GENERAL_AI_MAX_LOADED_MODELS_KEY.to_string(),
                Value::Number(max_loaded_models.into()),
            );
        });
    })
    .map_err(|err| user_config_error_message(user_settings_file, &err))
}

#[cfg(test)]
mod settings_writer_tests {
    use super::*;
    use crate::AiRuntime;

    // Unique temp file per test to avoid cross-test/process collisions,
    // following the crate's existing `temp_dir + process id` test pattern.
    fn temp_config_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ms_ort_guard_{}_{}_{:?}.json",
            tag,
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn read_root(path: &Path) -> Value {
        let raw = fs::read_to_string(path).expect("config file written");
        serde_json::from_str::<Value>(&raw).expect("config file is valid json")
    }

    #[test]
    fn save_text_language_round_trips_and_preserves_unrelated_keys() {
        let path = temp_config_path("text_language");
        let _ = fs::remove_file(&path);

        // A pre-existing sibling key in the same section must survive the targeted
        // read-modify-write: two panes (Тайп + the shared general widget) call this.
        save_hanging_punctuation(&path, "«»").expect("seed punctuation");
        save_text_language(&path, ms_text_util::language::TextLanguage::Pl.tag())
            .expect("save language");

        let root = read_root(&path);
        let text_tab = root
            .get("TextTab")
            .and_then(Value::as_object)
            .expect("TextTab present");
        assert_eq!(
            text_tab
                .get(TEXT_TAB_TEXT_LANGUAGE_KEY)
                .and_then(Value::as_str),
            Some(ms_text_util::language::TextLanguage::Pl.tag())
        );
        assert_eq!(
            text_tab
                .get(TEXT_TAB_HANGING_PUNCTUATION_KEY)
                .and_then(Value::as_str),
            Some("«»")
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_ai_runtime_persists_selected_runtime() {
        let path = temp_config_path("runtime");
        let _ = fs::remove_file(&path);

        save_ai_runtime(&path, AiRuntime::Native).expect("save native");
        assert_eq!(AiRuntime::from_user_settings(&read_root(&path)), AiRuntime::Native);

        save_ai_runtime(&path, AiRuntime::Backend).expect("save backend");
        assert_eq!(
            AiRuntime::from_user_settings(&read_root(&path)),
            AiRuntime::Backend
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_onnx_provider_device_round_trips_and_marks_configured() {
        let path = temp_config_path("onnx_selection");
        let _ = fs::remove_file(&path);

        save_onnx_provider_device(&path, "DmlExecutionProvider", "1").expect("save dml");
        let root = read_root(&path);
        assert_eq!(
            ai_onnx_provider_token_from_user_settings(&root).as_deref(),
            Some("DmlExecutionProvider")
        );
        assert_eq!(
            ai_onnx_device_id_from_user_settings(&root).as_deref(),
            Some("1")
        );
        // The `*_configured` flags mirror the backend's own device.set write.
        assert_eq!(
            root.get("General")
                .and_then(Value::as_object)
                .and_then(|g| g.get(GENERAL_AI_ONNX_PROVIDER_CONFIGURED_KEY))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            root.get("General")
                .and_then(Value::as_object)
                .and_then(|g| g.get(GENERAL_AI_ONNX_DEVICE_ID_CONFIGURED_KEY))
                .and_then(Value::as_bool),
            Some(true)
        );

        save_onnx_provider_device(&path, "CPUExecutionProvider", "0").expect("save cpu");
        let root = read_root(&path);
        assert_eq!(
            ai_onnx_provider_token_from_user_settings(&root).as_deref(),
            Some("CPUExecutionProvider")
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_max_loaded_models_round_trips_as_integer() {
        let path = temp_config_path("max_models");
        let _ = fs::remove_file(&path);

        save_max_loaded_models(&path, 5).expect("save 5");
        assert_eq!(ai_max_loaded_models_from_user_settings(&read_root(&path)), 5);

        save_max_loaded_models(&path, 2).expect("save 2");
        assert_eq!(ai_max_loaded_models_from_user_settings(&read_root(&path)), 2);

        let _ = fs::remove_file(&path);
    }
}

/// Loads the canonical user config and persists only required migrations/defaults.
///
/// The read, the migration/backfill decision and the conditional write run as one
/// transaction under the user-config document lock, so a concurrent
/// full-file update cannot be lost. A document that already holds every default and needs
/// no migration is not rewritten.
///
/// # Errors
/// Fails when the document cannot be read, does not parse (it is then left untouched), or
/// the migrated/backfilled document cannot be written.
pub fn load_user_config() -> Result<JsonConfig> {
    let mut cfg = JsonConfig {
        path: user_config_path(),
        doc: user_config_doc(),
        defaults: user_config_defaults(),
        data: Value::Object(Map::new()),
    };
    ms_docstore::with_lock(&cfg.doc.clone(), |locked| -> Result<()> {
        cfg.data = config_root_or_empty(locked.read_value(), &cfg.path)?;
        let migrated = migrate_missing_memory_profile_from_legacy_cache_pages(&mut cfg.data);
        let defaults_applied = cfg.apply_defaults();
        if migrated || defaults_applied {
            locked
                .write_value(&cfg.data, WriteOptions::default())
                .with_context(|| format!("failed to write config {}", cfg.path.display()))?;
        }
        Ok(())
    })?;
    Ok(cfg)
}

/// Reads the current `user_config.json` exactly as stored (no defaults, no migration, no
/// root coercion, never writes). An absent document reads as `{}`.
///
/// # Errors
/// Fails when the document exists but cannot be read or does not parse.
pub fn load_raw_user_settings_for_startup() -> Result<Value> {
    read_raw_user_config(&user_config_doc())
}

/// Body of [`load_raw_user_settings_for_startup`] for an explicit document (tests use a
/// temp directory).
fn read_raw_user_config(doc: &DocRef) -> Result<Value> {
    let path = doc.path_for(ms_docstore::DocFormat::Json);
    match ms_docstore::read_value(doc) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Ok(Value::Object(Map::new())),
        Err(err) => Err(config_read_error(err, &path)),
    }
}

#[must_use]
pub fn user_settings_has_ai_install_type(user_settings: &Value) -> bool {
    user_settings
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_AI_INSTALL_TYPE_KEY))
        .is_some()
}

/// Whether the raw (unmerged) user settings already carry a working
/// `General.ui_language` choice: a key counts as present only when it holds a string
/// whose trimmed value is non-empty. A `null`, empty, whitespace-only, or non-string
/// value means the user never made a working choice.
///
/// Feeds [`mark_first_run_languages_if_needed`]: an existing install with BOTH
/// language keys already set must not be marked for the first-run modal. The modal
/// itself does NOT gate on this — it gates on the persisted
/// `General.first_run_languages_confirmed` marker
/// ([`user_settings_first_run_languages_pending`]), because the startup path persists
/// the defaults tree (materializing both language keys) before the launcher reads the
/// config, which would otherwise erase the "keys absent" signal.
///
/// Pass the RAW settings from [`load_raw_user_settings_for_startup`]; the merged
/// startup settings backfill the default and would always report `true`.
#[must_use]
pub fn user_settings_has_ui_language(user_settings: &Value) -> bool {
    user_settings
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_UI_LANGUAGE_KEY))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

/// Whether the raw (unmerged) user settings already carry a working
/// `TextTab.text_language` choice. Companion to [`user_settings_has_ui_language`] for
/// the typesetting language: present only when the key holds a non-empty (trimmed)
/// string; a `null`/empty/non-string value means the user never made a working choice.
///
/// Like [`user_settings_has_ui_language`], this feeds
/// [`mark_first_run_languages_if_needed`] (an existing install with both language keys
/// present is not marked) and NOT the modal's gate, which uses the persisted marker.
///
/// Pass the RAW settings from [`load_raw_user_settings_for_startup`] (see
/// [`user_settings_has_ui_language`] for why the merged settings must not be used).
#[must_use]
pub fn user_settings_has_text_language(user_settings: &Value) -> bool {
    user_settings
        .get("TextTab")
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(TEXT_TAB_TEXT_LANGUAGE_KEY))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

/// Whether the launcher must show the first-run language modal, decided purely from
/// the tri-state marker `General.first_run_languages_confirmed`.
///
/// Returns `true` ONLY when the marker is present AND holds the boolean `false`
/// (first run detected, not yet confirmed). A missing marker (feature never triggered
/// or an existing install), a `true` marker (already confirmed), or a non-boolean
/// value all return `false`.
///
/// Pass the RAW settings from [`load_raw_user_settings_for_startup`]; the marker is
/// deliberately absent from the defaults tree, so the merged settings carry the same
/// value.
#[must_use]
pub fn user_settings_first_run_languages_pending(user_settings: &Value) -> bool {
    user_settings
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_FIRST_RUN_LANGUAGES_CONFIRMED_KEY))
        .and_then(Value::as_bool)
        == Some(false)
}

/// Records the first-run language marker when a fresh install is detected, so the
/// launcher can later show the first-run language modal.
///
/// This MUST run before any other startup call that touches [`load_user_config`]:
/// those calls persist the full defaults tree (materializing `General.ui_language`
/// and `TextTab.text_language`), which would destroy the "language keys absent"
/// signal this detection relies on and make every fresh install look like an existing
/// one.
///
/// It reads the RAW settings via [`load_raw_user_settings_for_startup`] and returns
/// early, doing nothing, in three cases:
/// - a read error (logged; startup is never blocked);
/// - the marker key is already present (the first-run decision was already recorded
///   on an earlier launch — `false` pending or `true` confirmed);
/// - both language keys are already set — an existing install that predates this
///   feature, which must never trigger the modal.
///
/// Otherwise it persists `General.first_run_languages_confirmed = false`; persistence
/// errors are logged and swallowed so startup is never blocked.
pub fn mark_first_run_languages_if_needed() {
    let raw = match load_raw_user_settings_for_startup() {
        Ok(raw) => raw,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[startup] failed to read user config before first-run language detection: {err:#}"
            ));
            return;
        }
    };
    // The marker already exists (in any form): the first-run decision was made on an
    // earlier launch. Never re-evaluate or overwrite it.
    let marker_present = raw
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_FIRST_RUN_LANGUAGES_CONFIRMED_KEY))
        .is_some();
    if marker_present {
        return;
    }
    // Both language keys already chosen: an existing install from before this feature.
    // It must never show the modal, so leave the marker absent (key-absent state).
    if user_settings_has_ui_language(&raw) && user_settings_has_text_language(&raw) {
        return;
    }

    runtime_log::log_info(
        "[startup] fresh install detected; marking first-run language selection as pending",
    );
    let mut cfg = match load_user_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[startup] failed to load user config for first-run language marker: {err:#}"
            ));
            return;
        }
    };
    if let Err(err) = cfg.set_path(
        &["General", GENERAL_FIRST_RUN_LANGUAGES_CONFIRMED_KEY],
        Value::Bool(false),
    ) {
        runtime_log::log_warn(format!(
            "[startup] failed to persist first-run language marker: {err:#}"
        ));
    }
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

/// Backfills the global memory profile from the legacy Canvas preference.
///
/// Returns `true` only when the JSON tree was changed, allowing persistent callers
/// to retain an already-complete file byte-for-byte.
fn migrate_missing_memory_profile_from_legacy_cache_pages(data: &mut Value) -> bool {
    let mut changed = false;
    if !data.is_object() {
        *data = Value::Object(Map::new());
        changed = true;
    }
    let Some(root_obj) = data.as_object_mut() else {
        return changed;
    };

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
    let general = root_obj
        .entry("General".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !general.is_object() {
        *general = Value::Object(Map::new());
        changed = true;
    }
    let Some(general_obj) = general.as_object_mut() else {
        return changed;
    };
    if general_obj.contains_key(GENERAL_MEMORY_PROFILE_KEY) {
        return changed;
    }
    general_obj.insert(
        GENERAL_MEMORY_PROFILE_KEY.to_string(),
        Value::String(profile.as_config_str().to_string()),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_config_new_keeps_complete_config_bytes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(PROJECT_SETTINGS_FILE);
        let defaults = json!({"Canvas": {"zoom": 1}, "General": {"enabled": true}});
        // Deliberately compact and differently ordered: semantic completeness must
        // prevent the pretty serializer from normalizing the file on construction.
        let sentinel = r#"{"General":{"enabled":true},"Canvas":{"zoom":1}}"#;
        fs::write(&path, sentinel)?;

        let config = JsonConfig::new(&path, defaults.clone())?;

        assert_eq!(config.data, defaults);
        assert_eq!(fs::read_to_string(path)?, sentinel);
        Ok(())
    }

    #[test]
    fn json_config_new_backfills_missing_defaults_once() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(PROJECT_SETTINGS_FILE);
        let defaults = json!({"Canvas": {"zoom": 1}, "General": {"enabled": true}});
        let incomplete = r#"{"General":{"enabled":true}}"#;
        fs::write(&path, incomplete)?;

        let config = JsonConfig::new(&path, defaults.clone())?;
        let backfilled = fs::read_to_string(&path)?;

        assert_eq!(config.data, defaults);
        assert_ne!(backfilled, incomplete);
        assert_eq!(serde_json::from_str::<Value>(&backfilled)?, defaults);

        let second_load = JsonConfig::new(&path, defaults)?;
        assert_eq!(
            second_load.data,
            serde_json::from_str::<Value>(&backfilled)?
        );
        assert_eq!(fs::read_to_string(path)?, backfilled);
        Ok(())
    }

    #[test]
    fn user_config_writers_and_direct_docstore_updates_lose_no_increment() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(USER_CONFIG_FILE);
        const THREADS: u64 = 6;
        const ROUNDS: u64 = 15;

        fn increment(root: &mut Value) {
            let current = root.get("counter").and_then(Value::as_u64).unwrap_or(0);
            root["counter"] = json!(current + 1);
        }

        let handles: Vec<_> = (0..THREADS)
            .map(|thread_index| {
                let path = path.clone();
                std::thread::spawn(move || -> Result<()> {
                    for _ in 0..ROUNDS {
                        // Half of the writers use this crate's wrapper, half call the
                        // store directly: the shared document lock must serialize both.
                        if thread_index % 2 == 0 {
                            update_user_config_file(&path, |root| {
                                increment(root);
                                Ok(())
                            })?;
                        } else {
                            let doc = DocRef::new(&path, DocKind::UserConfig);
                            ms_docstore::update(&doc, WriteOptions::default(), |root| {
                                increment(root);
                                Ok(())
                            })?;
                        }
                    }
                    Ok(())
                })
            })
            .collect();
        for handle in handles {
            handle.join().map_err(|_| anyhow::anyhow!("writer panicked"))??;
        }

        let saved: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        assert_eq!(saved.get("counter").and_then(Value::as_u64), Some(THREADS * ROUNDS));
        Ok(())
    }

    #[test]
    fn section_writer_rejects_malformed_document_and_leaves_it() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(USER_CONFIG_FILE);
        let corrupt = "not json";
        fs::write(&path, corrupt)?;

        assert!(save_text_language(&path, "en").is_err());
        assert!(save_hanging_punctuation(&path, "«»").is_err());
        assert!(save_ai_runtime(&path, AiRuntime::Native).is_err());

        assert_eq!(fs::read_to_string(&path)?, corrupt);
        Ok(())
    }

    #[test]
    fn section_writer_replaces_document_atomically_in_historical_layout() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(USER_CONFIG_FILE);
        fs::write(&path, "{\"General\":{\"other\":1},\"Canvas\":{\"zoom\":2}}")?;

        save_onnx_build(&path, "cpu").map_err(anyhow::Error::msg)?;

        let expected = json!({"General": {"other": 1, "ai_onnx_build": "cpu"}, "Canvas": {"zoom": 2}});
        // Pretty, no trailing newline: the bytes the std::fs savers always wrote.
        assert_eq!(fs::read_to_string(&path)?, serde_json::to_string_pretty(&expected)?);
        // Atomic replace leaves no sibling temp file behind.
        let names: Vec<_> = fs::read_dir(temp.path())?.map(|entry| entry.map(|entry| entry.file_name())).collect::<std::io::Result<_>>()?;
        assert_eq!(names, vec![std::ffi::OsString::from(USER_CONFIG_FILE)]);
        Ok(())
    }

    #[test]
    fn read_user_config_root_tolerates_absent_and_non_object_but_not_malformed() -> Result<()> {
        use crate::ort_load_guard::read_user_config_root;
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(USER_CONFIG_FILE);

        assert_eq!(read_user_config_root(&path).map_err(anyhow::Error::msg)?, json!({}));
        fs::write(&path, "42")?;
        assert_eq!(read_user_config_root(&path).map_err(anyhow::Error::msg)?, json!({}));
        fs::write(&path, "{\"a\":1}")?;
        assert_eq!(read_user_config_root(&path).map_err(anyhow::Error::msg)?, json!({"a": 1}));
        fs::write(&path, "{broken")?;
        assert!(read_user_config_root(&path).is_err());
        Ok(())
    }

    #[test]
    fn raw_user_config_read_is_verbatim_and_absent_is_empty_object() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(USER_CONFIG_FILE);
        let doc = DocRef::new(&path, DocKind::UserConfig);

        assert_eq!(read_raw_user_config(&doc)?, json!({}));
        // No root coercion and no defaults: the startup read is the document as stored.
        fs::write(&path, "[1]")?;
        assert_eq!(read_raw_user_config(&doc)?, json!([1]));
        fs::write(&path, "{broken")?;
        let err = read_raw_user_config(&doc).expect_err("malformed must fail");
        assert!(format!("{err:#}").contains("failed to parse config"), "{err:#}");
        Ok(())
    }

    #[test]
    fn json_config_kind_follows_file_name_or_explicit_kind() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let settings = JsonConfig::new(temp.path().join(PROJECT_SETTINGS_FILE), json!({}))?;
        assert_eq!(settings.doc().kind(), DocKind::ProjectSettings);
        let user = JsonConfig::new(temp.path().join(USER_CONFIG_FILE), json!({}))?;
        assert_eq!(user.doc().kind(), DocKind::UserConfig);
        let explicit = JsonConfig::with_kind(temp.path().join("other.json"), DocKind::ProjectSettings, json!({}))?;
        assert_eq!(explicit.doc().kind(), DocKind::ProjectSettings);
        // Nothing to backfill: none of the three constructions created a file.
        assert_eq!(fs::read_dir(temp.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn json_config_set_path_rereads_the_document_and_keeps_foreign_keys() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(PROJECT_SETTINGS_FILE);
        let mut config = JsonConfig::new(&path, json!({"Canvas": {"zoom": 1}}))?;
        // Another writer adds a key after this snapshot was taken.
        fs::write(&path, "{\"Canvas\":{\"zoom\":1},\"OCR\":{\"engine\":\"x\"}}")?;

        config.set_path(&["Canvas", "zoom"], json!(3))?;
        config.set("comic_type", json!("ribbon"))?;

        let saved: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        let expected = json!({"Canvas": {"zoom": 3}, "OCR": {"engine": "x"}, "comic_type": "ribbon"});
        assert_eq!(saved, expected);
        assert_eq!(config.data, expected);
        Ok(())
    }

    #[test]
    fn json_config_new_leaves_malformed_document_untouched() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(PROJECT_SETTINGS_FILE);
        fs::write(&path, "{oops")?;

        assert!(JsonConfig::new(&path, json!({"Canvas": {"zoom": 1}})).is_err());
        assert_eq!(fs::read_to_string(&path)?, "{oops");
        Ok(())
    }

    #[test]
    fn document_helpers_name_the_expected_files() {
        let title = Path::new("/titles/one");
        let settings = project_settings_doc(title);
        assert_eq!(settings.kind(), DocKind::ProjectSettings);
        assert_eq!(settings.path_for(ms_docstore::DocFormat::Json), title.join(PROJECT_SETTINGS_FILE));
        let user = user_config_doc();
        assert_eq!(user.kind(), DocKind::UserConfig);
        assert_eq!(user.path_for(ms_docstore::DocFormat::Json), user_config_path());
    }

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
    fn user_settings_has_ui_language_detects_presence() {
        // Absent General object, absent key, and a present non-empty string are the
        // basic cases.
        assert!(!user_settings_has_ui_language(&json!({})));
        assert!(!user_settings_has_ui_language(&json!({"General": {}})));
        assert!(user_settings_has_ui_language(
            &json!({"General": {"ui_language": "ru"}})
        ));
        // A key that holds no working value counts as "not set": null, empty,
        // whitespace-only, or a non-string (numeric) value.
        assert!(!user_settings_has_ui_language(
            &json!({"General": {"ui_language": null}})
        ));
        assert!(!user_settings_has_ui_language(
            &json!({"General": {"ui_language": ""}})
        ));
        assert!(!user_settings_has_ui_language(
            &json!({"General": {"ui_language": "   "}})
        ));
        assert!(!user_settings_has_ui_language(
            &json!({"General": {"ui_language": 42}})
        ));
    }

    #[test]
    fn user_settings_has_text_language_detects_presence() {
        // Absent TextTab object, absent key, and a present non-empty string.
        assert!(!user_settings_has_text_language(&json!({})));
        assert!(!user_settings_has_text_language(&json!({"TextTab": {}})));
        assert!(user_settings_has_text_language(
            &json!({"TextTab": {"text_language": "en"}})
        ));
        // A key that holds no working value counts as "not set": null, empty,
        // whitespace-only, or a non-string (numeric) value.
        assert!(!user_settings_has_text_language(
            &json!({"TextTab": {"text_language": null}})
        ));
        assert!(!user_settings_has_text_language(
            &json!({"TextTab": {"text_language": ""}})
        ));
        assert!(!user_settings_has_text_language(
            &json!({"TextTab": {"text_language": "   "}})
        ));
        assert!(!user_settings_has_text_language(
            &json!({"TextTab": {"text_language": 7}})
        ));
    }

    #[test]
    fn first_run_languages_pending_only_for_false_marker() {
        // Absent marker (feature never triggered / existing install): not pending.
        assert!(!user_settings_first_run_languages_pending(&json!({})));
        assert!(!user_settings_first_run_languages_pending(
            &json!({"General": {}})
        ));
        // `true`: already confirmed — not pending.
        assert!(!user_settings_first_run_languages_pending(
            &json!({"General": {"first_run_languages_confirmed": true}})
        ));
        // `false`: first run detected, awaiting confirmation — the only pending case.
        assert!(user_settings_first_run_languages_pending(
            &json!({"General": {"first_run_languages_confirmed": false}})
        ));
        // A non-boolean value is malformed and never counts as pending.
        assert!(!user_settings_first_run_languages_pending(
            &json!({"General": {"first_run_languages_confirmed": "false"}})
        ));
        assert!(!user_settings_first_run_languages_pending(
            &json!({"General": {"first_run_languages_confirmed": null}})
        ));
    }

    #[test]
    fn user_config_defaults_omit_first_run_marker() {
        // Contract guard: the first-run marker must never be materialized by
        // `apply_defaults`, so it must be absent from the defaults tree. Its absence is
        // the "feature never triggered" state; a default would make every fresh install
        // look already-decided.
        let defaults = user_config_defaults();
        let general = defaults
            .get("General")
            .and_then(Value::as_object)
            .expect("user_config_defaults must contain a General object");
        assert!(
            !general.contains_key(GENERAL_FIRST_RUN_LANGUAGES_CONFIRMED_KEY),
            "General.first_run_languages_confirmed must not be part of the defaults tree"
        );
    }

    #[test]
    fn ui_scale_percent_defaults_and_clamps() {
        // Missing key / missing section / wrong type -> native size.
        assert_eq!(
            ui_scale_percent_from_user_settings(&json!({})),
            UI_SCALE_PERCENT_DEFAULT
        );
        assert_eq!(
            ui_scale_percent_from_user_settings(&json!({"General": {}})),
            UI_SCALE_PERCENT_DEFAULT
        );
        assert_eq!(
            ui_scale_percent_from_user_settings(&json!({"General": {"ui_scale_percent": "90"}})),
            UI_SCALE_PERCENT_DEFAULT
        );
        // A stored value is honored, including a hand-written float.
        assert_eq!(
            ui_scale_percent_from_user_settings(&json!({"General": {"ui_scale_percent": 90}})),
            90
        );
        assert_eq!(
            ui_scale_percent_from_user_settings(&json!({"General": {"ui_scale_percent": 89.6}})),
            90
        );
        // A hand-edited out-of-range value can never make the UI unusable.
        assert_eq!(
            ui_scale_percent_from_user_settings(&json!({"General": {"ui_scale_percent": 5}})),
            UI_SCALE_PERCENT_MIN
        );
        assert_eq!(
            ui_scale_percent_from_user_settings(&json!({"General": {"ui_scale_percent": 5000}})),
            UI_SCALE_PERCENT_MAX
        );
        assert_eq!(
            ui_scale_percent_from_user_settings(&json!({"General": {"ui_scale_percent": -1}})),
            UI_SCALE_PERCENT_MIN
        );
    }

    #[test]
    fn ui_scale_factor_maps_percent_to_zoom_factor() {
        assert!((ui_scale_factor_from_percent(100) - 1.0).abs() < f32::EPSILON);
        assert!((ui_scale_factor_from_percent(90) - 0.9).abs() < 1e-6);
        // Out-of-range input is clamped, never turned into an absurd zoom factor.
        assert!(
            (ui_scale_factor_from_percent(10_000) - ui_scale_factor_from_percent(UI_SCALE_PERCENT_MAX))
                .abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn user_config_defaults_carry_native_ui_scale() {
        let defaults = user_config_defaults();
        assert_eq!(
            ui_scale_percent_from_user_settings(&defaults),
            UI_SCALE_PERCENT_DEFAULT
        );
    }

    #[test]
    fn ai_runtime_resolves_effective_runtime() {
        // Fresh install: empty tree and JSON null both default to native.
        assert_eq!(AiRuntime::from_user_settings(&json!({})), AiRuntime::Native);
        assert_eq!(
            AiRuntime::from_user_settings(&Value::Null),
            AiRuntime::Native
        );
        // Upgrade migration: a pre-flag config's stored "backend" token is ignored.
        assert_eq!(
            AiRuntime::from_user_settings(&json!({"General": {"ai_runtime": "backend"}})),
            AiRuntime::Native
        );
        // Explicit-choice flag false: stored token ignored, native default applies.
        assert_eq!(
            AiRuntime::from_user_settings(
                &json!({"General": {"ai_runtime_configured": false, "ai_runtime": "backend"}})
            ),
            AiRuntime::Native
        );
        // Explicit choice of the Python backend is respected.
        assert_eq!(
            AiRuntime::from_user_settings(
                &json!({"General": {"ai_runtime_configured": true, "ai_runtime": "backend"}})
            ),
            AiRuntime::Backend
        );
        // Explicit choice of the native runtime is respected.
        assert_eq!(
            AiRuntime::from_user_settings(
                &json!({"General": {"ai_runtime_configured": true, "ai_runtime": "native"}})
            ),
            AiRuntime::Native
        );
        // Explicit flag but missing token: documented fallback to native.
        assert_eq!(
            AiRuntime::from_user_settings(&json!({"General": {"ai_runtime_configured": true}})),
            AiRuntime::Native
        );
        // Explicit flag but garbage token: documented fallback to native.
        assert_eq!(
            AiRuntime::from_user_settings(
                &json!({"General": {"ai_runtime_configured": true, "ai_runtime": "onnx"}})
            ),
            AiRuntime::Native
        );
        // Whitespace is trimmed around an explicit token.
        assert_eq!(
            AiRuntime::from_user_settings(
                &json!({"General": {"ai_runtime_configured": true, "ai_runtime": " backend "}})
            ),
            AiRuntime::Backend
        );
    }

    #[test]
    fn ai_runtime_as_key_round_trips() {
        assert_eq!(AiRuntime::Backend.as_key(), "backend");
        assert_eq!(AiRuntime::Native.as_key(), "native");
        // An explicit choice (configured=true) round-trips through the stored token.
        for runtime in [AiRuntime::Backend, AiRuntime::Native] {
            assert_eq!(
                AiRuntime::from_user_settings(&json!({
                    "General": {
                        "ai_runtime_configured": true,
                        "ai_runtime": runtime.as_key(),
                    }
                })),
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
    fn ai_onnx_build_reads_and_filters() {
        // Absent / empty / not-selected -> None (caller applies the per-OS default).
        assert_eq!(ai_onnx_build_from_user_settings(&json!({})), None);
        assert_eq!(
            ai_onnx_build_from_user_settings(&json!({"General": {}})),
            None
        );
        assert_eq!(
            ai_onnx_build_from_user_settings(
                &json!({"General": {"ai_onnx_build": "not-selected"}})
            ),
            None
        );
        assert_eq!(
            ai_onnx_build_from_user_settings(&json!({"General": {"ai_onnx_build": "  "}})),
            None
        );
        // A stored slug is trimmed and returned verbatim.
        assert_eq!(
            ai_onnx_build_from_user_settings(
                &json!({"General": {"ai_onnx_build": " cuda13 "}})
            )
            .as_deref(),
            Some("cuda13")
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

    

    /// The two engines share every widget-id stem and every settings path unless these
    /// four identities really differ, and the 9B half of each is frozen: an installation
    /// that already holds `flux2_klein_settings.json` and a downloaded
    /// `side_models/FLUX.2-klein-9B` must keep reading exactly those.
    #[test]
    fn the_two_variants_are_distinct_and_the_9b_identity_is_frozen() {
        assert_eq!(Flux2Variant::Klein9B.wire(), "9b");
        assert_eq!(Flux2Variant::Klein4B.wire(), "4b");
        assert_eq!(Flux2Variant::Klein9B.engine_id(), "flux2_klein");
        assert_eq!(
            Flux2Variant::Klein9B.settings_file_name(),
            "flux2_klein_settings.json",
            "the historic settings file name is what existing installations hold"
        );
        assert_eq!(Flux2Variant::Klein9B.dir_name(), "FLUX.2-klein-9B");
        assert_eq!(Flux2Variant::Klein4B.dir_name(), "FLUX.2-klein-4B");
        let (a, b) = (Flux2Variant::Klein9B, Flux2Variant::Klein4B);
        assert_ne!(a.wire(), b.wire());
        assert_ne!(a.engine_id(), b.engine_id());
        assert_ne!(a.settings_file_name(), b.settings_file_name());
        assert_ne!(a.dir_name(), b.dir_name());
    }

    /// An absent variant field is what every settings file written before the 4B engine
    /// existed looks like, and those files describe the 9B model.
    #[test]
    fn an_unknown_variant_token_reads_as_9b() {
        assert_eq!(Flux2Variant::from_wire(""), Flux2Variant::Klein9B);
        assert_eq!(Flux2Variant::from_wire("   "), Flux2Variant::Klein9B);
        assert_eq!(Flux2Variant::from_wire("klein-42b"), Flux2Variant::Klein9B);
        assert_eq!(Flux2Variant::from_wire(" 4b "), Flux2Variant::Klein4B);
    }

    /// The uncensored encoder and the access token are the two capabilities the panel
    /// gates a control on, so a wrong answer here draws a control that cannot work.
    #[test]
    fn only_the_9b_variant_has_an_uncensored_encoder_and_a_gated_repository() {
        assert!(Flux2Variant::Klein9B.supports_uncensored_encoder());
        assert!(!Flux2Variant::Klein4B.supports_uncensored_encoder());
        assert!(Flux2Variant::Klein9B.requires_hf_token());
        assert!(!Flux2Variant::Klein4B.requires_hf_token());
    }

    /// Each variant's three model paths must live under ITS OWN directory: a shared one
    /// would make the two engines overwrite each other's download.
    #[test]
    fn the_derived_paths_are_variant_specific() {
        for variant in Flux2Variant::all() {
            let root = flux2_klein_dir(variant);
            assert!(root.ends_with(variant.dir_name()));
            assert!(flux2_klein_transformer_dir(variant).starts_with(&root));
            assert!(flux2_klein_vae_dir(variant).starts_with(&root));
            assert!(flux2_klein_text_encoder_dir(variant, false).starts_with(&root));
        }
        assert_ne!(
            flux2_klein_transformer_dir(Flux2Variant::Klein9B),
            flux2_klein_transformer_dir(Flux2Variant::Klein4B),
        );
        assert_ne!(
            flux2_klein_settings_path(Flux2Variant::Klein9B),
            flux2_klein_settings_path(Flux2Variant::Klein4B),
        );
    }
}
