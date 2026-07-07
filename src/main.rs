/*
FILE OVERVIEW: src/main.rs
Unified app entrypoint for `manhwastudio_rs`.

Main flow:
- Parses CLI args (`--project` optional, `--no-ai` optional).
- Handles hidden service flags for installer continuation and Windows uninstall before normal startup.
- Handles hidden service flags for installer continuation, Windows uninstall, and elevated Start Menu shortcut creation before normal startup.
- Windows uninstall can relaunch itself elevated with a hidden continuation flag before deleting files from protected install locations.
- If `--project` points to a valid chapter, opens it immediately.
- If chapter has legacy `scr` and no `src`, auto-renames `scr -> src` during startup validation.
- Invalid `--project` startup path is reported both in console and in a modal error dialog.
- If `--project` is missing:
  - checks whether Python env required by installer-dependent features is available;
  - when env is missing, asks whether to start the installer, update a custom install folder, or
    continue into the Rust launcher;
- starts a non-blocking GitHub release version check before the Rust launcher and lets the launcher
  return an update intent when the user clicks "Обновить";
- supports `--update` to open the Rust update window directly before normal startup routing;
- supports hidden `--continue-update` to resume update work after the executable has been replaced;
- can update an already installed copy found through Windows install discovery or a user-selected
  custom install folder;
- prepares startup artifacts (`user_config.json`, `ManhwaStudio_AI_Models`, `last.log/previous.log`) before Rust launcher startup.
- auto-detects and persists `General.ai_install_type` when that key is absent.
- Validates chapter folders in a worker thread (`src` + image files in `src`).
- Rust launcher берёт корень проектов из `user_config.json` (`General.projects_dir`)
  с fallback на `{Documents}/manhwastudio_projects`.
- Loads project data and starts the main eframe app.
*/
#![cfg_attr(
    all(target_os = "windows", not(feature = "active-logs")),
    windows_subsystem = "windows"
)]

mod ai_backend_capabilities;
mod ai_backend_panel;
mod ai_backend_supervisor;
mod ai_install_probe;
mod ai_models;
mod app;
mod args;
mod backend_ipc;
mod bubble_status;
mod canvas;
mod config;
pub mod gpu_utils;
mod input_manager_v2;
mod input_util;
mod installer;
mod launcher;
mod memory_manager;
mod models;
mod paste_image;
mod project;
mod python_manager;
mod screen_capture;
mod storage;
mod tabs;
#[cfg(target_arch = "wasm32")]
mod web_entry;
mod tools;
// Onboarding tutorial subsystem; gated behind the `tutorial` feature (off by
// default). The demo bin `src/bin/tutorial_test` mounts `engine.rs` directly via
// `#[path]`, so it stays buildable regardless of this feature.
#[cfg(feature = "tutorial")]
mod tutorial;
pub mod widgets;

// `runtime_log` and `trace` now live in the standalone `ms-log` crate. These
// re-exports keep the existing `crate::runtime_log::…` / `crate::trace::…` module
// paths and the `crate::trace_log!` / `crate::trace_scope!` macro paths valid
// across the whole binary without touching call sites.
pub use ms_log::{runtime_log, trace, trace_log, trace_scope};

// `text_punctuation` now lives in the config-free `ms-text-util` crate. Re-export
// keeps `crate::text_punctuation::…` valid across the binary. The crate no longer
// reads user config itself; `seed_hanging_punctuation_from_config` seeds it at startup.
pub use ms_text_util::text_punctuation;

// Native-only startup imports. All of these feed the native launcher/installer/
// update-check flow (`eframe::run_native`, `rfd`, `ureq`, `clap` CLI, native
// windows) which does not exist on `wasm32`. They are gated so the wasm build
// does not fail on missing deps or unused imports. `std::ffi::OsStr` and
// `std::path::{Path, PathBuf}` stay unconditional because the shared filesystem
// helpers (`list_titles`, `list_chapters`, `validate_project_dir_for_startup`,
// `count_images_in_dir`, `find_unsaved_chapter`) used by other modules need them
// on both targets.
#[cfg(not(target_arch = "wasm32"))]
use crate::widgets::WheelComboBox;
#[cfg(not(target_arch = "wasm32"))]
use anyhow::Context;
#[cfg(not(target_arch = "wasm32"))]
use args::Cli;
#[cfg(not(target_arch = "wasm32"))]
use clap::Parser;
#[cfg(not(target_arch = "wasm32"))]
use installer::install as launcher_install;
#[cfg(not(target_arch = "wasm32"))]
use installer::update::UpdateWindowOutcome;
#[cfg(not(target_arch = "wasm32"))]
use installer::utils::ExternalUpdateTarget;
#[cfg(not(target_arch = "wasm32"))]
use launcher::state::{LauncherOutcome, UpdateNotification};
#[cfg(not(target_arch = "wasm32"))]
use serde::Deserialize;
#[cfg(not(target_arch = "wasm32"))]
use std::cmp::Ordering;
#[cfg(target_os = "linux")]
use std::env;
use std::ffi::OsStr;
#[cfg(target_os = "linux")]
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Mutex, mpsc};
#[cfg(not(target_arch = "wasm32"))]
use ms_thread as thread;
#[cfg(not(target_arch = "wasm32"))]
use web_time::Duration;

const EMBEDDED_APP_ICON_ICO: &[u8] = include_bytes!("../app_icon.ico");
const EMBEDDED_APP_ICON_PNG: &[u8] = include_bytes!("../app_icon_512.png");
#[allow(dead_code)]
const LAUNCHER_OUTPUT_PREFIX: &str = "MS_OPEN_PROJECT_JSON=";
#[allow(dead_code)]
const UPDATE_API_RELEASES: &str = "https://api.github.com/repos/Vasyanator/ManhwaStudio/releases";
#[allow(dead_code)]
const UPDATE_ASSET_NAME: &str = "ManhwaStudio.zip";
#[cfg(target_os = "linux")]
const MAIN_WINDOW_APP_ID: &str = "manhwastudio_rs";
#[cfg(not(target_os = "linux"))]
const MAIN_WINDOW_APP_ID: &str = "manhwastudio_rs.main";
const UPDATE_CHECK_WINDOW_APP_ID: &str = "manhwastudio_rs.update_check";
const BASIC_LAUNCHER_WINDOW_APP_ID: &str = "manhwastudio_rs.basic_launcher";

// Web entry: boots the eframe WebRunner on the page canvas. Real startup lives
// in `web_entry.rs`; the native launcher/installer flow below is compiled out.
#[cfg(target_arch = "wasm32")]
fn main() {
    web_entry::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> anyhow::Result<()> {
    let result = run_main();
    match &result {
        Ok(()) => runtime_log::log_info("manhwastudio_rs exited normally"),
        Err(err) => runtime_log::log_error(format!("fatal startup/runtime error: {err:#}")),
    }
    result
}

#[cfg(not(target_arch = "wasm32"))]
fn run_main() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    install_linux_desktop_integration_async();

    init_startup_logging_best_effort();
    let mut cli = Cli::parse();
    if let Err(err) = trace::init_trace(&config::data_dir(), cli.trace) {
        eprintln!("failed to initialize tracing: {err}");
    }
    trace_log!(
        trace::cat::STARTUP,
        "tracing enabled, args: project={:?} no_ai={} update={} test_launcher={} trace={}",
        cli.project,
        cli.no_ai,
        cli.update,
        cli.test_launcher,
        cli.trace
    );
    #[cfg(target_os = "windows")]
    if let Some(install_dir) = cli.create_start_menu_shortcut_install_dir.as_deref() {
        launcher_install::run_windows_create_start_menu_shortcut_for_install(
            install_dir,
            cli.continue_create_start_menu_shortcut,
        )
        .map_err(anyhow::Error::msg)?;
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    if cli.uninstall {
        launcher_install::run_windows_uninstall_from_current_exe(
            cli.continue_uninstall,
            cli.uninstall_signal_file.as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        return Ok(());
    }

    runtime_log::log_info("starting main application flow");
    auto_detect_missing_ai_install_type_for_startup();
    seed_hanging_punctuation_from_config();
    seed_rotation_ctrl_wheel_from_config();
    // Seed the typing tab's per-effect-kind default overrides so newly added effect
    // cards pick up the user's stored defaults from the first frame.
    tabs::typing::seed_effect_defaults_from_config();
    // Seed the typing tab's user-imported system font list from user config.
    tabs::typing::seed_imported_system_fonts_from_config();
    let user_settings = config::load_user_settings_for_startup()?;

    if cli.continue_update {
        init_runtime_logging();
        if installer::update::run_continue_update_window().map_err(anyhow::Error::msg)?
            == UpdateWindowOutcome::Exit
        {
            return Ok(());
        }
    }

    if cli.update {
        init_runtime_logging();
        if installer::update::run_update_window(cli.test_ver_check).map_err(anyhow::Error::msg)?
            == UpdateWindowOutcome::Exit
        {
            return Ok(());
        }
    }

    if cli.test_launcher {
        init_runtime_logging();
        let supervisor = ai_backend_supervisor::AiBackendSupervisor::start(!cli.no_ai);
        return launcher::run_test_launcher(
            &user_settings,
            Some(spawn_startup_update_check(cli.test_ver_check)),
            &supervisor.handle(),
        );
    }

    let ai_enabled = !cli.no_ai;
    // The single Python AI backend is owned here, above the launcher/studio loop, so it
    // starts in the launcher (per the autostart toggle) and survives transitions between
    // the launcher and the studio. Dropped (which stops the process + probe) on any return.
    let ai_backend_supervisor = ai_backend_supervisor::AiBackendSupervisor::start(ai_enabled);
    let ai_backend = ai_backend_supervisor.handle();

    // Main loop: re-enters the launcher when the user chooses "Выйти в лаунчер".
    loop {
        let project_dir = resolve_startup_project_dir(&cli, &user_settings, &ai_backend)?;
        let Some(project_dir) = project_dir else {
            return Ok(());
        };

        // Detect resume_unsaved: look for a {chapter}_unsaved folder next to the chapter.
        let resume_unsaved = detect_unsaved_for_project(&project_dir);
        let project = if resume_unsaved {
            project::ProjectData::load_resume_unsaved(&project_dir, &user_settings)
        } else {
            project::ProjectData::load(&project_dir, &user_settings)
        }
        .with_context(|| format!("failed to load project at {}", project_dir.display()))?;

        match run_main_window(project, ai_backend.clone())? {
            RunResult::Exit => return Ok(()),
            RunResult::ReturnToLauncher => {
                // Clear the --project CLI flag so the launcher is shown next iteration.
                cli.project = None;
                runtime_log::log_info("returning to launcher");
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn init_startup_logging_best_effort() {
    let log_dir = config::data_dir();
    if let Err(err) = runtime_log::init_session_logs(&log_dir) {
        eprintln!("[runtime-log] failed to initialize logging: {err}");
    }
}

/// Outcome reported by `run_main_window` after the eframe window closes.
///
/// Native-only: this only describes the native windowed run loop.
#[cfg(not(target_arch = "wasm32"))]
pub enum RunResult {
    Exit,
    ReturnToLauncher,
}

/// Returns true when there is a `{chapter}_unsaved` folder adjacent to the chapter dir.
#[cfg(not(target_arch = "wasm32"))]
fn detect_unsaved_for_project(project_dir: &Path) -> bool {
    let Some(chapter_name) = project_dir.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let Some(title_dir) = project_dir.parent() else {
        return false;
    };
    title_dir.join(format!("{chapter_name}_unsaved")).is_dir()
}

/// Returns Some(chapter_name) when the given title folder contains a `{chapter}_unsaved` dir
/// that has a corresponding base chapter dir.
pub(crate) fn find_unsaved_chapter(projects_root: &Path, title: &str) -> Option<String> {
    let title_dir = projects_root.join(title);
    let entries = std::fs::read_dir(&title_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(base) = name_str.strip_suffix("_unsaved")
            && !base.is_empty()
            && title_dir.join(base).is_dir()
        {
            return Some(base.to_string());
        }
    }
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn init_runtime_logging() {
    let log_dir = config::data_dir();
    if let Err(err) = runtime_log::init_session_logs(&log_dir) {
        eprintln!("[runtime-log] failed to initialize logging: {err}");
        return;
    }
    runtime_log::log_info(format!(
        "session log files: last='{}', previous='{}'",
        log_dir.join("last.log").display(),
        log_dir.join("previous.log").display()
    ));
}

#[cfg(not(target_arch = "wasm32"))]
fn ensure_standard_launcher_startup_artifacts() -> anyhow::Result<()> {
    init_runtime_logging();
    config::ensure_model_dirs()?;
    let _ = config::load_user_config()?;
    Ok(())
}

/// Seeds the config-free `ms_text_util::text_punctuation` set from user config at
/// startup. The util crate defaults to `DEFAULT_HANGING_PUNCTUATION`; here the app
/// (which owns config) overrides it with `TextTab.hanging_punctuation` when present
/// and non-blank. Best-effort: on any config error the default set is kept.
#[cfg(not(target_arch = "wasm32"))]
fn seed_hanging_punctuation_from_config() {
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

/// Seeds the runtime `rotation_ctrl_wheel` global from user config at startup.
/// The module defaults to `DEFAULT_ROTATION_CTRL_WHEEL_MODE` (`Vector`); here the app
/// overrides it with `TextTab.rotation_ctrl_wheel_mode` when present and recognized.
/// Best-effort: on any config error or unknown value the default mode is kept.
#[cfg(not(target_arch = "wasm32"))]
fn seed_rotation_ctrl_wheel_from_config() {
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

#[cfg(not(target_arch = "wasm32"))]
fn auto_detect_missing_ai_install_type_for_startup() {
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

#[cfg(not(target_arch = "wasm32"))]
fn resolve_startup_project_dir(
    cli: &Cli,
    user_settings: &serde_json::Value,
    ai_backend: &ai_backend_supervisor::AiBackendHandle,
) -> anyhow::Result<Option<PathBuf>> {
    if let Some(project_dir) = &cli.project {
        return resolve_cli_project_dir(project_dir);
    }
    resolve_project_dir_without_cli_arg(
        user_settings,
        cli.test_ver_check,
        cli.continue_install || cli.continue_install_target.is_some(),
        cli.continue_install_target.clone(),
        ai_backend,
    )
}

#[cfg(not(target_arch = "wasm32"))]
fn resolve_cli_project_dir(project_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let path = project_dir.to_path_buf();
    match validate_project_dir_for_startup(&path) {
        ProjectValidationState::Valid { .. } => Ok(Some(path)),
        ProjectValidationState::Invalid { message } => {
            let full_message = format!("--project path is invalid: {message}");
            show_startup_error_dialog(&full_message);
            anyhow::bail!("{full_message}")
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn resolve_project_dir_without_cli_arg(
    user_settings: &serde_json::Value,
    force_update_available: bool,
    force_run_installer: bool,
    auto_install_target: Option<PathBuf>,
    ai_backend: &ai_backend_supervisor::AiBackendHandle,
) -> anyhow::Result<Option<PathBuf>> {
    if should_enter_installer_flow(force_run_installer, auto_install_target.as_ref()) {
        return run_startup_installer(config::program_dir(), auto_install_target);
    }

    let program_dir = config::program_dir();
    let has_python_env = python_manager::has_supported_python_env(&program_dir);

    #[cfg(target_os = "windows")]
    if !has_python_env {
        let existing_install_action =
            match launcher_install::handle_existing_windows_install(&program_dir) {
                Ok(action) => action,
                Err(err) => {
                    runtime_log::log_warn(format!(
                        "existing Windows install detection failed for '{}': {err}",
                        program_dir.display()
                    ));
                    launcher_install::ExistingInstallAction::NoInstallFound
                }
            };
        match existing_install_action {
            launcher_install::ExistingInstallAction::NoInstallFound => {}
            launcher_install::ExistingInstallAction::ExitCurrentCopy => return Ok(None),
            launcher_install::ExistingInstallAction::StartInstaller(target_dir) => {
                return run_startup_installer(program_dir, Some(target_dir));
            }
            launcher_install::ExistingInstallAction::UpdateInstalled(target) => {
                let _ = installer::update::run_external_install_update_window(target)
                    .map_err(anyhow::Error::msg)?;
                return Ok(None);
            }
        }
    }

    if !has_python_env {
        match prompt_missing_python_env_action() {
            MissingPythonEnvAction::Install => {
                return run_startup_installer(config::program_dir(), None);
            }
            MissingPythonEnvAction::UpdateCustom(target) => {
                let _ = installer::update::run_external_install_update_window(target)
                    .map_err(anyhow::Error::msg)?;
                return Ok(None);
            }
            MissingPythonEnvAction::LaunchLauncher => {}
        }
    }

    ensure_standard_launcher_startup_artifacts()?;
    match launcher::run_launcher(
        user_settings,
        Some(spawn_startup_update_check(force_update_available)),
        ai_backend,
    )? {
        Some(LauncherOutcome::OpenProject(selection)) => Ok(Some(selection.project_dir)),
        Some(LauncherOutcome::StartUpdate) => {
            let _ = installer::update::run_update_window(force_update_available)
                .map_err(anyhow::Error::msg)?;
            Ok(None)
        }
        None => Ok(None),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_startup_update_check(
    force_update_available: bool,
) -> mpsc::Receiver<Option<UpdateNotification>> {
    let (tx, rx) = mpsc::channel();
    let local_version = env!("CARGO_PKG_VERSION").to_string();

    let spawn_result = thread::Builder::new()
        .name("startup-version-check".to_string())
        .spawn(move || {
            let notification = if force_update_available {
                Some(UpdateNotification {
                    local_version,
                    remote_version: "test-update".to_string(),
                })
            } else {
                match fetch_latest_release_tag() {
                    Ok(remote_version) if is_remote_newer(&local_version, &remote_version) => {
                        Some(UpdateNotification {
                            local_version,
                            remote_version,
                        })
                    }
                    Ok(remote_version) => {
                        runtime_log::log_info(format!(
                            "[startup-version-check] no update: local={}, remote={}",
                            local_version, remote_version
                        ));
                        None
                    }
                    Err(err) => {
                        runtime_log::log_warn(format!("[startup-version-check] {err}"));
                        None
                    }
                }
            };
            let _ = tx.send(notification);
        });

    if let Err(err) = spawn_result {
        runtime_log::log_warn(format!(
            "[startup-version-check] failed to spawn version check worker: {err}"
        ));
    }

    rx
}

#[cfg(not(target_arch = "wasm32"))]
enum MissingPythonEnvAction {
    Install,
    UpdateCustom(ExternalUpdateTarget),
    LaunchLauncher,
}

#[cfg(not(target_arch = "wasm32"))]
fn prompt_missing_python_env_action() -> MissingPythonEnvAction {
    let output = Arc::new(Mutex::new(MissingPythonEnvAction::LaunchLauncher));
    let output_for_app = Arc::clone(&output);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([520.0, 250.0])
        .with_min_inner_size([480.0, 230.0])
        .with_resizable(false);
    if let Some(icon) = load_embedded_icon_data() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    if let Err(err) = eframe::run_native(
        "ManhwaStudio",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(MissingPythonEnvPromptApp {
                output: output_for_app,
                error_text: None,
            }))
        }),
    ) {
        runtime_log::log_warn(format!("missing-env prompt failed: {err}"));
        return MissingPythonEnvAction::LaunchLauncher;
    }

    output
        .lock()
        .map(|mut guard| std::mem::replace(&mut *guard, MissingPythonEnvAction::LaunchLauncher))
        .unwrap_or(MissingPythonEnvAction::LaunchLauncher)
}

#[cfg(not(target_arch = "wasm32"))]
struct MissingPythonEnvPromptApp {
    output: Arc<Mutex<MissingPythonEnvAction>>,
    error_text: Option<String>,
}

#[cfg(not(target_arch = "wasm32"))]
impl MissingPythonEnvPromptApp {
    fn set_output_and_close(&self, action: MissingPythonEnvAction, ctx: &egui::Context) {
        if let Ok(mut output) = self.output.lock() {
            *output = action;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn choose_custom_update_target(&mut self, ctx: &egui::Context) {
        let Some(folder) = rfd::FileDialog::new()
            .set_title("Выберите папку установки ManhwaStudio")
            .pick_folder()
        else {
            return;
        };
        match launcher_install::resolve_installed_program_copy_path(&folder) {
            Ok(executable_path) => {
                self.set_output_and_close(
                    MissingPythonEnvAction::UpdateCustom(ExternalUpdateTarget {
                        root_dir: folder,
                        executable_path,
                    }),
                    ctx,
                );
            }
            Err(err) => {
                self.error_text = Some(err);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl eframe::App for MissingPythonEnvPromptApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35 hands `App::ui` the window-root `Ui`; keep a borrowed `Context` handle for
        // the calls (viewport commands, helper methods) that still operate on the context.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("Программа не установлена");
                ui.add_space(8.0);
                ui.label(
                    "Выполнить установку сейчас или обновить установленную копию в другой папке?",
                );
                if let Some(error_text) = &self.error_text {
                    ui.add_space(8.0);
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), error_text);
                }
                ui.add_space(14.0);
                if ui
                    .add_sized([360.0, 34.0], egui::Button::new("Установить"))
                    .clicked()
                {
                    self.set_output_and_close(MissingPythonEnvAction::Install, ctx);
                }
                ui.add_space(6.0);
                if ui
                    .add_sized(
                        [360.0, 34.0],
                        egui::Button::new("Обновить программу в кастомном месте установки"),
                    )
                    .clicked()
                {
                    self.choose_custom_update_target(ctx);
                }
                ui.add_space(6.0);
                if ui
                    .add_sized([360.0, 34.0], egui::Button::new("Открыть базовый лаунчер"))
                    .clicked()
                {
                    self.set_output_and_close(MissingPythonEnvAction::LaunchLauncher, ctx);
                }
            });
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn should_enter_installer_flow(
    force_run_installer: bool,
    auto_install_target: Option<&PathBuf>,
) -> bool {
    force_run_installer || auto_install_target.is_some()
}

#[cfg(not(target_arch = "wasm32"))]
fn run_startup_installer(
    app_dir: PathBuf,
    auto_install_target: Option<PathBuf>,
) -> anyhow::Result<Option<PathBuf>> {
    match launcher_install::run_python_installer_window(&app_dir, auto_install_target)
        .map_err(anyhow::Error::msg)?
    {
        launcher_install::InstallerOutcome::Completed => Ok(None),
        launcher_install::InstallerOutcome::LaunchLauncher(install_dir) => {
            launcher_install::spawn_installed_program_copy(&install_dir)
                .map_err(anyhow::Error::msg)?;
            Ok(None)
        }
        launcher_install::InstallerOutcome::ElevatedRelaunchStarted => Ok(None),
        launcher_install::InstallerOutcome::Cancelled => Ok(None),
        launcher_install::InstallerOutcome::Failed(err) => Err(anyhow::Error::msg(err)),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn show_startup_error_dialog(message: &str) {
    runtime_log::log_error(format!("startup error dialog shown: {message}"));
    let _ = rfd::MessageDialog::new()
        .set_title("ManhwaStudio - Ошибка запуска")
        .set_description(message)
        .set_buttons(rfd::MessageButtons::Ok)
        .set_level(rfd::MessageLevel::Error)
        .show();
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreLaunchAction {
    LaunchLauncher,
    RunUpdaterAndExit,
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn open_launcher_with_optional_update_check(
    app_dir: &Path,
    create_startup_artifacts: bool,
) -> anyhow::Result<Option<PathBuf>> {
    let action = decide_pre_launch_action(app_dir)?;
    if action == PreLaunchAction::RunUpdaterAndExit {
        if let Err(err) = spawn_python_update_and_exit(app_dir) {
            let message = format!(
                "Не удалось запустить update.py: {err}\n\nЗапускаю лаунчер без обновления."
            );
            show_startup_error_dialog(&message);
        } else {
            return Ok(None);
        }
    }
    run_python_launcher_and_wait_for_project(app_dir, create_startup_artifacts)
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn decide_pre_launch_action(app_dir: &Path) -> anyhow::Result<PreLaunchAction> {
    let config_path = app_dir.join("config.py");
    if !config_path.is_file() {
        return Ok(PreLaunchAction::LaunchLauncher);
    }

    let local_version =
        read_local_version_from_config(&config_path).unwrap_or_else(|| "2.0".to_string());
    let decision = match run_update_check_window(local_version) {
        Ok(decision) => decision,
        Err(err) => {
            runtime_log::log_error(format!("[startup-update-check] UI error: {err}"));
            eprintln!("[startup-update-check] UI error: {err}");
            return Ok(PreLaunchAction::LaunchLauncher);
        }
    };
    Ok(match decision {
        UpdateCheckDecision::LaunchLauncher => PreLaunchAction::LaunchLauncher,
        UpdateCheckDecision::RunUpdaterAndExit => PreLaunchAction::RunUpdaterAndExit,
    })
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn spawn_python_update_and_exit(app_dir: &Path) -> anyhow::Result<()> {
    runtime_log::log_info(format!("launching update.py from '{}'", app_dir.display()));
    let mut cmd = build_python_update_command(app_dir).map_err(anyhow::Error::msg)?;
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("failed to launch update.py: {e}"))?;
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn run_python_launcher_and_wait_for_project(
    app_dir: &Path,
    create_startup_artifacts: bool,
) -> anyhow::Result<Option<PathBuf>> {
    if create_startup_artifacts {
        ensure_standard_launcher_startup_artifacts()?;
    }
    runtime_log::log_info(format!(
        "launching launcher.py from '{}'",
        app_dir.display()
    ));
    let mut cmd = build_python_launcher_command(app_dir).map_err(anyhow::Error::msg)?;
    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("failed to launch launcher.py: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !stdout.trim().is_empty() {
        runtime_log::log_info(format!("[launcher.py][stdout] {}", stdout.trim()));
    }
    if !stderr.trim().is_empty() {
        runtime_log::log_warn(format!("[launcher.py][stderr] {}", stderr.trim()));
    }

    let selected = parse_selected_project_from_stdout(&stdout)?;
    if !output.status.success() && selected.is_none() {
        let code = output.status.code().unwrap_or(-1);
        runtime_log::log_error(format!(
            "launcher.py exited with code {code}; stderr='{}'",
            stderr.trim()
        ));
        anyhow::bail!(
            "launcher.py exited with code {code}. stderr: {}",
            stderr.trim()
        );
    }

    let Some(project_dir) = selected else {
        runtime_log::log_info("launcher.py exited without selected project");
        return Ok(None);
    };
    runtime_log::log_info(format!(
        "launcher selected project '{}'",
        project_dir.display()
    ));

    match validate_project_dir_for_startup(&project_dir) {
        ProjectValidationState::Valid { .. } => Ok(Some(project_dir)),
        ProjectValidationState::Invalid { message } => {
            anyhow::bail!("launcher returned invalid project path: {message}")
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct LauncherSelectionPayload {
    project: String,
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn parse_selected_project_from_stdout(stdout: &str) -> anyhow::Result<Option<PathBuf>> {
    for line in stdout.lines().rev() {
        let trimmed = line.trim();
        let Some(payload_raw) = trimmed.strip_prefix(LAUNCHER_OUTPUT_PREFIX) else {
            continue;
        };
        let payload: LauncherSelectionPayload = serde_json::from_str(payload_raw)
            .with_context(|| "invalid launcher selection payload")?;
        let project = payload.project.trim();
        if project.is_empty() {
            anyhow::bail!("launcher returned empty project path");
        }
        return Ok(Some(PathBuf::from(project)));
    }
    Ok(None)
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn has_launcher_script(app_dir: &Path) -> bool {
    app_dir.join("launcher.py").is_file()
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
#[cfg(target_os = "windows")]
fn build_python_launcher_command(app_dir: &Path) -> Result<std::process::Command, String> {
    python_manager::build_python_script_command(app_dir, "launcher.py")
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
#[cfg(target_os = "windows")]
fn build_python_update_command(app_dir: &Path) -> Result<std::process::Command, String> {
    python_manager::build_python_script_command(app_dir, "update.py")
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
#[cfg(not(target_os = "windows"))]
fn build_python_launcher_command(app_dir: &Path) -> Result<std::process::Command, String> {
    python_manager::build_python_script_command(app_dir, "launcher.py")
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
#[cfg(not(target_os = "windows"))]
fn build_python_update_command(app_dir: &Path) -> Result<std::process::Command, String> {
    python_manager::build_python_script_command(app_dir, "update.py")
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateCheckDecision {
    LaunchLauncher,
    RunUpdaterAndExit,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct UpdateCheckResult {
    remote_version: String,
    update_available: bool,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
enum UpdateCheckUiState {
    Checking,
    UpdateAvailable { remote_version: String },
    Error { message: String },
}

#[cfg(not(target_arch = "wasm32"))]
struct UpdateCheckApp {
    local_version: String,
    state: UpdateCheckUiState,
    pending_check: Option<mpsc::Receiver<Result<UpdateCheckResult, String>>>,
    output_decision: Arc<Mutex<Option<UpdateCheckDecision>>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl UpdateCheckApp {
    fn new(
        local_version: String,
        output_decision: Arc<Mutex<Option<UpdateCheckDecision>>>,
    ) -> Self {
        let mut app = Self {
            local_version,
            state: UpdateCheckUiState::Checking,
            pending_check: None,
            output_decision,
        };
        app.start_check();
        app
    }

    fn start_check(&mut self) {
        let local_version = self.local_version.clone();
        let (tx, rx) = mpsc::channel();
        self.pending_check = Some(rx);
        self.state = UpdateCheckUiState::Checking;

        let _ = thread::Builder::new()
            .name("startup-update-check".to_string())
            .spawn(move || {
                let result = fetch_latest_release_tag().map(|remote_version| UpdateCheckResult {
                    update_available: is_remote_newer(&local_version, &remote_version),
                    remote_version,
                });
                let _ = tx.send(result);
            });
    }

    fn set_decision_and_close(&self, decision: UpdateCheckDecision, ctx: &egui::Context) {
        if let Ok(mut out) = self.output_decision.lock() {
            *out = Some(decision);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn poll_check_result(&mut self, ctx: &egui::Context) {
        let mut should_clear_receiver = false;
        if let Some(rx) = &self.pending_check {
            match rx.try_recv() {
                Ok(Ok(result)) => {
                    should_clear_receiver = true;
                    if result.update_available {
                        self.state = UpdateCheckUiState::UpdateAvailable {
                            remote_version: result.remote_version,
                        };
                    } else {
                        self.set_decision_and_close(UpdateCheckDecision::LaunchLauncher, ctx);
                    }
                }
                Ok(Err(err)) => {
                    should_clear_receiver = true;
                    self.state = UpdateCheckUiState::Error { message: err };
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    should_clear_receiver = true;
                    self.state = UpdateCheckUiState::Error {
                        message: "Проверка обновлений завершилась ошибкой.".to_string(),
                    };
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        if should_clear_receiver {
            self.pending_check = None;
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl eframe::App for UpdateCheckApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35: `App::ui` receives the window-root `Ui`. Keep a borrowed `Context` handle
        // for the context-level calls (poll, viewport commands, repaint scheduling) below.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.poll_check_result(ctx);

        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("Проверка обновлений");
                ui.add_space(8.0);
                ui.label(format!("Локальная версия: {}", self.local_version));
                ui.add_space(10.0);

                match &self.state {
                    UpdateCheckUiState::Checking => {
                        ui.spinner();
                        ui.label("Проверяю последний релиз ManhwaStudio на GitHub...");
                    }
                    UpdateCheckUiState::UpdateAvailable { remote_version } => {
                        ui.colored_label(
                            egui::Color32::from_rgb(120, 210, 120),
                            format!("Доступна новая версия: {remote_version}"),
                        );
                    }
                    UpdateCheckUiState::Error { message } => {
                        ui.colored_label(egui::Color32::from_rgb(230, 120, 120), message);
                    }
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if matches!(self.state, UpdateCheckUiState::Error { .. })
                        && ui.button("Повторить проверку").clicked()
                    {
                        self.start_check();
                    }

                    if matches!(self.state, UpdateCheckUiState::UpdateAvailable { .. })
                        && ui.button("Обновить").clicked()
                    {
                        self.set_decision_and_close(UpdateCheckDecision::RunUpdaterAndExit, ctx);
                    }

                    if ui.button("Пропустить проверку").clicked() {
                        self.set_decision_and_close(UpdateCheckDecision::LaunchLauncher, ctx);
                    }
                });
            });
        });

        if matches!(self.state, UpdateCheckUiState::Checking) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn run_update_check_window(local_version: String) -> anyhow::Result<UpdateCheckDecision> {
    let output = Arc::new(Mutex::new(None::<UpdateCheckDecision>));
    let out = Arc::clone(&output);
    let local_version_for_ui = local_version;

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([540.0, 210.0])
        .with_min_inner_size([460.0, 180.0])
        .with_resizable(true)
        .with_app_id(UPDATE_CHECK_WINDOW_APP_ID);
    if let Some(icon) = load_embedded_icon_data() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "ManhwaStudio - Проверка обновлений",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(UpdateCheckApp::new(
                local_version_for_ui.clone(),
                Arc::clone(&out),
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let decision = output
        .lock()
        .map_err(|_| anyhow::anyhow!("failed to read update-check decision"))?
        .unwrap_or(UpdateCheckDecision::LaunchLauncher);
    Ok(decision)
}

#[cfg(not(target_arch = "wasm32"))]
fn read_local_version_from_config(config_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(config_path).ok()?;
    parse_version_from_config(&content)
}

#[cfg(not(target_arch = "wasm32"))]
fn parse_version_from_config(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("VERSION") {
            continue;
        }

        let after_name = trimmed.strip_prefix("VERSION")?.trim_start();
        let after_eq = after_name.strip_prefix('=')?.trim_start();
        let quote = after_eq.chars().next()?;
        if quote != '"' && quote != '\'' {
            continue;
        }

        let tail = &after_eq[quote.len_utf8()..];
        if let Some(end_idx) = tail.find(quote) {
            return Some(tail[..end_idx].trim().to_string());
        }
    }
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_latest_release_tag() -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .build();

    let mut req = agent
        .get(UPDATE_API_RELEASES)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "UpdateScript/1.0");

    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
    }

    let resp = req
        .call()
        .map_err(|e| format!("ошибка запроса релизов: {e}"))?;
    let body = resp
        .into_string()
        .map_err(|e| format!("ошибка чтения ответа релизов: {e}"))?;

    let releases: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("ошибка JSON релизов: {e}"))?;
    let releases = releases
        .as_array()
        .ok_or_else(|| "неожиданный формат списка релизов".to_string())?;

    for rel in releases {
        let tag = rel
            .get("tag_name")
            .and_then(|v| v.as_str())
            .or_else(|| rel.get("name").and_then(|v| v.as_str()))
            .unwrap_or("")
            .trim();
        if tag.is_empty() {
            continue;
        }

        let has_zip = rel
            .get("assets")
            .and_then(|v| v.as_array())
            .map(|assets| {
                assets.iter().any(|asset| {
                    asset.get("name").and_then(|v| v.as_str()) == Some(UPDATE_ASSET_NAME)
                })
            })
            .unwrap_or(false);

        if has_zip {
            return Ok(tag.to_string());
        }
    }

    Err(format!("не найден релиз с asset '{}'", UPDATE_ASSET_NAME))
}

#[cfg(not(target_arch = "wasm32"))]
fn is_remote_newer(local: &str, remote: &str) -> bool {
    compare_versions(remote, local).is_gt()
}

#[cfg(not(target_arch = "wasm32"))]
fn compare_versions(a: &str, b: &str) -> Ordering {
    let pa = parse_version_to_parts(a);
    let pb = parse_version_to_parts(b);

    for (left, right) in pa.iter().zip(pb.iter()) {
        let ord = compare_version_part(left, right);
        if !ord.is_eq() {
            return ord;
        }
    }

    pa.len().cmp(&pb.len())
}

#[cfg(not(target_arch = "wasm32"))]
fn parse_version_to_parts(version: &str) -> Vec<VersionPart> {
    let normalized = normalize_version(version);
    normalized
        .split(['.', '-', '+', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            if let Ok(num) = part.parse::<u64>() {
                VersionPart::Num(num)
            } else {
                VersionPart::Text(part.to_ascii_lowercase())
            }
        })
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
fn normalize_version(version: &str) -> &str {
    let trimmed = version.trim();
    if let Some(rest) = trimmed.strip_prefix('v') {
        rest
    } else if let Some(rest) = trimmed.strip_prefix('V') {
        rest
    } else {
        trimmed
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn compare_version_part(left: &VersionPart, right: &VersionPart) -> Ordering {
    match (left, right) {
        (VersionPart::Num(a), VersionPart::Num(b)) => a.cmp(b),
        (VersionPart::Text(a), VersionPart::Text(b)) => a.cmp(b),
        (VersionPart::Num(_), VersionPart::Text(_)) => Ordering::Greater,
        (VersionPart::Text(_), VersionPart::Num(_)) => Ordering::Less,
    }
}

#[cfg(not(target_arch = "wasm32"))]
enum VersionPart {
    Num(u64),
    Text(String),
}

#[cfg(not(target_arch = "wasm32"))]
fn run_main_window(
    project: project::ProjectData,
    ai_backend: ai_backend_supervisor::AiBackendHandle,
) -> anyhow::Result<RunResult> {
    let title = format!(
        "ManhwaStudio v{} - {}",
        env!("CARGO_PKG_VERSION"),
        project.project_dir.display()
    );

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1400.0, 900.0])
        .with_min_inner_size([900.0, 600.0])
        .with_app_id(MAIN_WINDOW_APP_ID);
    #[cfg(not(target_os = "windows"))]
    {
        viewport = viewport.with_maximized(true);
    }
    if let Some(icon) = load_embedded_icon_data() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let return_to_launcher_flag = Arc::new(AtomicBool::new(false));
    let flag_for_app = Arc::clone(&return_to_launcher_flag);

    eframe::run_native(
        &title,
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(app::MangaApp::new(
                project,
                ai_backend.clone(),
                flag_for_app,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    if return_to_launcher_flag.load(AtomicOrdering::SeqCst) {
        Ok(RunResult::ReturnToLauncher)
    } else {
        Ok(RunResult::Exit)
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
#[allow(dead_code)]
struct ProjectValidationResult {
    project_dir: PathBuf,
    state: ProjectValidationState,
}

#[derive(Debug)]
pub(crate) enum ProjectValidationState {
    Valid { image_count: usize },
    Invalid { message: String },
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
#[allow(dead_code)]
enum ChooserUiState {
    Empty,
    Validating,
    Ready { image_count: usize },
    Invalid { message: String },
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
struct BasicLauncherApp {
    projects_root: PathBuf,
    titles: Vec<String>,
    chapters: Vec<String>,
    selected_title: Option<String>,
    selected_chapter: Option<String>,
    state: ChooserUiState,
    pending_validation: Option<mpsc::Receiver<ProjectValidationResult>>,
    output_project_dir: Arc<Mutex<Option<PathBuf>>>,
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
impl BasicLauncherApp {
    fn new(projects_root: PathBuf, output_project_dir: Arc<Mutex<Option<PathBuf>>>) -> Self {
        let mut app = Self {
            projects_root,
            titles: Vec::new(),
            chapters: Vec::new(),
            selected_title: None,
            selected_chapter: None,
            state: ChooserUiState::Empty,
            pending_validation: None,
            output_project_dir,
        };
        app.reload_lists();
        app
    }

    fn selected_project_dir(&self) -> Option<PathBuf> {
        let title = self.selected_title.as_ref()?;
        let chapter = self.selected_chapter.as_ref()?;
        Some(self.projects_root.join(title).join(chapter))
    }

    fn reload_lists(&mut self) {
        self.titles = list_titles(&self.projects_root).unwrap_or_default();
        self.selected_title = self.titles.first().cloned();
        self.reload_chapters_for_selected_title();
    }

    fn reload_chapters_for_selected_title(&mut self) {
        let Some(title) = self.selected_title.clone() else {
            self.chapters.clear();
            self.selected_chapter = None;
            self.state = ChooserUiState::Invalid {
                message: format!(
                    "Не найдено тайтлов в папке '{}'.",
                    self.projects_root.display()
                ),
            };
            return;
        };

        self.chapters = list_chapters(&self.projects_root, &title).unwrap_or_default();
        self.selected_chapter = self.chapters.first().cloned();
        if let Some(project_dir) = self.selected_project_dir() {
            self.start_validation(project_dir);
        } else {
            self.state = ChooserUiState::Invalid {
                message: "Для выбранного тайтла не найдено глав.".to_string(),
            };
        }
    }

    fn start_validation(&mut self, project_dir: PathBuf) {
        let (tx, rx) = mpsc::channel();
        self.state = ChooserUiState::Validating;
        self.pending_validation = Some(rx);

        thread::spawn(move || {
            let state = validate_project_dir_for_startup(&project_dir);
            let _ = tx.send(ProjectValidationResult { project_dir, state });
        });
    }

    fn poll_validation(&mut self) {
        let mut should_clear_receiver = false;
        if let Some(rx) = &self.pending_validation {
            match rx.try_recv() {
                Ok(result) => {
                    should_clear_receiver = true;
                    if self.selected_project_dir().as_ref() == Some(&result.project_dir) {
                        self.state = match result.state {
                            ProjectValidationState::Valid { image_count } => {
                                ChooserUiState::Ready { image_count }
                            }
                            ProjectValidationState::Invalid { message } => {
                                ChooserUiState::Invalid { message }
                            }
                        };
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    should_clear_receiver = true;
                    self.state = ChooserUiState::Invalid {
                        message: "Проверка папки завершилась ошибкой.".to_string(),
                    };
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        if should_clear_receiver {
            self.pending_validation = None;
        }
    }

    fn can_start(&self) -> bool {
        matches!(self.state, ChooserUiState::Ready { .. }) && self.selected_project_dir().is_some()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl eframe::App for BasicLauncherApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35: `App::ui` receives the window-root `Ui`. Keep a borrowed `Context` handle
        // for the context-level `send_viewport_cmd` call inside the panel body below.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.poll_validation();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("Базовый лаунчер");
                ui.add_space(8.0);
                ui.label("Выберите тайтл и главу для открытия.");
                ui.add_space(12.0);

                ui.horizontal(|ui| {
                    ui.label("Тайтл:");
                    let mut changed_title = false;
                    WheelComboBox::from_id_salt("basic_launcher_title")
                        .width(320.0)
                        .selected_text(self.selected_title.as_deref().unwrap_or("—"))
                        .show_ui(ui, |ui| {
                            for title in &self.titles {
                                if ui
                                    .selectable_value(
                                        &mut self.selected_title,
                                        Some(title.clone()),
                                        title,
                                    )
                                    .changed()
                                {
                                    changed_title = true;
                                }
                            }
                        });
                    if ui.button("Обновить").clicked() {
                        self.reload_lists();
                    }
                    if changed_title {
                        self.reload_chapters_for_selected_title();
                    }
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Глава:");
                    let mut changed_chapter = false;
                    WheelComboBox::from_id_salt("basic_launcher_chapter")
                        .width(320.0)
                        .selected_text(self.selected_chapter.as_deref().unwrap_or("—"))
                        .show_ui(ui, |ui| {
                            for chapter in &self.chapters {
                                if ui
                                    .selectable_value(
                                        &mut self.selected_chapter,
                                        Some(chapter.clone()),
                                        chapter,
                                    )
                                    .changed()
                                {
                                    changed_chapter = true;
                                }
                            }
                        });
                    if changed_chapter && let Some(project_dir) = self.selected_project_dir() {
                        self.start_validation(project_dir);
                    }
                });

                ui.add_space(10.0);
                if let Some(path) = self.selected_project_dir() {
                    ui.monospace(path.display().to_string());
                } else {
                    ui.label("Глава не выбрана.");
                }

                ui.add_space(8.0);
                match &self.state {
                    ChooserUiState::Empty => {
                        ui.label("Выберите тайтл и главу.");
                    }
                    ChooserUiState::Validating => {
                        ui.label("Проверка структуры папки...");
                    }
                    ChooserUiState::Ready { image_count } => {
                        ui.colored_label(
                            egui::Color32::from_rgb(120, 210, 120),
                            format!("Готово: найдено {} изображений в src.", image_count),
                        );
                    }
                    ChooserUiState::Invalid { message } => {
                        ui.colored_label(egui::Color32::from_rgb(230, 120, 120), message);
                    }
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let launch_btn =
                        ui.add_enabled(self.can_start(), egui::Button::new("Запустить"));
                    if launch_btn.clicked() {
                        if let Some(path) = self.selected_project_dir()
                            && let Ok(mut out) = self.output_project_dir.lock()
                        {
                            *out = Some(path);
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn pick_project_dir_from_basic_launcher_gui(
    projects_root: PathBuf,
) -> anyhow::Result<Option<PathBuf>> {
    let selected = Arc::new(Mutex::new(None::<PathBuf>));
    let out = Arc::clone(&selected);

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([620.0, 260.0])
        .with_min_inner_size([520.0, 220.0])
        .with_resizable(true)
        .with_app_id(BASIC_LAUNCHER_WINDOW_APP_ID);
    if let Some(icon) = load_embedded_icon_data() {
        viewport = viewport.with_icon(icon);
    }

    let chooser_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "ManhwaStudio - Базовый лаунчер",
        chooser_options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(BasicLauncherApp::new(
                projects_root.clone(),
                Arc::clone(&out),
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let selected = selected
        .lock()
        .map_err(|_| anyhow::anyhow!("failed to read selected project directory"))?
        .clone();
    Ok(selected)
}

pub(crate) fn validate_project_dir_for_startup(project_dir: &Path) -> ProjectValidationState {
    let src_dir = project_dir.join(config::SRC_DIR);
    if !src_dir.is_dir() {
        let scr_dir = project_dir.join("scr");
        if scr_dir.is_dir() {
            if let Err(err) = std::fs::rename(&scr_dir, &src_dir) {
                return ProjectValidationState::Invalid {
                    message: format!(
                        "Папка '{}' не подходит: найдена директория scr, но не удалось переименовать её в src: {}",
                        project_dir.display(),
                        err
                    ),
                };
            }
        } else {
            return ProjectValidationState::Invalid {
                message: format!(
                    "Папка '{}' не подходит: внутри нет директории src.",
                    project_dir.display()
                ),
            };
        }
    }

    match count_images_in_dir(&src_dir) {
        Ok(0) => ProjectValidationState::Invalid {
            message: format!(
                "Папка '{}' не подходит: в src нет изображений.",
                project_dir.display()
            ),
        },
        Ok(image_count) => ProjectValidationState::Valid { image_count },
        Err(err) => ProjectValidationState::Invalid {
            message: format!("Не удалось проверить '{}': {}", src_dir.display(), err),
        },
    }
}

pub(crate) fn count_images_in_dir(dir: &Path) -> std::io::Result<usize> {
    let mut count = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
            count += 1;
        }
    }
    Ok(count)
}

pub(crate) fn list_titles(projects_root: &Path) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(projects_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        out.push(name.to_string());
    }
    out.sort();
    Ok(out)
}

pub(crate) fn list_chapters(projects_root: &Path, title: &str) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    let title_dir = projects_root.join(title);
    for entry in std::fs::read_dir(title_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if name == "characters" {
            continue;
        }
        out.push(name.to_string());
    }
    out.sort();
    Ok(out)
}

#[cfg(not(target_arch = "wasm32"))]
fn load_embedded_icon_data() -> Option<egui::IconData> {
    let image = image::load_from_memory(EMBEDDED_APP_ICON_ICO)
        .or_else(|_| image::load_from_memory(EMBEDDED_APP_ICON_PNG))
        .ok()?;
    let rgba = image.into_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}

#[cfg(target_os = "linux")]
fn install_linux_desktop_integration_async() {
    let _ = thread::Builder::new()
        .name("desktop-integration-installer".to_string())
        .spawn(|| {
            let _ = install_linux_desktop_integration();
        });
}

#[cfg(target_os = "linux")]
fn install_linux_desktop_integration() -> std::io::Result<()> {
    let home = match env::var_os("HOME") {
        Some(v) => PathBuf::from(v),
        None => return Ok(()),
    };

    let apps_dir = home.join(".local/share/applications");
    let icon_dir = home.join(".local/share/icons/hicolor/512x512/apps");
    fs::create_dir_all(&apps_dir)?;
    fs::create_dir_all(&icon_dir)?;

    let icon_path = icon_dir.join("manhwastudio_rs.png");
    fs::write(&icon_path, EMBEDDED_APP_ICON_PNG)?;

    let exec_path = match env::current_exe() {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    let desktop_entry = format!(
        "[Desktop Entry]\n\
Type=Application\n\
Name=ManhwaStudio\n\
Comment=ManhwaStudio Rust Prototype\n\
Exec={} %u\n\
Icon=manhwastudio_rs\n\
Terminal=false\n\
Categories=Graphics;\n\
StartupNotify=true\n\
StartupWMClass=manhwastudio_rs\n\
X-KDE-DBUS-Restricted-Interfaces=org.kde.kwin.Screenshot,org.kde.KWin.ScreenShot2\n",
        escape_desktop_exec_arg(&exec_path)
    );
    fs::write(
        apps_dir.join("manhwastudio_rs.desktop"),
        desktop_entry.as_bytes(),
    )?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn escape_desktop_exec_arg(path: &Path) -> String {
    let mut out = String::with_capacity(path.as_os_str().len() + 2);
    out.push('"');
    for ch in path.to_string_lossy().chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}
