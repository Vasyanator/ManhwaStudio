/*
FILE OVERVIEW: src/tabs/settings/mod.rs
Settings tab state and shared runtime for settings subpanes.

Main types:
- `SettingsPane`: active settings subsection (`General`, `AiBackend`, `Hotkeys`).
- `SettingsTabState`: pane state + shared AI backend health snapshot bindings.
- `SettingsTabState`: also owns the user-facing memory profile binding to `MemoryManager`.
- `AiBackendProcessSnapshot`: thread-safe process state for `ai_backend.py` (running/status/logs/autostart).
- `AiBackendProcessRuntime`: background process worker handle + command channel.

Flow:
- `draw`: renders pane switcher and delegates UI to submodules.
- `spawn_ai_backend_process_worker`: starts process manager thread that resolves Python env,
  launches/stops/restarts `ai_backend.py`, reads stdout/stderr asynchronously,
  and persists `Запускать автоматически` in `user_config.json`.
- Backend startup path resolution uses `config::program_dir()` (launch working directory,
  falling back to executable directory).
- Before launch, the manager checks the default backend port and passes `--port` with either the
  default or a free fallback port to `ai_backend.py`.
- Backend manager/status/output lines are mirrored into runtime file logs
  (`last.log` / `previous.log`) via `crate::runtime_log`.
*/

mod ai_backend;
mod canvas_ribbon;
mod general;
mod hotkeys;

use crate::bubble_status::BubbleStatusCondition;
use crate::canvas::{save_canvas_settings_to_project_file, save_canvas_settings_to_user_file};
use crate::config;
use crate::input_manager_v2::InputManagerV2;
use crate::memory_manager::{MemoryManager, MemoryProfile};
use crate::models::bubbles_model::{BubblesModel, SharedCanvasSettings};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::project::{ComicType, save_comic_type_to_project_file};
use crate::python_manager;
use crate::python_manager::ManagedPythonChild;
use crate::runtime_log;
use crate::tabs::translation::backend_health::{
    AI_BACKEND_HOST, AI_BACKEND_PORT, AiBackendHealthSnapshot, AiBackendProbeCommand,
    set_ai_backend_port,
};
use crate::tabs::typing::TypingPanelLayout;
use crate::widgets::{
    current_spellcheck_words_revision, load_custom_spellcheck_words, load_project_spellcheck_words,
    save_custom_spellcheck_words, save_project_spellcheck_words,
    set_project_spellcheck_settings_file,
};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) const AI_BACKEND_LOG_LIMIT: usize = 400;
pub(super) const AI_BACKEND_WORKER_TICK: Duration = Duration::from_millis(150);
pub(super) const AI_BACKEND_AUTOSTART_KEY: &str = "ai_backend_autostart";
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

#[derive(Debug, Clone)]
pub(super) struct AiBackendProcessSnapshot {
    running: bool,
    status: String,
    auto_start: bool,
    updated_at: Option<Instant>,
    logs: VecDeque<String>,
}

impl AiBackendProcessSnapshot {
    pub(super) fn new(auto_start: bool) -> Self {
        Self {
            running: false,
            status: "Процесс не запущен.".to_string(),
            auto_start,
            updated_at: None,
            logs: VecDeque::new(),
        }
    }

    pub(super) fn disabled(auto_start: bool) -> Self {
        Self {
            running: false,
            status: "Управление запуском отключено (--no-ai).".to_string(),
            auto_start,
            updated_at: None,
            logs: VecDeque::new(),
        }
    }
}

#[derive(Debug)]
pub(super) struct AiBackendProcessRuntime {
    pub(super) tx: Sender<AiBackendProcessCommand>,
    pub(super) thread: JoinHandle<()>,
}

#[derive(Debug, Clone)]
pub(super) enum AiBackendProcessCommand {
    Start,
    Stop,
    Restart,
    SetAutoStart(bool),
    Shutdown,
}

#[derive(Debug)]
pub(super) enum AiBackendOutputEvent {
    Stdout(String),
    Stderr(String),
    StreamError(&'static str, String),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum SettingsPane {
    General,
    CanvasRibbon,
    AiBackend,
    Hotkeys,
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
    project_settings_file: PathBuf,
    canvas_settings: SharedCanvasSettings,
    bubbles_model: Option<Arc<Mutex<BubblesModel>>>,
    clean_overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    canvas_settings_runtime: Option<CanvasSettingsRuntime>,
    spellcheck_custom_words: String,
    project_spellcheck_custom_words: String,
    spellcheck_words_revision_seen: u64,
    ai_enabled: bool,
    ai_backend_probe: Arc<Mutex<AiBackendHealthSnapshot>>,
    ai_backend_probe_tx: Option<Sender<AiBackendProbeCommand>>,
    ai_backend_process_snapshot: Arc<Mutex<AiBackendProcessSnapshot>>,
    ai_backend_process_runtime: Option<AiBackendProcessRuntime>,
    selected_backend_device: String,
    selected_onnx_provider: String,
    selected_onnx_device_id: String,
    selected_max_loaded_models: u32,
    requested_initial_device_refresh: bool,
    dragged_bubble_condition_node: Option<DraggedBubbleConditionNode>,
    hotkey_capture_command_id: Option<String>,
}

impl Default for SettingsTabState {
    fn default() -> Self {
        Self::new(
            false,
            Arc::new(Mutex::new(AiBackendHealthSnapshot::default())),
            None,
            Arc::new(MemoryManager::default()),
        )
    }
}

impl SettingsTabState {
    pub fn new(
        ai_enabled: bool,
        ai_backend_probe: Arc<Mutex<AiBackendHealthSnapshot>>,
        ai_backend_probe_tx: Option<Sender<AiBackendProbeCommand>>,
        memory_manager: Arc<MemoryManager>,
    ) -> Self {
        let user_settings_file = config::user_config_path();
        let auto_start = load_ai_backend_autostart(&user_settings_file);
        let typing_panel_layout = load_typing_panel_layout(&user_settings_file);
        let memory_profile = load_memory_profile(&user_settings_file);
        memory_manager.set_profile(memory_profile);
        let projects_dir = load_projects_dir(&user_settings_file);
        let ai_backend_process_snapshot = Arc::new(Mutex::new(if ai_enabled {
            AiBackendProcessSnapshot::new(auto_start)
        } else {
            AiBackendProcessSnapshot::disabled(auto_start)
        }));
        let ai_backend_process_runtime = if ai_enabled {
            Some(spawn_ai_backend_process_worker(
                Arc::clone(&ai_backend_process_snapshot),
                user_settings_file.clone(),
                auto_start,
            ))
        } else {
            None
        };

        Self {
            active_pane: SettingsPane::General,
            user_settings_file,
            typing_panel_layout,
            pending_typing_panel_layout: Some(typing_panel_layout),
            memory_manager,
            memory_profile,
            projects_dir_input: projects_dir.clone(),
            saved_projects_dir: projects_dir,
            project_settings_file: PathBuf::new(),
            canvas_settings: SharedCanvasSettings::default(),
            bubbles_model: None,
            clean_overlays_model: None,
            canvas_settings_runtime: None,
            spellcheck_custom_words: String::new(),
            project_spellcheck_custom_words: String::new(),
            spellcheck_words_revision_seen: current_spellcheck_words_revision(),
            ai_enabled,
            ai_backend_probe,
            ai_backend_probe_tx,
            ai_backend_process_snapshot,
            ai_backend_process_runtime,
            selected_backend_device: String::new(),
            selected_onnx_provider: String::new(),
            selected_onnx_device_id: String::new(),
            selected_max_loaded_models: 0,
            requested_initial_device_refresh: false,
            dragged_bubble_condition_node: None,
            hotkey_capture_command_id: None,
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
        let process_running = self.process_snapshot().running;
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
            let selected = self.active_pane == SettingsPane::AiBackend;
            if ui.selectable_label(selected, "ИИ бэкенд").clicked() {
                self.active_pane = SettingsPane::AiBackend;
            }
            let selected = self.active_pane == SettingsPane::Hotkeys;
            if ui.selectable_label(selected, "Горячие клавиши").clicked() {
                self.active_pane = SettingsPane::Hotkeys;
            }
        });
        ui.separator();

        match self.active_pane {
            SettingsPane::General => self.draw_general(ui),
            SettingsPane::CanvasRibbon => self.draw_canvas_ribbon(ui),
            SettingsPane::AiBackend => self.draw_ai_backend(ui),
            SettingsPane::Hotkeys => self.draw_hotkeys(ui, hotkeys_v2),
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
    fn send_probe_command(&self, command: AiBackendProbeCommand) {
        if let Some(tx) = self.ai_backend_probe_tx.as_ref() {
            let _ = tx.send(command);
        }
    }

    fn send_process_command(&self, command: AiBackendProcessCommand) {
        if let Some(runtime) = self.ai_backend_process_runtime.as_ref() {
            let _ = runtime.tx.send(command);
        }
    }

    fn process_snapshot(&self) -> AiBackendProcessSnapshot {
        match self.ai_backend_process_snapshot.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

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
        if let Some(runtime) = self.ai_backend_process_runtime.take() {
            let _ = runtime.tx.send(AiBackendProcessCommand::Shutdown);
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

pub(super) fn spawn_ai_backend_process_worker(
    snapshot: Arc<Mutex<AiBackendProcessSnapshot>>,
    user_settings_file: PathBuf,
    auto_start: bool,
) -> AiBackendProcessRuntime {
    let (tx, rx) = mpsc::channel::<AiBackendProcessCommand>();
    let thread = thread::spawn(move || {
        let (output_tx, output_rx) = mpsc::channel::<AiBackendOutputEvent>();
        let mut child: Option<ManagedPythonChild> = None;

        if auto_start && let Err(err) = start_ai_backend_process(&mut child, &snapshot, &output_tx)
        {
            update_process_status(
                &snapshot,
                false,
                format!("Не удалось автозапустить backend: {err}"),
            );
            append_process_log(
                &snapshot,
                format!("[manager] Ошибка автозапуска backend: {err}"),
            );
        }

        let mut should_exit = false;
        while !should_exit {
            match rx.recv_timeout(AI_BACKEND_WORKER_TICK) {
                Ok(command) => {
                    should_exit = handle_process_command(
                        command,
                        &mut child,
                        &snapshot,
                        &output_tx,
                        &user_settings_file,
                    );
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => should_exit = true,
            }

            drain_backend_output(&snapshot, &output_rx);
            poll_backend_exit(&snapshot, &mut child);
        }

        stop_ai_backend_process(&mut child, &snapshot, "Приложение закрывается.");
    });

    AiBackendProcessRuntime { tx, thread }
}

pub(super) fn handle_process_command(
    command: AiBackendProcessCommand,
    child: &mut Option<ManagedPythonChild>,
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    output_tx: &Sender<AiBackendOutputEvent>,
    user_settings_file: &Path,
) -> bool {
    match command {
        AiBackendProcessCommand::Start => {
            if let Err(err) = start_ai_backend_process(child, snapshot, output_tx) {
                update_process_status(snapshot, false, format!("Ошибка запуска backend: {err}"));
                append_process_log(snapshot, format!("[manager] Ошибка запуска backend: {err}"));
            }
            false
        }
        AiBackendProcessCommand::Stop => {
            stop_ai_backend_process(child, snapshot, "Остановлено пользователем.");
            false
        }
        AiBackendProcessCommand::Restart => {
            stop_ai_backend_process(child, snapshot, "Перезапуск по запросу пользователя.");
            if let Err(err) = start_ai_backend_process(child, snapshot, output_tx) {
                update_process_status(
                    snapshot,
                    false,
                    format!("Ошибка перезапуска backend: {err}"),
                );
                append_process_log(
                    snapshot,
                    format!("[manager] Ошибка перезапуска backend: {err}"),
                );
            }
            false
        }
        AiBackendProcessCommand::SetAutoStart(enabled) => {
            set_autostart_value(snapshot, enabled);
            match save_ai_backend_autostart(user_settings_file, enabled) {
                Ok(()) => append_process_log(
                    snapshot,
                    format!(
                        "[settings] Автозапуск backend {}",
                        if enabled {
                            "включен"
                        } else {
                            "выключен"
                        }
                    ),
                ),
                Err(err) => append_process_log(
                    snapshot,
                    format!(
                        "[settings] Не удалось сохранить автозапуск backend ({enabled}): {err}"
                    ),
                ),
            }
            false
        }
        AiBackendProcessCommand::Shutdown => true,
    }
}

pub(super) fn start_ai_backend_process(
    child: &mut Option<ManagedPythonChild>,
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    output_tx: &Sender<AiBackendOutputEvent>,
) -> Result<(), String> {
    if let Some(existing) = child.as_mut() {
        match existing.try_wait() {
            Ok(None) => {
                let pid = existing.id();
                update_process_status(snapshot, true, format!("Backend уже запущен (PID {pid})."));
                return Ok(());
            }
            Ok(Some(status)) => {
                append_process_log(
                    snapshot,
                    format!(
                        "[manager] Обнаружен уже завершенный backend: {}",
                        format_exit_code(status.code())
                    ),
                );
                *child = None;
            }
            Err(err) => {
                append_process_log(
                    snapshot,
                    format!("[manager] Ошибка проверки состояния процесса: {err}"),
                );
                *child = None;
            }
        }
    }

    let app_dir = config::program_dir();
    let backend_script = app_dir.join("ai_backend.py");
    if !backend_script.is_file() {
        return Err(format!(
            "в директории приложения '{}' не найден ai_backend.py",
            app_dir.display()
        ));
    }

    let python = python_manager::resolve_python_executable(&app_dir)?;
    let backend_port = reserve_ai_backend_port(snapshot)?;
    set_ai_backend_port(backend_port);
    let mut command = python_manager::build_python_command(&app_dir)?;
    command
        .current_dir(&app_dir)
        .env("PYTHONUNBUFFERED", "1")
        .arg("-u")
        .arg("ai_backend.py")
        .arg("--port")
        .arg(backend_port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    python_manager::apply_windows_no_window(&mut command);

    let mut spawned = python_manager::spawn_kill_with_parent(command).map_err(|err| {
        format!(
            "не удалось запустить backend через '{}' ({})",
            python.display(),
            err
        )
    })?;

    let pid = spawned.id();
    let stdout = spawned.stdout.take();
    let stderr = spawned.stderr.take();
    *child = Some(spawned);

    update_process_status(snapshot, true, format!("Backend запущен (PID {pid})."));
    append_process_log(
        snapshot,
        format!(
            "[manager] Запуск ai_backend.py через '{}' (cwd '{}', pid {pid}, port {backend_port})",
            python.display(),
            app_dir.display()
        ),
    );

    if let Some(stdout) = stdout {
        spawn_backend_output_reader("stdout", stdout, output_tx.clone());
    } else {
        append_process_log(snapshot, "[manager] stdout backend недоступен.".to_string());
    }
    if let Some(stderr) = stderr {
        spawn_backend_output_reader("stderr", stderr, output_tx.clone());
    } else {
        append_process_log(snapshot, "[manager] stderr backend недоступен.".to_string());
    }

    Ok(())
}

fn reserve_ai_backend_port(snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>) -> Result<u16, String> {
    reserve_ai_backend_port_for(AI_BACKEND_HOST, AI_BACKEND_PORT, snapshot)
}

fn reserve_ai_backend_port_for(
    host: &str,
    default_port: u16,
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
) -> Result<u16, String> {
    match TcpListener::bind((host, default_port)) {
        Ok(listener) => {
            drop(listener);
            Ok(default_port)
        }
        Err(default_err) => {
            append_process_log(
                snapshot,
                format!(
                    "[manager] Стандартный порт {default_port} занят или недоступен: {default_err}"
                ),
            );
            let listener = TcpListener::bind((host, 0)).map_err(|err| {
                format!(
                    "стандартный порт {default_port} недоступен ({default_err}); \
                     не удалось найти свободный порт: {err}"
                )
            })?;
            let port = listener
                .local_addr()
                .map_err(|err| format!("не удалось прочитать выбранный свободный порт: {err}"))?
                .port();
            drop(listener);
            append_process_log(
                snapshot,
                format!("[manager] Для AI backend выбран свободный порт {port}."),
            );
            Ok(port)
        }
    }
}

pub(super) fn stop_ai_backend_process(
    child: &mut Option<ManagedPythonChild>,
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    reason: &str,
) {
    let Some(mut running_child) = child.take() else {
        update_process_status(
            snapshot,
            false,
            "Процесс backend уже остановлен.".to_string(),
        );
        return;
    };

    let pid = running_child.id();
    append_process_log(
        snapshot,
        format!("[manager] Остановка backend (pid {pid}): {reason}"),
    );

    if let Err(err) = running_child.kill() {
        append_process_log(
            snapshot,
            format!("[manager] Не удалось отправить kill backend (pid {pid}): {err}"),
        );
    }

    match running_child.wait() {
        Ok(status) => {
            update_process_status(
                snapshot,
                false,
                format!(
                    "Backend остановлен (pid {pid}, {}).",
                    format_exit_code(status.code())
                ),
            );
        }
        Err(err) => {
            update_process_status(
                snapshot,
                false,
                format!("Backend завершился с ошибкой ожидания (pid {pid}): {err}"),
            );
        }
    }
}

pub(super) fn poll_backend_exit(
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    child: &mut Option<ManagedPythonChild>,
) {
    let Some(running_child) = child.as_mut() else {
        return;
    };

    match running_child.try_wait() {
        Ok(None) => {}
        Ok(Some(status)) => {
            let pid = running_child.id();
            update_process_status(
                snapshot,
                false,
                format!(
                    "Backend завершился (pid {pid}, {}).",
                    format_exit_code(status.code())
                ),
            );
            append_process_log(
                snapshot,
                format!(
                    "[manager] Backend завершился сам (pid {pid}, {}).",
                    format_exit_code(status.code())
                ),
            );
            *child = None;
        }
        Err(err) => {
            update_process_status(
                snapshot,
                false,
                format!("Ошибка проверки backend-процесса: {err}"),
            );
            append_process_log(
                snapshot,
                format!("[manager] Ошибка try_wait backend-процесса: {err}"),
            );
            *child = None;
        }
    }
}

pub(super) fn spawn_backend_output_reader<R: std::io::Read + Send + 'static>(
    stream_name: &'static str,
    stream: R,
    tx: Sender<AiBackendOutputEvent>,
) {
    let _ = thread::Builder::new()
        .name(format!("ai-backend-{stream_name}-reader"))
        .spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line_buf = Vec::with_capacity(1024);
            loop {
                line_buf.clear();
                match reader.read_until(b'\n', &mut line_buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        while matches!(line_buf.last(), Some(b'\n' | b'\r')) {
                            line_buf.pop();
                        }
                        let payload = String::from_utf8_lossy(&line_buf).into_owned();
                        let event = if stream_name == "stderr" {
                            AiBackendOutputEvent::Stderr(payload)
                        } else {
                            AiBackendOutputEvent::Stdout(payload)
                        };
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(AiBackendOutputEvent::StreamError(
                            stream_name,
                            err.to_string(),
                        ));
                        break;
                    }
                }
            }
        });
}

pub(super) fn drain_backend_output(
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    output_rx: &Receiver<AiBackendOutputEvent>,
) {
    while let Ok(event) = output_rx.try_recv() {
        match event {
            AiBackendOutputEvent::Stdout(line) => {
                append_process_log(snapshot, format!("[stdout] {line}"));
            }
            AiBackendOutputEvent::Stderr(line) => {
                append_process_log(snapshot, format!("[stderr] {line}"));
            }
            AiBackendOutputEvent::StreamError(stream, err) => {
                append_process_log(snapshot, format!("[manager] Ошибка чтения {stream}: {err}"));
            }
        }
    }
}

pub(super) fn append_process_log(snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>, line: String) {
    let now = Instant::now();
    let file_log_line = line.clone();
    match snapshot.lock() {
        Ok(mut guard) => {
            guard.logs.push_back(line);
            while guard.logs.len() > AI_BACKEND_LOG_LIMIT {
                guard.logs.pop_front();
            }
            guard.updated_at = Some(now);
        }
        Err(mut poisoned) => {
            let guard = poisoned.get_mut();
            guard.logs.push_back(line);
            while guard.logs.len() > AI_BACKEND_LOG_LIMIT {
                guard.logs.pop_front();
            }
            guard.updated_at = Some(now);
        }
    }
    runtime_log::log_ai_backend(file_log_line);
}

pub(super) fn update_process_status(
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    running: bool,
    status: String,
) {
    let now = Instant::now();
    let status_for_file_log = status.clone();
    match snapshot.lock() {
        Ok(mut guard) => {
            guard.running = running;
            guard.status = status;
            guard.updated_at = Some(now);
        }
        Err(mut poisoned) => {
            let guard = poisoned.get_mut();
            guard.running = running;
            guard.status = status;
            guard.updated_at = Some(now);
        }
    }
    runtime_log::log_ai_backend(format!(
        "[status] {status_for_file_log} (running={running})"
    ));
}

pub(super) fn set_autostart_value(
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    auto_start: bool,
) {
    let now = Instant::now();
    match snapshot.lock() {
        Ok(mut guard) => {
            guard.auto_start = auto_start;
            guard.updated_at = Some(now);
        }
        Err(mut poisoned) => {
            let guard = poisoned.get_mut();
            guard.auto_start = auto_start;
            guard.updated_at = Some(now);
        }
    }
}

pub(super) fn format_exit_code(code: Option<i32>) -> String {
    code.map(|value| format!("код выхода {value}"))
        .unwrap_or_else(|| "завершён сигналом".to_string())
}

pub(super) fn load_ai_backend_autostart(user_settings_file: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return true;
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return true;
    };
    payload
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(AI_BACKEND_AUTOSTART_KEY))
        .and_then(Value::as_bool)
        .unwrap_or(true)
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

pub(super) fn save_ai_backend_autostart(
    user_settings_file: &Path,
    enabled: bool,
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
    general_obj.insert(AI_BACKEND_AUTOSTART_KEY.to_string(), Value::Bool(enabled));
    root_obj.insert("General".to_string(), Value::Object(general_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_ai_backend_port_uses_fallback_when_default_is_busy() {
        let occupied = match TcpListener::bind((AI_BACKEND_HOST, 0)) {
            Ok(listener) => listener,
            Err(err) => panic!("failed to bind occupied-port fixture: {err}"),
        };
        let occupied_port = match occupied.local_addr() {
            Ok(addr) => addr.port(),
            Err(err) => panic!("failed to inspect occupied-port fixture: {err}"),
        };
        let snapshot = Arc::new(Mutex::new(AiBackendProcessSnapshot::new(false)));

        let selected = match reserve_ai_backend_port_for(AI_BACKEND_HOST, occupied_port, &snapshot)
        {
            Ok(port) => port,
            Err(err) => panic!("expected fallback port, got error: {err}"),
        };

        assert_ne!(selected, occupied_port);
        assert!(TcpListener::bind((AI_BACKEND_HOST, selected)).is_ok());
    }
}
