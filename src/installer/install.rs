/*
File: install.rs

Purpose:
Owns the installer UI and installer-specific window flows.

Main responsibilities:
- draw the egui installation, existing-install, and uninstall progress windows;
- collect install target, dependency profile, and PyTorch choices from the user;
- surface actions for a detected existing install, including launching, shortcut creation,
  reinstall, and updating that installed executable;
- start background installer workers and consume their progress events;
- persist the selected AI dependency level into the installed `user_config.json`;
- expose startup service entry points used by `main.rs`.

Notes:
Non-UI installer work lives in `utils.rs` so the same worker helpers can be reused by future
update flows.
*/

use std::env;
#[cfg(target_os = "windows")]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
#[cfg(target_os = "windows")]
use std::time::Instant;
#[cfg(target_os = "windows")]
use std::time::SystemTime;

use crate::config;
use crate::gpu_utils::RuntimeVersion;
#[cfg(target_os = "windows")]
use crate::python_manager;
use eframe::egui;

use super::utils::*;
#[cfg(target_os = "windows")]
pub use super::utils::{
    run_windows_create_start_menu_shortcut_for_install, run_windows_uninstall_from_current_exe,
};

pub(super) const INSTALL_SUBDIR_NAME: &str = "ManhwaStudio";
const TELEGRAM_INVITE_URL: &str = "https://t.me/SelfTranslators";
const DISCORD_INVITE_URL: &str = "https://discord.gg/mZjZszwDbH";
pub(super) const EMBEDDED_APP_ICON_ICO: &[u8] = include_bytes!("../../app_icon.ico");
pub(super) const EMBEDDED_APP_ICON_PNG: &[u8] = include_bytes!("../../app_icon_512.png");

pub enum InstallerOutcome {
    Completed,
    LaunchLauncher(PathBuf),
    ElevatedRelaunchStarted,
    Cancelled,
    Failed(String),
}

#[cfg(target_os = "windows")]
pub enum ExistingInstallAction {
    NoInstallFound,
    ExitCurrentCopy,
    StartInstaller(PathBuf),
    UpdateInstalled(ExternalUpdateTarget),
}

#[cfg(target_os = "windows")]
#[derive(Clone, Debug)]
struct ExistingWindowsInstall {
    install_dir: PathBuf,
    launcher_path: PathBuf,
    source_label: String,
}

#[cfg(target_os = "windows")]
enum ExistingInstallUiState {
    Choice,
    WaitingForReinstall,
    Error,
}

#[cfg(target_os = "windows")]
enum ExistingInstallEvent {
    ReinstallFinished(Result<PathBuf, String>),
}

#[cfg(target_os = "windows")]
struct ExistingInstallApp {
    install: ExistingWindowsInstall,
    state: ExistingInstallUiState,
    status_text: String,
    error_text: Option<String>,
    rx: Option<mpsc::Receiver<ExistingInstallEvent>>,
    result_sink: Arc<Mutex<Option<ExistingInstallAction>>>,
}

pub fn run_python_installer_window(
    root_dir: &Path,
    auto_install_target: Option<PathBuf>,
) -> Result<InstallerOutcome, String> {
    let shared_result = Arc::new(Mutex::new(None::<InstallerOutcome>));
    let shared_result_for_app = Arc::clone(&shared_result);
    let root_dir = root_dir.to_path_buf();
    let auto_install_target = auto_install_target.clone();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([700.0, 470.0])
        .with_min_inner_size([620.0, 380.0]);
    if let Some(icon) = load_embedded_icon_data() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Установщик ManhwaStudio",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(InstallerApp::new(
                root_dir.clone(),
                Arc::clone(&shared_result_for_app),
                auto_install_target.clone(),
            )))
        }),
    )
    .map_err(|e| e.to_string())?;

    let mut guard = shared_result
        .lock()
        .map_err(|_| "не удалось получить результат установки".to_string())?;
    Ok(guard.take().unwrap_or(InstallerOutcome::Cancelled))
}

#[cfg(target_os = "windows")]
pub fn handle_existing_windows_install(
    current_root: &Path,
) -> Result<ExistingInstallAction, String> {
    let Some(existing_install) = find_existing_windows_install(current_root)? else {
        return Ok(ExistingInstallAction::NoInstallFound);
    };
    run_existing_windows_install_window(existing_install)
}

#[cfg(target_os = "windows")]
fn run_existing_windows_install_window(
    existing_install: ExistingWindowsInstall,
) -> Result<ExistingInstallAction, String> {
    let result_sink = Arc::new(Mutex::new(None::<ExistingInstallAction>));
    let result_sink_for_app = Arc::clone(&result_sink);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([430.0, 520.0])
        .with_min_inner_size([400.0, 520.0])
        .with_max_inner_size([520.0, 620.0])
        .with_resizable(false);
    if let Some(icon) = load_embedded_icon_data() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "ManhwaStudio",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(ExistingInstallApp::new(
                existing_install.clone(),
                result_sink_for_app,
            )))
        }),
    )
    .map_err(|e| e.to_string())?;

    let mut guard = result_sink
        .lock()
        .map_err(|_| "не удалось получить результат окна установленной копии".to_string())?;
    Ok(guard
        .take()
        .unwrap_or(ExistingInstallAction::ExitCurrentCopy))
}

#[cfg(target_os = "windows")]
fn run_existing_install_reinstall_worker(
    install: &ExistingWindowsInstall,
) -> Result<PathBuf, String> {
    let signal_file = build_uninstall_signal_file_path();
    remove_path_if_exists(&signal_file)?;

    let mut cmd = Command::new(&install.launcher_path);
    apply_windows_no_window(&mut cmd);
    cmd.current_dir(&install.install_dir)
        .arg("--uninstall")
        .arg("--uninstall-signal-file")
        .arg(&signal_file);
    cmd.spawn().map_err(|e| {
        format!(
            "не удалось запустить удаление установленной копии '{}': {e}",
            install.launcher_path.display()
        )
    })?;

    let started_at = Instant::now();
    let timeout = Duration::from_secs(60 * 30);
    loop {
        if signal_file.is_file() {
            let signal_text = fs::read_to_string(&signal_file).unwrap_or_default();
            let _ = fs::remove_file(&signal_file);
            let trimmed = signal_text.trim();
            if trimmed.is_empty() || trimmed == "ok" {
                return Ok(install.install_dir.clone());
            }
            if let Some(error_text) = trimmed.strip_prefix("error:") {
                return Err(error_text.trim().to_string());
            }
            return Err(format!("неожиданный сигнал завершения удаления: {trimmed}"));
        }

        if started_at.elapsed() > timeout {
            return Err("удаление установленной копии не завершилось за 30 минут".to_string());
        }

        std::thread::sleep(Duration::from_millis(300));
    }
}

#[cfg(target_os = "windows")]
fn build_uninstall_signal_file_path() -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    env::temp_dir().join(format!(
        "manhwastudio_uninstall_signal_{}_{}.txt",
        std::process::id(),
        suffix
    ))
}

#[cfg(target_os = "windows")]
fn find_existing_windows_install(
    current_root: &Path,
) -> Result<Option<ExistingWindowsInstall>, String> {
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();

    if let Some(all_users) = query_registry_install_dir("HKLM")? {
        candidates.push((all_users, "реестр HKLM".to_string()));
    }
    if let Some(current_user) = query_registry_install_dir("HKCU")? {
        candidates.push((current_user, "реестр HKCU".to_string()));
    }
    if let Some(app_path_dir) = query_registry_app_path_install_dir("HKLM")? {
        candidates.push((app_path_dir, "App Paths HKLM".to_string()));
    }
    if let Some(app_path_dir) = query_registry_app_path_install_dir("HKCU")? {
        candidates.push((app_path_dir, "App Paths HKCU".to_string()));
    }
    if let Ok(local_default) = default_local_install_dir() {
        candidates.push((local_default, "стандартный путь пользователя".to_string()));
    }
    if let Ok(all_users_default) = default_all_users_install_dir() {
        candidates.push((all_users_default, "стандартный путь для всех".to_string()));
    }

    let current_root_normalized = normalize_windows_path(current_root);
    let mut seen = std::collections::HashSet::new();
    for (candidate_dir, source_label) in candidates {
        let normalized = normalize_windows_path(&candidate_dir);
        if normalized == current_root_normalized || !seen.insert(normalized) {
            continue;
        }
        if !python_manager::has_supported_python_env(&candidate_dir) {
            continue;
        }
        let launcher_path = match resolve_windows_launcher_target(&candidate_dir) {
            Ok(path) => path,
            Err(_) => continue,
        };
        return Ok(Some(ExistingWindowsInstall {
            install_dir: candidate_dir,
            launcher_path,
            source_label,
        }));
    }

    Ok(None)
}

#[cfg(target_os = "windows")]
fn query_registry_install_dir(registry_root: &str) -> Result<Option<PathBuf>, String> {
    let key = format!(
        r"{registry_root}\Software\Microsoft\Windows\CurrentVersion\Uninstall\ManhwaStudio"
    );
    let value = reg_query_string_value(&key, Some("InstallLocation"))?;
    Ok(value
        .filter(|item| !item.trim().is_empty())
        .map(PathBuf::from))
}

#[cfg(target_os = "windows")]
fn query_registry_app_path_install_dir(registry_root: &str) -> Result<Option<PathBuf>, String> {
    let key = format!(
        r"{registry_root}\Software\Microsoft\Windows\CurrentVersion\App Paths\manhwastudio_rs.exe"
    );
    let value = reg_query_string_value(&key, Some("Path"))?;
    Ok(value
        .filter(|item| !item.trim().is_empty())
        .map(PathBuf::from))
}

pub fn spawn_installed_program_copy(install_dir: &Path) -> Result<PathBuf, String> {
    let target_exe = resolve_installed_program_copy_path(install_dir)?;
    let mut cmd = Command::new(&target_exe);
    cmd.current_dir(install_dir);
    apply_windows_no_window(&mut cmd);
    cmd.spawn().map_err(|e| {
        format!(
            "не удалось запустить установленную копию '{}' из '{}': {e}",
            target_exe.display(),
            install_dir.display()
        )
    })?;
    Ok(target_exe)
}

pub(crate) fn resolve_installed_program_copy_path(install_dir: &Path) -> Result<PathBuf, String> {
    let current_exe_name = env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|name| name.to_os_string()));
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(name) = current_exe_name {
        candidates.push(install_dir.join(name));
    }
    #[cfg(target_os = "windows")]
    {
        candidates.push(install_dir.join("manhwastudio_rs.exe"));
    }
    #[cfg(not(target_os = "windows"))]
    {
        candidates.push(install_dir.join("manhwastudio_rs"));
    }

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }

    let listed = candidates
        .iter()
        .map(|p| format!("'{}'", p.display()))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "не найдена установленная копия программы в '{}'; проверены: {}",
        install_dir.display(),
        listed
    ))
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum TorchBackend {
    Cuda,
    Rocm,
}

#[derive(Clone, Debug)]
pub(crate) struct TorchWheelOption {
    pub(crate) backend: TorchBackend,
    pub(crate) wheel_tag: String,
    pub(crate) label: String,
    pub(crate) version: RuntimeVersion,
}

#[derive(Clone, Debug)]
pub(crate) struct TorchChoicePrompt {
    pub(crate) options: Vec<TorchWheelOption>,
    pub(crate) recommended_index: usize,
    pub(crate) summary: String,
}

#[derive(Debug)]
pub(crate) enum TorchPreflightResult {
    Skip { reason: String },
    Choose(TorchChoicePrompt),
}

#[derive(Clone, Debug)]
pub(crate) enum TorchInstallSelection {
    SkipCpu,
    InstallGpu(TorchWheelOption),
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub(super) enum InstallDependencyProfile {
    Fast,
    Full,
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum InstallLocationChoice {
    Local,
    AllUsers,
    Custom,
}

#[cfg(target_os = "windows")]
impl ExistingInstallApp {
    fn new(
        install: ExistingWindowsInstall,
        result_sink: Arc<Mutex<Option<ExistingInstallAction>>>,
    ) -> Self {
        let status_text = format!(
            "Найдена установленная копия: {} ({})",
            install.install_dir.display(),
            install.source_label
        );
        Self {
            install,
            state: ExistingInstallUiState::Choice,
            status_text,
            error_text: None,
            rx: None,
            result_sink,
        }
    }

    fn set_result(&self, result: ExistingInstallAction) {
        if let Ok(mut guard) = self.result_sink.lock() {
            *guard = Some(result);
        }
    }

    fn create_desktop_shortcut(&mut self) -> Result<(), String> {
        create_windows_desktop_shortcut(&self.install.install_dir)?;
        self.set_result(ExistingInstallAction::ExitCurrentCopy);
        Ok(())
    }

    fn create_start_menu_shortcut(&mut self) -> Result<(), String> {
        run_windows_create_start_menu_shortcut_for_install(&self.install.install_dir, false)?;
        self.set_result(ExistingInstallAction::ExitCurrentCopy);
        Ok(())
    }

    fn launch_installed_copy(&mut self) -> Result<(), String> {
        spawn_installed_program_copy(&self.install.install_dir)?;
        self.set_result(ExistingInstallAction::ExitCurrentCopy);
        Ok(())
    }

    fn start_reinstall(&mut self) {
        let install = self.install.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.state = ExistingInstallUiState::WaitingForReinstall;
        self.error_text = None;
        self.status_text = format!(
            "Удаляем установленную копию из '{}', затем вернёмся в режим установки...",
            install.install_dir.display()
        );
        let _ = std::thread::Builder::new()
            .name("existing-install-reinstall".to_string())
            .spawn(move || {
                let result = run_existing_install_reinstall_worker(&install);
                let _ = tx.send(ExistingInstallEvent::ReinstallFinished(result));
            });
    }
}

#[cfg(target_os = "windows")]
impl eframe::App for ExistingInstallApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut queued_events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(event) = rx.try_recv() {
                queued_events.push(event);
            }
        }

        for event in queued_events {
            match event {
                ExistingInstallEvent::ReinstallFinished(Ok(target_dir)) => {
                    self.set_result(ExistingInstallAction::StartInstaller(target_dir));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ExistingInstallEvent::ReinstallFinished(Err(err)) => {
                    self.state = ExistingInstallUiState::Error;
                    self.error_text = Some(err.clone());
                    self.status_text = "Переустановка не была подготовлена".to_string();
                }
            }
        }

        let mut close_window = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("Программа уже установлена. Чем-то помочь?");
            });
            ui.add_space(10.0);
            ui.small(format!(
                "Установленная копия: {}",
                self.install.install_dir.display()
            ));
            ui.small(format!("Источник: {}", self.install.source_label));
            ui.add_space(8.0);
            ui.label(&self.status_text);
            if let Some(error_text) = &self.error_text {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), error_text);
            }
            ui.add_space(12.0);

            match self.state {
                ExistingInstallUiState::Choice | ExistingInstallUiState::Error => {
                    if ui
                        .add_sized([280.0, 34.0], egui::Button::new("Запустить установленную"))
                        .clicked()
                    {
                        match self.launch_installed_copy() {
                            Ok(()) => close_window = true,
                            Err(err) => {
                                self.state = ExistingInstallUiState::Error;
                                self.error_text = Some(err);
                            }
                        }
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_sized(
                            [280.0, 34.0],
                            egui::Button::new("Создать ярлык на рабочем столе"),
                        )
                        .clicked()
                    {
                        match self.create_desktop_shortcut() {
                            Ok(()) => close_window = true,
                            Err(err) => {
                                self.state = ExistingInstallUiState::Error;
                                self.error_text = Some(err);
                            }
                        }
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_sized(
                            [280.0, 34.0],
                            egui::Button::new("Создать ярлык в меню пуск"),
                        )
                        .clicked()
                    {
                        match self.create_start_menu_shortcut() {
                            Ok(()) => close_window = true,
                            Err(err) => {
                                self.state = ExistingInstallUiState::Error;
                                self.error_text = Some(err);
                            }
                        }
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_sized([280.0, 34.0], egui::Button::new("Обновить установленную"))
                        .clicked()
                    {
                        self.set_result(ExistingInstallAction::UpdateInstalled(
                            ExternalUpdateTarget {
                                root_dir: self.install.install_dir.clone(),
                                executable_path: self.install.launcher_path.clone(),
                            },
                        ));
                        close_window = true;
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_sized([280.0, 34.0], egui::Button::new("Переустановить"))
                        .clicked()
                    {
                        self.start_reinstall();
                    }
                }
                ExistingInstallUiState::WaitingForReinstall => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Ожидаем завершения удаления установленной копии...");
                    });
                }
            }
        });

        if close_window {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

struct InstallerApp {
    root_dir: PathBuf,
    install_location_choice: InstallLocationChoice,
    custom_install_base_dir_input: String,
    install_target_dir: Option<PathBuf>,
    launcher_exe_path: Option<PathBuf>,
    #[cfg(target_os = "windows")]
    create_windows_desktop_shortcut: bool,
    #[cfg(target_os = "windows")]
    create_windows_start_menu_shortcut: bool,
    state: UiState,
    current_operation: String,
    stage_progress: f32,
    stage_label: String,
    overall_progress: f32,
    overall_label: String,
    console_lines: Vec<String>,
    rx: Option<mpsc::Receiver<InstallEvent>>,
    result_sink: Arc<Mutex<Option<InstallerOutcome>>>,
    torch_choice_prompt: Option<TorchChoicePrompt>,
    pending_ai_install_type: config::AiInstallType,
    invite_telegram: bool,
    invite_discord: bool,
}

impl InstallerApp {
    fn new(
        root_dir: PathBuf,
        result_sink: Arc<Mutex<Option<InstallerOutcome>>>,
        auto_install_target: Option<PathBuf>,
    ) -> Self {
        let custom_install_base_dir_input = root_dir.to_string_lossy().to_string();
        let mut app = Self {
            root_dir,
            install_location_choice: InstallLocationChoice::AllUsers,
            custom_install_base_dir_input,
            install_target_dir: None,
            launcher_exe_path: env::current_exe().ok(),
            #[cfg(target_os = "windows")]
            create_windows_desktop_shortcut: true,
            #[cfg(target_os = "windows")]
            create_windows_start_menu_shortcut: true,
            state: UiState::Idle,
            current_operation: "Ожидание запуска".to_string(),
            stage_progress: 0.0,
            stage_label: "Этап не запущен".to_string(),
            overall_progress: 0.0,
            overall_label: "Инициализация".to_string(),
            console_lines: Vec::new(),
            rx: None,
            result_sink,
            torch_choice_prompt: None,
            pending_ai_install_type: config::AiInstallType::None,
            invite_telegram: true,
            invite_discord: false,
        };
        if let Some(target_dir) = auto_install_target {
            app.apply_auto_install_target(target_dir);
        }
        app
    }

    fn apply_auto_install_target(&mut self, target_dir: PathBuf) {
        self.install_target_dir = Some(target_dir.clone());
        self.install_location_choice = match (
            default_local_install_dir().ok(),
            default_all_users_install_dir().ok(),
        ) {
            (_, Some(all_users)) if all_users == target_dir => InstallLocationChoice::AllUsers,
            (Some(local), _) if local == target_dir => InstallLocationChoice::Local,
            _ => InstallLocationChoice::Custom,
        };
        if self.install_location_choice == InstallLocationChoice::Custom {
            let custom_base = target_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| target_dir.clone());
            self.custom_install_base_dir_input = custom_base.to_string_lossy().to_string();
        }
        self.console_lines.push(format!(
            "[UAC] Продолжение установки в '{}'",
            target_dir.display()
        ));
        self.show_dependency_profile_choice(target_dir);
    }

    fn resolved_install_target_dir(&self) -> Result<PathBuf, String> {
        match self.install_location_choice {
            InstallLocationChoice::Local => default_local_install_dir(),
            InstallLocationChoice::AllUsers => default_all_users_install_dir(),
            InstallLocationChoice::Custom => {
                let raw = self.custom_install_base_dir_input.trim();
                if raw.is_empty() {
                    return Err("папка установки не указана".to_string());
                }
                let base = PathBuf::from(raw);
                if base.is_file() {
                    return Err(format!(
                        "указанный путь '{}' является файлом, а не папкой",
                        base.display()
                    ));
                }
                Ok(base.join(INSTALL_SUBDIR_NAME))
            }
        }
    }

    fn install_requires_elevation(&self, target_dir: &Path) -> bool {
        if is_running_elevated() {
            return false;
        }
        if self.install_location_choice == InstallLocationChoice::AllUsers {
            return true;
        }
        !has_write_access_for_install(target_dir)
    }

    fn maybe_create_windows_shortcuts(&mut self) {
        #[cfg(target_os = "windows")]
        {
            if let Some(target_dir) = self.install_target_dir.clone() {
                let mut created_paths = Vec::new();

                if self.create_windows_desktop_shortcut {
                    match create_windows_desktop_shortcut(&target_dir) {
                        Ok(path) => created_paths.push(format!("Desktop: {}", path.display())),
                        Err(err) => {
                            self.console_lines.push(format!("[Shortcut/Desktop] {err}"));
                        }
                    }
                }

                if self.create_windows_start_menu_shortcut
                    && !is_windows_all_users_install_dir(&target_dir)
                {
                    match create_windows_start_menu_shortcut(&target_dir) {
                        Ok(path) => created_paths.push(format!("Start Menu: {}", path.display())),
                        Err(err) => {
                            self.console_lines
                                .push(format!("[Shortcut/StartMenu] {err}"));
                        }
                    }
                }

                if !created_paths.is_empty() {
                    self.current_operation =
                        format!("Ярлыки созданы: {}", created_paths.join(" | "));
                } else if self.create_windows_desktop_shortcut
                    || self.create_windows_start_menu_shortcut
                {
                    self.current_operation = "Ярлыки не созданы".to_string();
                } else {
                    self.current_operation = "Создание ярлыков пропущено".to_string();
                }
                if self.create_windows_start_menu_shortcut
                    && is_windows_all_users_install_dir(&target_dir)
                {
                    self.console_lines.push(
                        "[Shortcut/StartMenu] Ярлык меню Пуск уже создан в all-users post-install."
                            .to_string(),
                    );
                    if created_paths.is_empty() && !self.create_windows_desktop_shortcut {
                        self.current_operation = "Ярлык меню Пуск уже создан".to_string();
                    }
                }
            }
        }
    }

    fn start_torch_preflight(&mut self, install_target_dir: PathBuf) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.state = UiState::PreparingTorchChoice;
        self.install_target_dir = Some(install_target_dir.clone());
        self.current_operation = "Проверка GPU / CUDA / ROCm...".to_string();
        self.stage_progress = 0.0;
        self.stage_label = "Подготовка этапа PyTorch".to_string();
        self.overall_progress = 0.0;
        self.overall_label = format!("Целевая папка: {}", install_target_dir.display());
        self.torch_choice_prompt = None;

        let _ = std::thread::Builder::new()
            .name("mini-launcher-torch-preflight".to_string())
            .spawn(move || {
                let result = detect_torch_preflight();
                let _ = tx.send(InstallEvent::TorchPreflightReady(result));
            });
    }

    fn show_dependency_profile_choice(&mut self, install_target_dir: PathBuf) {
        self.rx = None;
        self.state = UiState::DependencyProfileChoice;
        self.install_target_dir = Some(install_target_dir.clone());
        self.current_operation = "Выбор режима установки".to_string();
        self.stage_progress = 0.0;
        self.stage_label = "Выберите быстрый или полный набор зависимостей".to_string();
        self.overall_progress = 0.0;
        self.overall_label = format!("Целевая папка: {}", install_target_dir.display());
        self.torch_choice_prompt = None;
    }

    fn start_install(
        &mut self,
        install_target_dir: PathBuf,
        dependency_profile: InstallDependencyProfile,
        torch_selection: TorchInstallSelection,
    ) {
        let (tx, rx) = mpsc::channel();
        let root_dir = install_target_dir.clone();
        let launcher_exe_path = self.launcher_exe_path.clone();
        self.rx = Some(rx);
        self.state = UiState::Running;
        self.install_target_dir = Some(install_target_dir.clone());
        self.current_operation = "Инициализация".to_string();
        self.stage_progress = 0.0;
        self.stage_label = "Подготовка".to_string();
        self.overall_progress = 0.0;
        self.overall_label = format!("Установка в {}", install_target_dir.display());
        self.console_lines.clear();
        self.torch_choice_prompt = None;
        self.pending_ai_install_type = match dependency_profile {
            InstallDependencyProfile::Fast => config::AiInstallType::Base,
            InstallDependencyProfile::Full => config::AiInstallType::Full,
        };

        let _ = std::thread::Builder::new()
            .name("mini-launcher-python-installer".to_string())
            .spawn(move || {
                let result = run_install_worker(
                    root_dir,
                    launcher_exe_path,
                    dependency_profile,
                    torch_selection,
                    &tx,
                );
                let _ = tx.send(InstallEvent::Finished(result));
            });
    }

    fn set_result(&self, outcome: InstallerOutcome) {
        if let Ok(mut guard) = self.result_sink.lock() {
            *guard = Some(outcome);
        }
    }
}

impl eframe::App for InstallerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut queued_events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(event) = rx.try_recv() {
                queued_events.push(event);
            }
        }

        for event in queued_events {
            match event {
                InstallEvent::Step(text) => {
                    self.current_operation = text;
                }
                InstallEvent::ConsoleLine(line) => {
                    self.console_lines.push(line);
                    if self.console_lines.len() > 2000 {
                        self.console_lines.drain(0..200);
                    }
                }
                InstallEvent::Progress {
                    stage_value,
                    stage_label,
                    overall_value,
                    overall_label,
                } => {
                    self.stage_progress = stage_value.clamp(0.0, 1.0);
                    self.stage_label = stage_label;
                    self.overall_progress = overall_value.clamp(0.0, 1.0);
                    self.overall_label = overall_label;
                }
                InstallEvent::TorchPreflightReady(result) => match result {
                    TorchPreflightResult::Skip { reason } => {
                        self.console_lines.push(format!("[PyTorch] {reason}"));
                        if let Some(target_dir) = self.install_target_dir.clone() {
                            self.start_install(
                                target_dir,
                                InstallDependencyProfile::Full,
                                TorchInstallSelection::SkipCpu,
                            );
                        } else {
                            self.state = UiState::Failed;
                            self.current_operation = "Ошибка установки".to_string();
                            self.stage_label = "Этап завершился ошибкой".to_string();
                            self.overall_label = "не выбрана целевая папка установки".to_string();
                            self.set_result(InstallerOutcome::Failed(
                                "не выбрана целевая папка установки".to_string(),
                            ));
                        }
                    }
                    TorchPreflightResult::Choose(prompt) => {
                        self.state = UiState::TorchChoice;
                        self.current_operation = "Выбор версии PyTorch".to_string();
                        self.stage_progress = 0.0;
                        self.stage_label = "Выберите wheel для GPU или оставьте CPU".to_string();
                        self.overall_progress = 0.0;
                        self.overall_label = prompt.summary.clone();
                        self.torch_choice_prompt = Some(prompt);
                    }
                },
                InstallEvent::Finished(Ok(())) => {
                    if let Some(install_target_dir) = self.install_target_dir.as_deref() {
                        match persist_ai_install_type_for_install_target(
                            install_target_dir,
                            self.pending_ai_install_type,
                        ) {
                            Ok(()) => self.console_lines.push(format!(
                                "[Config] Тип ИИ установки сохранён: {}",
                                self.pending_ai_install_type.as_str()
                            )),
                            Err(err) => self.console_lines.push(format!(
                                "[Config] Не удалось сохранить тип ИИ установки: {err}"
                            )),
                        }
                    }
                    self.state = UiState::Completed;
                    self.current_operation = "Установка завершена".to_string();
                    self.stage_progress = 1.0;
                    self.stage_label = "Завершено".to_string();
                    self.overall_progress = 1.0;
                    self.overall_label = "Готово".to_string();
                    self.set_result(InstallerOutcome::Completed);
                }
                InstallEvent::Finished(Err(err)) => {
                    self.state = UiState::Failed;
                    self.current_operation = "Ошибка установки".to_string();
                    self.stage_label = "Этап завершился ошибкой".to_string();
                    self.overall_label = err.clone();
                    self.set_result(InstallerOutcome::Failed(err));
                }
            }
        }

        let mut selected_torch_install: Option<TorchInstallSelection> = None;
        let mut selected_dependency_profile: Option<InstallDependencyProfile> = None;
        let mut requested_start_install = false;
        let mut finish_outcome: Option<InstallerOutcome> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("ManhwaStudio");
            });
            ui.add_space(8.0);

            let center_height = (ui.available_height() - 170.0).max(110.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), center_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| match self.state {
                    UiState::Idle => {
                        ui.add_space((center_height * 0.20).max(8.0));
                        ui.label("Выберите тип установки:");
                        ui.radio_value(
                            &mut self.install_location_choice,
                            InstallLocationChoice::Local,
                            "Установить локально",
                        );
                        ui.radio_value(
                            &mut self.install_location_choice,
                            InstallLocationChoice::AllUsers,
                            "Установить для всех",
                        );
                        ui.radio_value(
                            &mut self.install_location_choice,
                            InstallLocationChoice::Custom,
                            "Другое место",
                        );

                        if self.install_location_choice == InstallLocationChoice::Custom {
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(
                                        &mut self.custom_install_base_dir_input,
                                    )
                                    .desired_width((ui.available_width() - 140.0).max(180.0)),
                                );
                                if ui.button("Выбрать...").clicked() {
                                    let dialog_dir =
                                        PathBuf::from(self.custom_install_base_dir_input.trim());
                                    let base_dir = if dialog_dir.is_dir() {
                                        dialog_dir
                                    } else {
                                        self.root_dir.clone()
                                    };
                                    if let Some(chosen_dir) =
                                        rfd::FileDialog::new().set_directory(base_dir).pick_folder()
                                    {
                                        self.custom_install_base_dir_input =
                                            chosen_dir.to_string_lossy().to_string();
                                    }
                                }
                            });
                        }

                        ui.add_space(6.0);
                        match self.resolved_install_target_dir() {
                            Ok(target) => {
                                ui.small(format!("Установится в: {}", target.display()));
                                if self.install_location_choice == InstallLocationChoice::AllUsers {
                                    ui.small("Понадобится повышение прав.");
                                }
                            }
                            Err(err) => {
                                ui.colored_label(
                                    egui::Color32::from_rgb(220, 80, 80),
                                    format!("Ошибка пути: {err}"),
                                );
                            }
                        }

                        ui.add_space((center_height * 0.15).max(12.0));
                        ui.horizontal_centered(|ui| {
                            if ui
                                .add_sized([180.0, 40.0], egui::Button::new("Установить"))
                                .clicked()
                            {
                                requested_start_install = true;
                            }
                        });
                    }
                    UiState::PreparingTorchChoice => {
                        ui.add_space((center_height * 0.28).max(12.0));
                        ui.horizontal_centered(|ui| {
                            ui.spinner();
                            ui.label("Проверяем доступные GPU и версии CUDA/ROCm...");
                        });
                    }
                    UiState::DependencyProfileChoice => {
                        ui.add_space((center_height * 0.16).max(8.0));
                        ui.label("Выберите набор Python-зависимостей:");
                        ui.add_space(8.0);
                        if ui
                            .add_sized([300.0, 38.0], egui::Button::new("Быстрая установка"))
                            .clicked()
                        {
                            selected_dependency_profile = Some(InstallDependencyProfile::Fast);
                        }
                        ui.small("Базовые зависимости без PyTorch и torch-зависимых AI-модулей.");
                        ui.add_space(12.0);
                        if ui
                            .add_sized([300.0, 38.0], egui::Button::new("Полная установка"))
                            .clicked()
                        {
                            selected_dependency_profile = Some(InstallDependencyProfile::Full);
                        }
                        ui.small("Выбор PyTorch, затем дополнительные torch-зависимости.");
                    }
                    UiState::TorchChoice => {
                        if let Some(prompt) = &self.torch_choice_prompt {
                            ui.label("Доступные wheels PyTorch:");
                            ui.small(&prompt.summary);
                            if prompt.options.is_empty() {
                                ui.add_space(8.0);
                                ui.small("GPU-варианты не найдены, можно оставить CPU.");
                            } else {
                                ui.add_space(8.0);
                                ui.small(format!(
                                    "Рекомендуется: {}",
                                    prompt.options[prompt.recommended_index].label
                                ));
                                ui.add_space(8.0);

                                for (idx, option) in prompt.options.iter().enumerate() {
                                    let title = if idx == prompt.recommended_index {
                                        format!("{} (Рекомендуется)", option.label)
                                    } else {
                                        option.label.clone()
                                    };
                                    if ui
                                        .add_sized([260.0, 34.0], egui::Button::new(title))
                                        .clicked()
                                    {
                                        selected_torch_install =
                                            Some(TorchInstallSelection::InstallGpu(option.clone()));
                                    }
                                }
                            }
                        }

                        ui.add_space(10.0);
                        if ui
                            .add_sized([260.0, 34.0], egui::Button::new("Оставить на CPU"))
                            .clicked()
                        {
                            selected_torch_install = Some(TorchInstallSelection::SkipCpu);
                        }
                    }
                    UiState::Running => {
                        if !self.console_lines.is_empty() {
                            let console_height = (center_height - 12.0).max(110.0);
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_min_height(console_height);
                                egui::ScrollArea::vertical()
                                    .stick_to_bottom(true)
                                    .show(ui, |ui| {
                                        ui.with_layout(
                                            egui::Layout::top_down(egui::Align::Min),
                                            |ui| {
                                                for line in &self.console_lines {
                                                    ui.monospace(line);
                                                }
                                            },
                                        );
                                    });
                            });
                        }
                    }
                    UiState::Completed => {
                        ui.add_space((center_height * 0.25).max(12.0));
                        ui.horizontal_centered(|ui| {
                            ui.heading("Установка завершена");
                        });
                        ui.add_space(10.0);
                        ui.horizontal_centered(|ui| {
                            if ui
                                .add_sized([210.0, 36.0], egui::Button::new("Открыть"))
                                .clicked()
                            {
                                self.maybe_create_windows_shortcuts();
                                if self.invite_telegram {
                                    let _ = open_url_in_browser(TELEGRAM_INVITE_URL);
                                }
                                if self.invite_discord {
                                    let _ = open_url_in_browser(DISCORD_INVITE_URL);
                                }
                                let install_dir = self
                                    .install_target_dir
                                    .clone()
                                    .unwrap_or_else(|| self.root_dir.join(INSTALL_SUBDIR_NAME));
                                finish_outcome =
                                    Some(InstallerOutcome::LaunchLauncher(install_dir));
                            }
                            ui.add_space(10.0);
                            ui.vertical(|ui| {
                                ui.checkbox(
                                    &mut self.invite_telegram,
                                    "Подписаться на мой Telegram",
                                );
                                ui.checkbox(&mut self.invite_discord, "Зайти на сервер Discord");
                                #[cfg(target_os = "windows")]
                                ui.checkbox(
                                    &mut self.create_windows_desktop_shortcut,
                                    "Создать ярлык на рабочем столе",
                                );
                                #[cfg(target_os = "windows")]
                                ui.checkbox(
                                    &mut self.create_windows_start_menu_shortcut,
                                    "Создать ярлык в меню Пуск",
                                );
                            });
                        });
                        ui.add_space(8.0);
                        ui.horizontal_centered(|ui| {
                            if ui.button("Закрыть").clicked() {
                                self.maybe_create_windows_shortcuts();
                                finish_outcome = Some(InstallerOutcome::Completed);
                            }
                        });
                    }
                    UiState::Failed => {
                        if !self.console_lines.is_empty() {
                            let console_height = (center_height * 0.76).max(110.0);
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_min_height(console_height);
                                egui::ScrollArea::vertical()
                                    .stick_to_bottom(true)
                                    .show(ui, |ui| {
                                        ui.with_layout(
                                            egui::Layout::top_down(egui::Align::Min),
                                            |ui| {
                                                for line in &self.console_lines {
                                                    ui.monospace(line);
                                                }
                                            },
                                        );
                                    });
                            });
                            ui.add_space(8.0);
                        }
                        ui.horizontal_centered(|ui| {
                            if ui.button("Закрыть").clicked() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        });
                    }
                },
            );
            ui.add_space(6.0);
            ui.label(format!("Текущая операция: {}", self.current_operation));
            ui.add_space(4.0);
            ui.label(format!("Этап: {}", self.stage_label));
            ui.add(egui::ProgressBar::new(self.stage_progress).show_percentage());
            ui.small(format!(
                "Прогресс этапа: {:.0}%",
                self.stage_progress * 100.0
            ));
            ui.add_space(4.0);
            ui.label("Общий прогресс установки");
            ui.add(egui::ProgressBar::new(self.overall_progress).show_percentage());
            ui.small(&self.overall_label);

            if self.state == UiState::Running {
                ui.small(format!(
                    "{} / {}",
                    env::consts::OS,
                    detect_arch_label(&detect_arch().unwrap_or("unknown".to_string()))
                ));
            }
        });

        if let Some(selection) = selected_torch_install {
            if let Some(target_dir) = self.install_target_dir.clone() {
                self.start_install(target_dir, InstallDependencyProfile::Full, selection);
            } else {
                self.state = UiState::Failed;
                self.current_operation = "Ошибка установки".to_string();
                self.stage_label = "Этап завершился ошибкой".to_string();
                self.overall_label = "не выбрана целевая папка установки".to_string();
                self.set_result(InstallerOutcome::Failed(
                    "не выбрана целевая папка установки".to_string(),
                ));
            }
        }
        if let Some(profile) = selected_dependency_profile {
            if let Some(target_dir) = self.install_target_dir.clone() {
                match profile {
                    InstallDependencyProfile::Fast => {
                        self.start_install(
                            target_dir,
                            InstallDependencyProfile::Fast,
                            TorchInstallSelection::SkipCpu,
                        );
                    }
                    InstallDependencyProfile::Full => {
                        self.start_torch_preflight(target_dir);
                    }
                }
            } else {
                self.state = UiState::Failed;
                self.current_operation = "Ошибка установки".to_string();
                self.stage_label = "Этап завершился ошибкой".to_string();
                self.overall_label = "не выбрана целевая папка установки".to_string();
                self.set_result(InstallerOutcome::Failed(
                    "не выбрана целевая папка установки".to_string(),
                ));
            }
        }
        if requested_start_install {
            match self.resolved_install_target_dir() {
                Ok(target_dir) => {
                    if self.install_requires_elevation(&target_dir) {
                        match relaunch_self_elevated(&self.root_dir, &target_dir) {
                            Ok(()) => {
                                self.set_result(InstallerOutcome::ElevatedRelaunchStarted);
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            Err(err) => {
                                self.current_operation =
                                    "Не удалось запросить повышение прав".to_string();
                                self.overall_label = err;
                            }
                        }
                    } else {
                        self.show_dependency_profile_choice(target_dir);
                    }
                }
                Err(err) => {
                    self.current_operation = "Ошибка пути установки".to_string();
                    self.overall_label = err;
                }
            }
        }
        if let Some(outcome) = finish_outcome {
            self.set_result(outcome);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum UiState {
    Idle,
    DependencyProfileChoice,
    PreparingTorchChoice,
    TorchChoice,
    Running,
    Completed,
    Failed,
}

fn persist_ai_install_type_for_install_target(
    install_target_dir: &Path,
    install_type: config::AiInstallType,
) -> Result<(), String> {
    let mut cfg = config::JsonConfig::new(
        install_target_dir.join(config::USER_CONFIG_FILE),
        config::user_config_defaults(),
    )
    .map_err(|err| {
        format!(
            "не удалось открыть user_config.json в '{}': {err:#}",
            install_target_dir.display()
        )
    })?;
    cfg.set_path(
        &["General", config::GENERAL_AI_INSTALL_TYPE_KEY],
        serde_json::Value::String(install_type.as_str().to_string()),
    )
    .map_err(|err| {
        format!(
            "не удалось записать тип ИИ установки в '{}': {err:#}",
            cfg.path.display()
        )
    })
}

#[derive(Debug)]
pub(crate) enum InstallEvent {
    Step(String),
    ConsoleLine(String),
    Progress {
        stage_value: f32,
        stage_label: String,
        overall_value: f32,
        overall_label: String,
    },
    TorchPreflightReady(TorchPreflightResult),
    Finished(Result<(), String>),
}

#[cfg(target_os = "windows")]
pub(super) enum UninstallEvent {
    Progress {
        value: f32,
        status: String,
        detail: String,
    },
    Finished(Result<(), String>),
}

#[cfg(target_os = "windows")]
struct UninstallApp {
    rx: mpsc::Receiver<UninstallEvent>,
    progress: f32,
    status: String,
    detail: String,
    error: Option<String>,
    close_after: Option<Instant>,
    result_sink: Arc<Mutex<Option<Result<(), String>>>>,
}

#[cfg(target_os = "windows")]
impl UninstallApp {
    fn new(
        rx: mpsc::Receiver<UninstallEvent>,
        result_sink: Arc<Mutex<Option<Result<(), String>>>>,
    ) -> Self {
        Self {
            rx,
            progress: 0.0,
            status: "Подготовка удаления".to_string(),
            detail: "Собираем план очистки...".to_string(),
            error: None,
            close_after: None,
            result_sink,
        }
    }

    fn set_result(&self, result: Result<(), String>) {
        if let Ok(mut guard) = self.result_sink.lock() {
            *guard = Some(result);
        }
    }
}

#[cfg(target_os = "windows")]
impl eframe::App for UninstallApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                UninstallEvent::Progress {
                    value,
                    status,
                    detail,
                } => {
                    self.progress = value.clamp(0.0, 1.0);
                    self.status = status;
                    self.detail = detail;
                }
                UninstallEvent::Finished(result) => match result {
                    Ok(()) => {
                        self.progress = 1.0;
                        self.status = "Удаление завершено".to_string();
                        self.detail =
                            "Окно закроется после передачи финальной очистки helper-процессу."
                                .to_string();
                        self.set_result(Ok(()));
                        self.close_after = Some(Instant::now() + Duration::from_millis(700));
                    }
                    Err(err) => {
                        self.error = Some(err.clone());
                        self.status = "Ошибка удаления".to_string();
                        self.detail = err.clone();
                        self.set_result(Err(err));
                    }
                },
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("Удаление ManhwaStudio");
            });
            ui.add_space(10.0);
            ui.label(&self.status);
            ui.add_space(4.0);
            ui.add(
                egui::ProgressBar::new(self.progress)
                    .desired_width(ui.available_width())
                    .show_percentage(),
            );
            ui.add_space(8.0);
            ui.small(&self.detail);
            if let Some(err) = &self.error {
                ui.add_space(10.0);
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                ui.add_space(8.0);
                if ui.button("Закрыть").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        });

        if let Some(deadline) = self.close_after {
            if Instant::now() >= deadline {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

#[cfg(target_os = "windows")]
pub(super) fn send_uninstall_progress(
    tx: &mpsc::Sender<UninstallEvent>,
    value: f32,
    status: impl Into<String>,
    detail: impl Into<String>,
) {
    let _ = tx.send(UninstallEvent::Progress {
        value: value.clamp(0.0, 1.0),
        status: status.into(),
        detail: detail.into(),
    });
}

#[cfg(target_os = "windows")]
pub(super) fn run_windows_uninstall_window(
    current_exe: PathBuf,
    install_dir: PathBuf,
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let result_sink = Arc::new(Mutex::new(None::<Result<(), String>>));
    let result_sink_for_app = Arc::clone(&result_sink);

    std::thread::Builder::new()
        .name("mini-launcher-uninstall".to_string())
        .spawn(move || {
            let result = run_windows_uninstall_worker(current_exe, install_dir, &tx);
            let _ = tx.send(UninstallEvent::Finished(result));
        })
        .map_err(|e| format!("не удалось запустить worker удаления: {e}"))?;

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([420.0, 150.0])
        .with_min_inner_size([380.0, 140.0])
        .with_max_inner_size([520.0, 180.0])
        .with_resizable(false);
    if let Some(icon) = load_embedded_icon_data() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Удаление ManhwaStudio",
        native_options,
        Box::new(move |_cc| Ok(Box::new(UninstallApp::new(rx, result_sink_for_app)))),
    )
    .map_err(|e| e.to_string())?;

    let mut guard = result_sink
        .lock()
        .map_err(|_| "не удалось получить результат удаления".to_string())?;
    guard
        .take()
        .unwrap_or_else(|| Err("окно удаления закрыто до завершения операции".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_ai_install_type_writes_installed_user_config() {
        let test_dir = std::env::temp_dir().join(format!(
            "manhwastudio_install_type_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));

        persist_ai_install_type_for_install_target(&test_dir, config::AiInstallType::Base)
            .expect("Base install type should be persisted");
        let raw = std::fs::read_to_string(test_dir.join(config::USER_CONFIG_FILE))
            .expect("written user config should be readable");
        let value: serde_json::Value =
            serde_json::from_str(&raw).expect("written user config should be valid JSON");
        assert_eq!(
            config::AiInstallType::from_user_settings(&value),
            config::AiInstallType::Base
        );

        persist_ai_install_type_for_install_target(&test_dir, config::AiInstallType::Full)
            .expect("Full install type should overwrite Base");
        let raw = std::fs::read_to_string(test_dir.join(config::USER_CONFIG_FILE))
            .expect("updated user config should be readable");
        let value: serde_json::Value =
            serde_json::from_str(&raw).expect("updated user config should be valid JSON");
        assert_eq!(
            config::AiInstallType::from_user_settings(&value),
            config::AiInstallType::Full
        );

        std::fs::remove_dir_all(&test_dir).expect("test config directory should be removable");
    }
}
