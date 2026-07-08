/*
FILE OVERVIEW: src/tabs/settings/mod.rs
Settings tab state and shared runtime for settings subpanes.

Main types:
- `SettingsPane`: active settings subsection (`General`, `CanvasRibbon`, `Typesetting`,
  `AiBackend`, `Hotkeys`).
- `SettingsTabState`: pane state + the shared `AiBackendHandle` it renders the AI
  backend pane against, plus the user-facing memory profile binding to `MemoryManager`.

Flow:
- `draw`: renders pane switcher and delegates UI to submodules.
- The AI backend pane forwards to the shared `crate::ai_backend_panel` widget over
  the app-global supervisor handle; the backend process/probe lifecycle itself lives
  in `crate::ai_backend_supervisor` (owned by `run_main`, not by this tab).
*/

mod ai_backend;
mod canvas_ribbon;
mod general;
mod hotkeys;
mod typesetting;
#[cfg(feature = "tutorial")]
mod tutorials;

use crate::ai_backend_panel::AiBackendPanelState;
use crate::ai_backend_supervisor::AiBackendHandle;
use crate::bubble_status::BubbleStatusCondition;
use crate::canvas::{save_canvas_settings_to_project_file, save_canvas_settings_to_user_file};
use crate::config;
use crate::input_manager_v2::InputManagerV2;
use crate::memory_manager::{MemoryManager, MemoryProfile};
use crate::models::bubbles_model::{BubblesModel, SharedCanvasSettings};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::project::{ComicType, save_comic_type_to_project_file};
use crate::runtime_log;
use crate::tabs::typing::TypingPanelLayout;
use crate::widgets::{
    current_spellcheck_words_revision, load_custom_spellcheck_words, load_project_spellcheck_words,
    save_custom_spellcheck_words, save_project_spellcheck_words,
    set_project_spellcheck_settings_file,
};
use serde_json::{Map, Value};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use ms_thread::{self as thread, JoinHandle};
use web_time::Duration;

pub(super) const GENERAL_TYPING_PANEL_LAYOUT_KEY: &str = "typing_panel_layout";

#[derive(Debug, Clone)]
pub(super) struct DraggedBubbleConditionNode {
    pub(super) rule_id: u64,
    pub(super) path: Vec<usize>,
    pub(super) payload: BubbleStatusCondition,
}

#[derive(Debug)]
pub(super) struct CanvasSettingsRuntime {
    pub(super) tx: Sender<Option<CanvasSettingsSaveRequest>>,
    pub(super) thread: JoinHandle<()>,
}

#[derive(Debug, Clone)]
pub(super) struct CanvasSettingsSaveRequest {
    pub(super) snapshot: SharedCanvasSettings,
    pub(super) comic_type: ComicType,
    pub(super) custom_spellcheck_words: String,
    pub(super) project_spellcheck_words: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum SettingsPane {
    General,
    CanvasRibbon,
    Typesetting,
    AiBackend,
    Hotkeys,
    #[cfg(feature = "tutorial")]
    Tutorials,
}

#[derive(Debug)]
pub struct SettingsTabState {
    active_pane: SettingsPane,
    user_settings_file: PathBuf,
    typing_panel_layout: TypingPanelLayout,
    pending_typing_panel_layout: Option<TypingPanelLayout>,
    memory_manager: Arc<MemoryManager>,
    memory_profile: MemoryProfile,
    projects_dir_input: String,
    saved_projects_dir: String,
    hanging_punctuation_input: String,
    saved_hanging_punctuation: String,
    project_settings_file: PathBuf,
    canvas_settings: SharedCanvasSettings,
    bubbles_model: Option<Arc<Mutex<BubblesModel>>>,
    clean_overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    canvas_settings_runtime: Option<CanvasSettingsRuntime>,
    spellcheck_custom_words: String,
    project_spellcheck_custom_words: String,
    spellcheck_words_revision_seen: u64,
    ai_backend_handle: AiBackendHandle,
    ai_backend_panel: AiBackendPanelState,
    /// Progress model behind the shared "Обучение" pane. Loaded here since the
    /// studio has no tutorial controller yet; resets persist to config and take
    /// effect on the next launcher run (or future studio tutorials). Gated behind
    /// the `tutorial` feature (off by default).
    #[cfg(feature = "tutorial")]
    tutorial_progress: crate::tutorial::TutorialProgressHandle,
    dragged_bubble_condition_node: Option<DraggedBubbleConditionNode>,
    hotkey_capture_command_id: Option<String>,
    /// Editor for per-effect-kind default parameters, shown in the "Тайп" pane.
    /// Self-contained typing-panel widget (double-interface pattern like
    /// `ai_backend_panel`): the effect model stays encapsulated behind this one
    /// public type; it reads/writes the runtime-global effect-defaults store and
    /// persists to `TextTab.effect_defaults` on its own background thread.
    effect_defaults_editor: crate::tabs::typing::EffectDefaultsEditorState,
    /// Editor for the "Настройки шрифтов" block, shown in the "Тайп" pane. Self-contained
    /// typing-panel widget (same double-interface pattern): it loads the font category
    /// lists off-thread, renders each font in its own typeface, and drives the
    /// runtime-global imported-fonts store for system-font import/removal. Settings needs
    /// no access to the private font model.
    font_settings_editor: crate::tabs::typing::FontSettingsEditorState,
}

impl Default for SettingsTabState {
    fn default() -> Self {
        Self::new(AiBackendHandle::disabled(), Arc::new(MemoryManager::default()))
    }
}

impl SettingsTabState {
    pub fn new(ai_backend_handle: AiBackendHandle, memory_manager: Arc<MemoryManager>) -> Self {
        let user_settings_file = config::user_config_path();
        let typing_panel_layout = load_typing_panel_layout(&user_settings_file);
        let memory_profile = load_memory_profile(&user_settings_file);
        memory_manager.set_profile(memory_profile);
        let projects_dir = load_projects_dir(&user_settings_file);
        // Триггерит ленивую загрузку набора из конфига и даёт текущее значение.
        let hanging_punctuation = crate::text_punctuation::hanging_punctuation_string();

        Self {
            active_pane: SettingsPane::General,
            user_settings_file,
            typing_panel_layout,
            pending_typing_panel_layout: Some(typing_panel_layout),
            memory_manager,
            memory_profile,
            projects_dir_input: projects_dir.clone(),
            saved_projects_dir: projects_dir,
            hanging_punctuation_input: hanging_punctuation.clone(),
            saved_hanging_punctuation: hanging_punctuation,
            project_settings_file: PathBuf::new(),
            canvas_settings: SharedCanvasSettings::default(),
            bubbles_model: None,
            clean_overlays_model: None,
            canvas_settings_runtime: None,
            spellcheck_custom_words: String::new(),
            project_spellcheck_custom_words: String::new(),
            spellcheck_words_revision_seen: current_spellcheck_words_revision(),
            ai_backend_handle,
            ai_backend_panel: AiBackendPanelState::default(),
            #[cfg(feature = "tutorial")]
            tutorial_progress: crate::tutorial::shared_progress(),
            dragged_bubble_condition_node: None,
            hotkey_capture_command_id: None,
            effect_defaults_editor: crate::tabs::typing::EffectDefaultsEditorState::new(),
            font_settings_editor: crate::tabs::typing::FontSettingsEditorState::new(),
        }
    }
}

impl SettingsTabState {
    pub fn set_canvas_settings_binding(
        &mut self,
        project_settings_file: PathBuf,
        initial_canvas_settings: SharedCanvasSettings,
        bubbles_model: Arc<Mutex<BubblesModel>>,
        clean_overlays_model: Arc<Mutex<CleanOverlaysModel>>,
    ) {
        if let Some(runtime) = self.canvas_settings_runtime.take() {
            let _ = runtime.tx.send(None);
            let _ = runtime.thread.join();
        }

        self.project_settings_file = project_settings_file.clone();
        self.canvas_settings = initial_canvas_settings;
        set_project_spellcheck_settings_file(Some(project_settings_file.clone()));
        self.spellcheck_custom_words = load_custom_spellcheck_words().unwrap_or_else(|err| {
            runtime_log::log_warn(format!(
                "[settings] failed to load custom spellcheck dictionary: {err}"
            ));
            String::new()
        });
        self.project_spellcheck_custom_words =
            load_project_spellcheck_words(&project_settings_file).unwrap_or_else(|err| {
                runtime_log::log_warn(format!(
                    "[settings] failed to load project spellcheck words '{}': {err}",
                    project_settings_file.display()
                ));
                String::new()
            });
        self.spellcheck_words_revision_seen = current_spellcheck_words_revision();
        self.bubbles_model = Some(bubbles_model);
        self.clean_overlays_model = Some(clean_overlays_model);
        self.apply_memory_profile_to_runtime(self.memory_profile);
        self.canvas_settings_runtime = Some(spawn_canvas_settings_save_worker(
            self.user_settings_file.clone(),
            project_settings_file,
        ));
    }

    pub fn take_typing_panel_layout_request(&mut self) -> Option<TypingPanelLayout> {
        self.pending_typing_panel_layout.take()
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, hotkeys_v2: &mut InputManagerV2) {
        let process_running = self.ai_backend_handle.process_snapshot().running();
        ui.heading("Настройки");
        ui.horizontal_wrapped(|ui| {
            let selected = self.active_pane == SettingsPane::General;
            if ui.selectable_label(selected, "Общие настройки").clicked() {
                self.active_pane = SettingsPane::General;
            }
            let selected = self.active_pane == SettingsPane::CanvasRibbon;
            if ui.selectable_label(selected, "Лента и пузыри").clicked() {
                self.active_pane = SettingsPane::CanvasRibbon;
            }
            let selected = self.active_pane == SettingsPane::Typesetting;
            if ui.selectable_label(selected, "Тайп").clicked() {
                self.active_pane = SettingsPane::Typesetting;
            }
            let selected = self.active_pane == SettingsPane::AiBackend;
            if ui.selectable_label(selected, "ИИ бэкенд").clicked() {
                self.active_pane = SettingsPane::AiBackend;
            }
            let selected = self.active_pane == SettingsPane::Hotkeys;
            if ui.selectable_label(selected, "Горячие клавиши").clicked() {
                self.active_pane = SettingsPane::Hotkeys;
            }
            #[cfg(feature = "tutorial")]
            {
                let selected = self.active_pane == SettingsPane::Tutorials;
                if ui.selectable_label(selected, "Обучение").clicked() {
                    self.active_pane = SettingsPane::Tutorials;
                }
            }
        });
        ui.separator();

        match self.active_pane {
            SettingsPane::General => self.draw_general(ui),
            SettingsPane::CanvasRibbon => self.draw_canvas_ribbon(ui),
            SettingsPane::Typesetting => self.draw_typesetting(ui),
            SettingsPane::AiBackend => self.draw_ai_backend(ui),
            SettingsPane::Hotkeys => self.draw_hotkeys(ui, hotkeys_v2),
            #[cfg(feature = "tutorial")]
            SettingsPane::Tutorials => self.draw_tutorials(ui),
        }

        let repaint_after = if process_running {
            Duration::from_millis(120)
        } else {
            Duration::from_millis(350)
        };
        ui.ctx().request_repaint_after(repaint_after);
    }
}

impl SettingsTabState {
    fn publish_canvas_settings(&self) {
        let comic_type = ComicType::from_canvas_preset_fields(
            &self.canvas_settings.aside_compact_mode,
            self.canvas_settings.separate_pages,
        );

        if let Some(model) = self.bubbles_model.as_ref() {
            match model.lock() {
                Ok(mut guard) => guard.set_canvas_settings(self.canvas_settings.clone()),
                Err(_) => runtime_log::log_warn(
                    "[settings] failed to lock BubblesModel while publishing canvas settings",
                ),
            }
        }

        if let Some(model) = self.clean_overlays_model.as_ref() {
            match model.lock() {
                Ok(mut guard) => guard.set_cache_pages_enabled(self.canvas_settings.cache_pages),
                Err(_) => runtime_log::log_warn(
                    "[settings] failed to lock CleanOverlaysModel while syncing cache_pages",
                ),
            }
        }

        if let Some(runtime) = self.canvas_settings_runtime.as_ref() {
            let _ = runtime.tx.send(Some(CanvasSettingsSaveRequest {
                snapshot: self.canvas_settings.clone(),
                comic_type,
                custom_spellcheck_words: self.spellcheck_custom_words.clone(),
                project_spellcheck_words: self.project_spellcheck_custom_words.clone(),
            }));
        }
    }

    pub fn replace_canvas_settings_from_snapshot(&mut self, snapshot: SharedCanvasSettings) {
        self.canvas_settings = snapshot;
    }

    pub fn persist_canvas_settings(&self) {
        self.publish_canvas_settings();
    }

    pub(super) fn apply_memory_profile_to_runtime(&self, profile: MemoryProfile) {
        self.memory_manager.set_profile(profile);
        if let Some(model) = self.clean_overlays_model.as_ref() {
            match model.lock() {
                Ok(mut guard) => guard.set_memory_profile(profile),
                Err(_) => runtime_log::log_warn(
                    "[settings] failed to lock CleanOverlaysModel while applying memory profile",
                ),
            }
        }
    }

    fn refresh_spellcheck_words_if_needed(&mut self) {
        let current_revision = current_spellcheck_words_revision();
        if current_revision == self.spellcheck_words_revision_seen {
            return;
        }

        self.spellcheck_custom_words = load_custom_spellcheck_words().unwrap_or_else(|err| {
            runtime_log::log_warn(format!(
                "[settings] failed to refresh custom spellcheck dictionary: {err}"
            ));
            String::new()
        });
        self.project_spellcheck_custom_words =
            load_project_spellcheck_words(&self.project_settings_file).unwrap_or_else(|err| {
                runtime_log::log_warn(format!(
                    "[settings] failed to refresh project spellcheck words '{}': {err}",
                    self.project_settings_file.display()
                ));
                String::new()
            });
        self.spellcheck_words_revision_seen = current_revision;
    }
}

impl Drop for SettingsTabState {
    fn drop(&mut self) {
        set_project_spellcheck_settings_file(None);
        if let Some(runtime) = self.canvas_settings_runtime.take() {
            let _ = runtime.tx.send(None);
            let _ = runtime.thread.join();
        }
    }
}

fn spawn_canvas_settings_save_worker(
    user_settings_file: PathBuf,
    project_settings_file: PathBuf,
) -> CanvasSettingsRuntime {
    let (tx, rx) = mpsc::channel::<Option<CanvasSettingsSaveRequest>>();
    let thread = thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let Some(mut latest) = first else {
                break;
            };
            while let Ok(next) = rx.try_recv() {
                let Some(request) = next else {
                    return;
                };
                latest = request;
            }

            if !project_settings_file.as_os_str().is_empty() {
                if let Err(err) =
                    save_canvas_settings_to_project_file(&project_settings_file, &latest.snapshot)
                {
                    runtime_log::log_error(format!(
                        "[settings] failed to persist project canvas settings {}; error={err}",
                        project_settings_file.display()
                    ));
                }

                if let Err(err) =
                    save_comic_type_to_project_file(&project_settings_file, latest.comic_type)
                {
                    runtime_log::log_error(format!(
                        "[settings] failed to persist comic_type='{}' to {}; error={err}",
                        latest.comic_type.as_config_str(),
                        project_settings_file.display()
                    ));
                }
            }

            if let Err(err) =
                save_canvas_settings_to_user_file(&user_settings_file, &latest.snapshot)
            {
                runtime_log::log_error(format!(
                    "[settings] failed to persist user canvas settings {}; error={err}",
                    user_settings_file.display()
                ));
            }

            if let Err(err) = save_custom_spellcheck_words(&latest.custom_spellcheck_words) {
                runtime_log::log_error(format!(
                    "[settings] failed to persist custom spellcheck dictionary; error={err}"
                ));
            }

            if !project_settings_file.as_os_str().is_empty()
                && let Err(err) = save_project_spellcheck_words(
                    &project_settings_file,
                    &latest.project_spellcheck_words,
                )
            {
                runtime_log::log_error(format!(
                    "[settings] failed to persist project spellcheck words '{}'; error={err}",
                    project_settings_file.display()
                ));
            }
        }
    });

    CanvasSettingsRuntime { tx, thread }
}

pub(super) fn load_typing_panel_layout(user_settings_file: &Path) -> TypingPanelLayout {
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return TypingPanelLayout::Vertical;
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return TypingPanelLayout::Vertical;
    };
    payload
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_TYPING_PANEL_LAYOUT_KEY))
        .and_then(Value::as_str)
        .and_then(TypingPanelLayout::from_config_str)
        .unwrap_or(TypingPanelLayout::Vertical)
}

pub(super) fn load_memory_profile(user_settings_file: &Path) -> MemoryProfile {
    let raw = match fs::read_to_string(user_settings_file) {
        Ok(raw) => raw,
        Err(err) if err.kind() == ErrorKind::NotFound => return MemoryProfile::default(),
        Err(err) => {
            runtime_log::log_error(format!(
                "[settings] failed to read memory profile from {}; error={err}",
                user_settings_file.display()
            ));
            return MemoryProfile::default();
        }
    };
    let payload = match serde_json::from_str::<Value>(&raw) {
        Ok(payload) => payload,
        Err(err) => {
            runtime_log::log_error(format!(
                "[settings] failed to parse memory profile config {}; error={err}",
                user_settings_file.display()
            ));
            return MemoryProfile::default();
        }
    };
    payload
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(config::GENERAL_MEMORY_PROFILE_KEY))
        .and_then(Value::as_str)
        .and_then(MemoryProfile::from_config_str)
        .or_else(|| {
            payload
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
        })
        .unwrap_or_default()
}

pub(super) fn load_projects_dir(user_settings_file: &Path) -> String {
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return config::default_projects_root()
            .to_string_lossy()
            .into_owned();
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return config::default_projects_root()
            .to_string_lossy()
            .into_owned();
    };
    config::projects_root_from_user_settings(&payload)
        .to_string_lossy()
        .into_owned()
}

pub(super) fn normalize_projects_dir_value(raw_value: &str) -> String {
    let trimmed = raw_value.trim();
    if trimmed.is_empty() {
        return config::default_projects_root()
            .to_string_lossy()
            .into_owned();
    }
    PathBuf::from(trimmed).to_string_lossy().into_owned()
}

pub(super) fn save_typing_panel_layout(
    user_settings_file: &Path,
    layout: TypingPanelLayout,
) -> Result<(), String> {
    let _write_guard = lock_user_config_write();
    let mut root = if user_settings_file.exists() {
        match fs::read_to_string(user_settings_file) {
            Ok(raw) => {
                serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
            }
            Err(_) => Value::Object(Map::new()),
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("object ensured");
    let mut general_obj = root_obj
        .get("General")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    general_obj.insert(
        GENERAL_TYPING_PANEL_LAYOUT_KEY.to_string(),
        Value::String(layout.as_config_str().to_string()),
    );
    root_obj.insert("General".to_string(), Value::Object(general_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
}

pub(super) fn save_memory_profile(
    user_settings_file: &Path,
    profile: MemoryProfile,
) -> Result<(), String> {
    let _write_guard = lock_user_config_write();
    let mut root = if user_settings_file.exists() {
        match fs::read_to_string(user_settings_file) {
            Ok(raw) => serde_json::from_str::<Value>(&raw).map_err(|err| {
                format!(
                    "Не удалось разобрать {}: {err}",
                    user_settings_file.display()
                )
            })?,
            Err(err) => {
                return Err(format!(
                    "Не удалось прочитать {}: {err}",
                    user_settings_file.display()
                ));
            }
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let Some(root_obj) = root.as_object_mut() else {
        return Err("Не удалось подготовить корень user_config.json.".to_string());
    };
    let mut general_obj = root_obj
        .get("General")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    general_obj.insert(
        config::GENERAL_MEMORY_PROFILE_KEY.to_string(),
        Value::String(profile.as_config_str().to_string()),
    );
    root_obj.insert("General".to_string(), Value::Object(general_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
}

pub(super) fn save_hanging_punctuation(
    user_settings_file: &Path,
    punctuation: &str,
) -> Result<(), String> {
    let _write_guard = lock_user_config_write();
    let mut root = if user_settings_file.exists() {
        match fs::read_to_string(user_settings_file) {
            Ok(raw) => {
                serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
            }
            Err(_) => Value::Object(Map::new()),
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("object ensured");
    let mut text_tab_obj = root_obj
        .get("TextTab")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    text_tab_obj.insert(
        config::TEXT_TAB_HANGING_PUNCTUATION_KEY.to_string(),
        Value::String(punctuation.to_string()),
    );
    root_obj.insert("TextTab".to_string(), Value::Object(text_tab_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
}

pub(super) fn save_rotation_ctrl_wheel_mode(
    user_settings_file: &Path,
    mode: crate::tabs::typing::rotation_ctrl_wheel::RotationCtrlWheelMode,
) -> Result<(), String> {
    let _write_guard = lock_user_config_write();
    let mut root = if user_settings_file.exists() {
        match fs::read_to_string(user_settings_file) {
            Ok(raw) => {
                serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
            }
            Err(_) => Value::Object(Map::new()),
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("object ensured");
    let mut text_tab_obj = root_obj
        .get("TextTab")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    text_tab_obj.insert(
        config::TEXT_TAB_ROTATION_CTRL_WHEEL_MODE_KEY.to_string(),
        Value::String(mode.as_config_str().to_string()),
    );
    root_obj.insert("TextTab".to_string(), Value::Object(text_tab_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
}

/// Persists the selected AI runtime under `General.ai_runtime` in
/// `user_config.json`.
///
/// Re-reads the file fresh, inserts only the `ai_runtime` key, and rewrites the
/// whole file (mirroring [`save_memory_profile`]). No fsync: this is an ordinary
/// preference with no crash-durability requirement. Safe to call from a
/// background thread; the caller must not invoke it on the GUI thread since it
/// does synchronous disk I/O.
// Wired into the AI runtime selector in `ai_backend_panel`.
pub fn save_ai_runtime(
    user_settings_file: &Path,
    runtime: config::AiRuntime,
) -> Result<(), String> {
    let _write_guard = lock_user_config_write();
    let mut root = read_user_config_root(user_settings_file)?;
    let Some(root_obj) = root.as_object_mut() else {
        return Err("Не удалось подготовить корень user_config.json.".to_string());
    };
    let mut general_obj = root_obj
        .get("General")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    general_obj.insert(
        config::GENERAL_AI_RUNTIME_KEY.to_string(),
        Value::String(runtime.as_key().to_string()),
    );
    root_obj.insert("General".to_string(), Value::Object(general_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
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
/// Re-reads the file fresh, inserts only these keys, and rewrites the whole file
/// (mirroring [`save_ai_runtime`]). No fsync: an ordinary preference. Synchronous
/// disk I/O: do not call from the GUI thread.
// Wired into the ONNX provider/device selector in `ai_backend_panel`.
pub fn save_onnx_provider_device(
    user_settings_file: &Path,
    provider_token: &str,
    device_id: &str,
) -> Result<(), String> {
    let _write_guard = lock_user_config_write();
    let mut root = read_user_config_root(user_settings_file)?;
    let Some(root_obj) = root.as_object_mut() else {
        return Err("Не удалось подготовить корень user_config.json.".to_string());
    };
    let mut general_obj = root_obj
        .get("General")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    general_obj.insert(
        config::GENERAL_AI_ONNX_PROVIDER_KEY.to_string(),
        Value::String(provider_token.to_string()),
    );
    general_obj.insert(
        config::GENERAL_AI_ONNX_DEVICE_ID_KEY.to_string(),
        Value::String(device_id.to_string()),
    );
    general_obj.insert(
        config::GENERAL_AI_ONNX_PROVIDER_CONFIGURED_KEY.to_string(),
        Value::Bool(true),
    );
    general_obj.insert(
        config::GENERAL_AI_ONNX_DEVICE_ID_CONFIGURED_KEY.to_string(),
        Value::Bool(true),
    );
    root_obj.insert("General".to_string(), Value::Object(general_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
}

/// Persists the maximum-loaded-models limit under `General.ai_max_loaded_models` in
/// `user_config.json`.
///
/// Stored as a JSON integer (matching the config default and the native LRU reader).
/// The value is clamped to `1..=10` by the caller/UI; the backend picks it up via
/// `device.set` when connected and from config on the next start. Re-reads fresh,
/// inserts only this key, rewrites the whole file. Synchronous disk I/O: do not call
/// from the GUI thread.
// Wired into the model-limit slider in `ai_backend_panel`.
pub fn save_max_loaded_models(
    user_settings_file: &Path,
    max_loaded_models: u32,
) -> Result<(), String> {
    let _write_guard = lock_user_config_write();
    let mut root = read_user_config_root(user_settings_file)?;
    let Some(root_obj) = root.as_object_mut() else {
        return Err("Не удалось подготовить корень user_config.json.".to_string());
    };
    let mut general_obj = root_obj
        .get("General")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    general_obj.insert(
        config::GENERAL_AI_MAX_LOADED_MODELS_KEY.to_string(),
        Value::Number(max_loaded_models.into()),
    );
    root_obj.insert("General".to_string(), Value::Object(general_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
}

/// Marks the ONNX Runtime load for `scope_key` as attempted-but-not-succeeded and
/// flushes the change to durable storage before returning.
///
/// Writes `{attempted:true, succeeded:false}` under
/// `General.ort_load_state[scope_key]` and fsyncs the file. Call this
/// immediately BEFORE touching the onnxruntime library so a subsequent SIGILL
/// leaves the aborted-attempt marker on disk for the next launch to read.
/// Synchronous disk I/O: do not call from the GUI thread.
// Called by the native ONNX Runtime load path in `native_runtime`.
pub fn mark_ort_load_attempted(
    user_settings_file: &Path,
    scope_key: &str,
) -> Result<(), String> {
    write_ort_load_state(
        user_settings_file,
        scope_key,
        Some(config::OrtLoadGuard {
            attempted: true,
            succeeded: false,
        }),
    )
}

/// Marks the ONNX Runtime load for `scope_key` as succeeded and flushes to disk.
///
/// Writes `{attempted:true, succeeded:true}` under
/// `General.ort_load_state[scope_key]` and fsyncs the file (durable for
/// symmetry with [`mark_ort_load_attempted`]). Call this only after the load
/// returns normally. Synchronous disk I/O: do not call from the GUI thread.
// Called by the native ONNX Runtime load path in `native_runtime`.
pub fn mark_ort_load_succeeded(
    user_settings_file: &Path,
    scope_key: &str,
) -> Result<(), String> {
    write_ort_load_state(
        user_settings_file,
        scope_key,
        Some(config::OrtLoadGuard {
            attempted: true,
            succeeded: true,
        }),
    )
}

/// Clears the ONNX Runtime load guard for `scope_key` (used by a future "retry
/// ORT" control) and flushes to disk.
///
/// Removes the `General.ort_load_state[scope_key]` entry so the scope reads as
/// "no attempt recorded" again, then fsyncs. Synchronous disk I/O: do not call
/// from the GUI thread.
// Called by the "Повторить попытку ORT" control + the native graceful-failure reset.
pub fn reset_ort_load_guard(
    user_settings_file: &Path,
    scope_key: &str,
) -> Result<(), String> {
    write_ort_load_state(user_settings_file, scope_key, None)
}

/// Process-wide lock serializing every `user_config.json` read-modify-write in this
/// module.
///
/// All the `save_*` full-file writers and `write_ort_load_state` re-read the file,
/// mutate one key, then truncate-and-rewrite the whole file. Without serialization,
/// two concurrent writers (background threads and GUI-thread savers) can interleave
/// their read/write and lose an update — dropping the just-written SIGILL
/// `attempted:true` marker (weakening the crash guard) or clobbering user settings.
static USER_CONFIG_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Acquires [`USER_CONFIG_WRITE_LOCK`], recovering from poisoning (a prior panic
/// while holding it leaves the `()` payload usable). Hold the returned guard across
/// the whole read-modify-write (and fsync) of `user_config.json`.
fn lock_user_config_write() -> MutexGuard<'static, ()> {
    USER_CONFIG_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Reads `user_config.json` fresh into a JSON object root, tolerating a missing
/// or non-object file by returning an empty object.
///
/// Returns an error only when an existing file cannot be read or parsed, so
/// callers never silently overwrite an unreadable config.
fn read_user_config_root(user_settings_file: &Path) -> Result<Value, String> {
    let mut root = if user_settings_file.exists() {
        match fs::read_to_string(user_settings_file) {
            Ok(raw) => serde_json::from_str::<Value>(&raw).map_err(|err| {
                format!(
                    "Не удалось разобрать {}: {err}",
                    user_settings_file.display()
                )
            })?,
            Err(err) => {
                return Err(format!(
                    "Не удалось прочитать {}: {err}",
                    user_settings_file.display()
                ));
            }
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    Ok(root)
}

/// Read-modify-write of a single `General.ort_load_state` scope entry with an
/// fsync before returning.
///
/// `entry = Some(guard)` upserts `{attempted, succeeded}` for `scope_key`;
/// `entry = None` removes it. The whole file is rewritten fresh (other keys
/// preserved), then fsynced so the change survives a process crash.
fn write_ort_load_state(
    user_settings_file: &Path,
    scope_key: &str,
    entry: Option<config::OrtLoadGuard>,
) -> Result<(), String> {
    let _write_guard = lock_user_config_write();
    // Whether the config file already existed decides if a parent-directory fsync is
    // also needed for durability (a fresh create adds a new directory entry that an
    // in-place overwrite never touches). Captured under the write lock so it reflects
    // the state this serialized write will act on.
    let file_pre_existed = user_settings_file.exists();
    let mut root = read_user_config_root(user_settings_file)?;
    let Some(root_obj) = root.as_object_mut() else {
        return Err("Не удалось подготовить корень user_config.json.".to_string());
    };
    let mut general_obj = root_obj
        .get("General")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut state_obj = general_obj
        .get(config::GENERAL_ORT_LOAD_STATE_KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    match entry {
        Some(guard) => {
            state_obj.insert(
                scope_key.to_string(),
                serde_json::json!({
                    "attempted": guard.attempted,
                    "succeeded": guard.succeeded,
                }),
            );
        }
        None => {
            state_obj.remove(scope_key);
        }
    }
    general_obj.insert(
        config::GENERAL_ORT_LOAD_STATE_KEY.to_string(),
        Value::Object(state_obj),
    );
    root_obj.insert("General".to_string(), Value::Object(general_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, &payload).map_err(|err| err.to_string())?;
    // Durability barrier: this is intentionally the first fsync in the codebase.
    // The onnxruntime library can abort the process with an uncatchable SIGILL on
    // CPUs missing required instructions, and that fault can arrive immediately
    // after this write returns. Without flushing, the page cache may still hold
    // the marker when the process dies, so the next launch would not see the
    // aborted attempt and would re-trigger the same crash. We reopen the file we
    // just overwrote in place and `sync_all()` its data to stable storage.
    let file = fs::OpenOptions::new()
        .write(true)
        .open(user_settings_file)
        .map_err(|err| {
            format!(
                "Не удалось открыть {} для сброса на диск: {err}",
                user_settings_file.display()
            )
        })?;
    file.sync_all().map_err(|err| {
        format!(
            "Не удалось сбросить {} на диск (fsync): {err}",
            user_settings_file.display()
        )
    })?;
    // For an in-place overwrite of a pre-existing file the directory entry is
    // unchanged, so flushing the file contents alone is durable. A FRESH create,
    // however, also adds a new directory entry that only a parent-directory fsync
    // makes durable; without it a crash could lose the whole file (and its marker).
    if !file_pre_existed {
        fsync_parent_dir_best_effort(user_settings_file);
    }
    Ok(())
}

/// Best-effort fsync of `path`'s parent directory so a newly created file's
/// directory entry is durable.
///
/// Only meaningful on Unix, where a directory can be opened and `sync_all()`ed. On
/// Windows the standard library cannot fsync a directory handle, so this is a no-op
/// there; in practice `user_config.json` is created at first launch (well before any
/// ORT marker write), so the fresh-create-then-crash window this closes is Unix-only
/// and rare. Failures are logged, not surfaced: the file contents were already
/// fsynced, and a missing directory-entry flush only risks losing a first-ever
/// create, not corrupting an existing config.
fn fsync_parent_dir_best_effort(path: &Path) {
    #[cfg(unix)]
    {
        let Some(parent) = path.parent() else {
            return;
        };
        // An empty parent means the current directory; skip rather than open "".
        if parent.as_os_str().is_empty() {
            return;
        }
        match fs::File::open(parent) {
            Ok(dir) => {
                if let Err(err) = dir.sync_all() {
                    runtime_log::log_warn(format!(
                        "[settings] parent directory fsync failed for {} ({err}); the config \
                         contents were still fsynced.",
                        parent.display()
                    ));
                }
            }
            Err(err) => runtime_log::log_warn(format!(
                "[settings] could not open parent directory {} for fsync ({err}); the config \
                 contents were still fsynced.",
                parent.display()
            )),
        }
    }
    #[cfg(not(unix))]
    {
        // No portable directory fsync on Windows via std; see the doc comment.
        let _ = path;
    }
}

pub(super) fn save_projects_dir(
    user_settings_file: &Path,
    projects_dir: &str,
) -> Result<(), String> {
    let _write_guard = lock_user_config_write();
    let mut root = if user_settings_file.exists() {
        match fs::read_to_string(user_settings_file) {
            Ok(raw) => {
                serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
            }
            Err(_) => Value::Object(Map::new()),
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("object ensured");
    let mut general_obj = root_obj
        .get("General")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    general_obj.insert(
        config::GENERAL_PROJECTS_DIR_KEY.to_string(),
        Value::String(projects_dir.to_string()),
    );
    root_obj.insert("General".to_string(), Value::Object(general_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
}

#[cfg(test)]
mod ort_guard_tests {
    use super::*;
    use crate::config::{self, AiRuntime, OrtLoadDecision, OrtLoadGuard};

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
    fn ort_load_state_round_trip_attempted_succeeded_reset() {
        let path = temp_config_path("round_trip");
        let _ = fs::remove_file(&path);
        let scope = config::ort_load_scope_key("cpu", None, "1.20.1");

        // Before any write, the scope reads as "no attempt".
        mark_ort_load_attempted(&path, &scope).expect("mark attempted");
        let root = read_root(&path);
        assert_eq!(
            config::read_ort_load_guard(&root, &scope),
            OrtLoadGuard {
                attempted: true,
                succeeded: false
            }
        );
        assert_eq!(
            config::ort_load_decision(config::read_ort_load_guard(&root, &scope)),
            OrtLoadDecision::Suspect
        );

        mark_ort_load_succeeded(&path, &scope).expect("mark succeeded");
        let root = read_root(&path);
        assert_eq!(
            config::read_ort_load_guard(&root, &scope),
            OrtLoadGuard {
                attempted: true,
                succeeded: true
            }
        );
        assert_eq!(
            config::ort_load_decision(config::read_ort_load_guard(&root, &scope)),
            OrtLoadDecision::Safe
        );

        reset_ort_load_guard(&path, &scope).expect("reset guard");
        let root = read_root(&path);
        assert_eq!(
            config::read_ort_load_guard(&root, &scope),
            OrtLoadGuard {
                attempted: false,
                succeeded: false
            }
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn ort_load_state_is_scoped_per_provider() {
        let path = temp_config_path("scoped");
        let _ = fs::remove_file(&path);
        let cpu = config::ort_load_scope_key("cpu", None, "1.20.1");
        let cuda = config::ort_load_scope_key("cuda", Some(0), "1.20.1");

        mark_ort_load_attempted(&path, &cuda).expect("mark cuda attempted");
        let root = read_root(&path);
        // A failed CUDA attempt must not mark the CPU scope as suspect.
        assert_eq!(
            config::read_ort_load_guard(&root, &cpu),
            OrtLoadGuard {
                attempted: false,
                succeeded: false
            }
        );
        assert_eq!(
            config::read_ort_load_guard(&root, &cuda),
            OrtLoadGuard {
                attempted: true,
                succeeded: false
            }
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
            config::ai_onnx_provider_token_from_user_settings(&root).as_deref(),
            Some("DmlExecutionProvider")
        );
        assert_eq!(
            config::ai_onnx_device_id_from_user_settings(&root).as_deref(),
            Some("1")
        );
        // The `*_configured` flags mirror the backend's own device.set write.
        assert_eq!(
            root.get("General")
                .and_then(Value::as_object)
                .and_then(|g| g.get(config::GENERAL_AI_ONNX_PROVIDER_CONFIGURED_KEY))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            root.get("General")
                .and_then(Value::as_object)
                .and_then(|g| g.get(config::GENERAL_AI_ONNX_DEVICE_ID_CONFIGURED_KEY))
                .and_then(Value::as_bool),
            Some(true)
        );

        save_onnx_provider_device(&path, "CPUExecutionProvider", "0").expect("save cpu");
        let root = read_root(&path);
        assert_eq!(
            config::ai_onnx_provider_token_from_user_settings(&root).as_deref(),
            Some("CPUExecutionProvider")
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_max_loaded_models_round_trips_as_integer() {
        let path = temp_config_path("max_models");
        let _ = fs::remove_file(&path);

        save_max_loaded_models(&path, 5).expect("save 5");
        assert_eq!(config::ai_max_loaded_models_from_user_settings(&read_root(&path)), 5);

        save_max_loaded_models(&path, 2).expect("save 2");
        assert_eq!(config::ai_max_loaded_models_from_user_settings(&read_root(&path)), 2);

        let _ = fs::remove_file(&path);
    }
}
