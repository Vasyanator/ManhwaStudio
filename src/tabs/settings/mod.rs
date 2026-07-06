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
use std::sync::{Arc, Mutex};
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

pub(super) fn save_projects_dir(
    user_settings_file: &Path,
    projects_dir: &str,
) -> Result<(), String> {
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
