/*
File: update.rs

Purpose:
Owns the Rust update window shown after the launcher hands control back to startup.

Main responsibilities:
- create the update viewport with the same native sizing parameters as the installer window;
- keep the update UI isolated from the launcher and startup routing;
- re-check release availability on entry without blocking the GUI thread;
- support the startup test-version override without performing network I/O;
- run the updater in two stages: executable replacement, then `--continue-update` environment and
  archive refresh.

Notes:
Updater work runs on background workers. The GUI thread only polls channels and draws progress or
choice state.
*/

use eframe::egui;
use std::cmp::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use ms_thread as thread;
use web_time::Duration;

use crate::config;

use super::install::TorchInstallSelection;
use super::utils::{
    ExternalUpdateTarget, UpdateWorkerEvent, github_api_get, load_embedded_icon_data,
    run_external_update_binary_stage, run_update_binary_stage, run_update_continuation_stage,
};

const APP_RELEASES_API: &str = "https://api.github.com/repos/Vasyanator/ManhwaStudio/releases";
const APP_ZIP_ASSET_NAME: &str = "ManhwaStudio.zip";
const LINUX_BINARY_ASSET_NAME: &str = "manhwastudio_rs";
const WINDOWS_BINARY_ASSET_NAME: &str = "manhwastudio_rs.exe";
// The macOS build (`build-macos.sh`) produces a bare `manhwastudio_rs` binary, which
// would collide with the Linux release asset. Publish it under a distinct suffixed name
// so the updater can pick the correct per-platform asset from the same GitHub release.
const MACOS_BINARY_ASSET_NAME: &str = "manhwastudio_rs_macos";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateWindowOutcome {
    Exit,
    LaunchLauncher,
}

pub fn run_update_window(force_update_available: bool) -> Result<UpdateWindowOutcome, String> {
    run_update_window_internal(UpdateMode::Initial {
        force_update_available,
    })
}

pub fn run_continue_update_window() -> Result<UpdateWindowOutcome, String> {
    run_update_window_internal(UpdateMode::Continue)
}

pub fn run_external_install_update_window(
    target: ExternalUpdateTarget,
) -> Result<UpdateWindowOutcome, String> {
    run_update_window_internal(UpdateMode::External { target })
}

fn run_update_window_internal(mode: UpdateMode) -> Result<UpdateWindowOutcome, String> {
    let output = Arc::new(Mutex::new(UpdateWindowOutcome::Exit));
    let output_for_app = Arc::clone(&output);
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
        t!("installer.update.window_title"),
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(UpdateApp::new(mode, output_for_app)))
        }),
    )
    .map_err(|e| e.to_string())?;

    let outcome = output
        .lock()
        .map_err(|_| t!("installer.update.no_update_result_error").to_string())?;
    Ok(*outcome)
}

#[derive(Debug)]
struct UpdateCheckResult {
    remote_version: String,
    update_available: bool,
}

#[derive(Debug)]
enum UpdatePage {
    Check,
    Running,
    TorchChoice,
    Completed,
}

#[derive(Debug)]
enum UpdateState {
    Checking,
    NoUpdate,
    Available { remote_version: String },
    Running,
    WaitingForTorchChoice,
    Relaunching,
    Completed,
    Error { message: String },
}

#[derive(Debug)]
enum UpdateMode {
    Initial { force_update_available: bool },
    Continue,
    External { target: ExternalUpdateTarget },
}

struct UpdateApp {
    local_version: String,
    page: UpdatePage,
    state: UpdateState,
    pending_check: Option<mpsc::Receiver<Result<UpdateCheckResult, String>>>,
    worker_rx: Option<mpsc::Receiver<UpdateWorkerEvent>>,
    output: Arc<Mutex<UpdateWindowOutcome>>,
    stage_progress: f32,
    stage_label: String,
    overall_progress: f32,
    overall_label: String,
    current_operation: String,
    console_lines: Vec<String>,
    torch_prompt: Option<super::install::TorchChoicePrompt>,
    selected_torch_index: Option<usize>,
}

impl UpdateApp {
    fn new(mode: UpdateMode, output: Arc<Mutex<UpdateWindowOutcome>>) -> Self {
        let mut app = Self {
            local_version: env!("CARGO_PKG_VERSION").to_string(),
            page: UpdatePage::Check,
            state: UpdateState::Checking,
            pending_check: None,
            worker_rx: None,
            output,
            stage_progress: 0.0,
            stage_label: t!("installer.update.waiting_stage").to_string(),
            overall_progress: 0.0,
            overall_label: t!("installer.update.waiting_stage").to_string(),
            current_operation: t!("installer.update.checking_updates_stage").to_string(),
            console_lines: Vec::new(),
            torch_prompt: None,
            selected_torch_index: None,
        };
        match mode {
            UpdateMode::Initial {
                force_update_available,
            } => {
                if force_update_available {
                    app.state = UpdateState::Available {
                        remote_version: "test-update".to_string(),
                    };
                } else {
                    app.start_check();
                }
            }
            UpdateMode::Continue => {
                app.start_continuation_update(None);
            }
            UpdateMode::External { target } => {
                app.start_external_binary_update(target);
            }
        }
        app
    }

    fn start_check(&mut self) {
        let local_version = self.local_version.clone();
        let (tx, rx) = mpsc::channel();
        self.pending_check = Some(rx);
        self.page = UpdatePage::Check;
        self.state = UpdateState::Checking;

        if let Err(err) = thread::Builder::new()
            .name("update-window-version-check".to_string())
            .spawn(move || {
                let result =
                    fetch_latest_app_release_tag().map(|remote_version| UpdateCheckResult {
                        update_available: is_remote_newer(&local_version, &remote_version),
                        remote_version,
                    });
                let _ = tx.send(result);
            })
        {
            self.pending_check = None;
            self.state = UpdateState::Error {
                message: tf!("installer.update.start_check_error", err = err),
            };
        }
    }

    fn poll_check_result(&mut self) {
        let mut clear_receiver = false;
        if let Some(rx) = &self.pending_check {
            match rx.try_recv() {
                Ok(Ok(result)) => {
                    clear_receiver = true;
                    self.state = if result.update_available {
                        UpdateState::Available {
                            remote_version: result.remote_version,
                        }
                    } else {
                        UpdateState::NoUpdate
                    };
                }
                Ok(Err(err)) => {
                    clear_receiver = true;
                    self.state = UpdateState::Error { message: err };
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    clear_receiver = true;
                    self.state = UpdateState::Error {
                        message: t!("installer.update.check_failed_status").to_string(),
                    };
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if clear_receiver {
            self.pending_check = None;
        }
    }

    fn start_binary_update(&mut self) {
        let (tx, rx) = mpsc::channel();
        let root_dir = config::program_dir();
        self.worker_rx = Some(rx);
        self.page = UpdatePage::Running;
        self.state = UpdateState::Running;
        self.current_operation = t!("installer.update.downloading_executable_stage").to_string();
        self.stage_progress = 0.0;
        self.stage_label = t!("installer.common.preparation").to_string();
        self.overall_progress = 0.0;
        self.overall_label = t!("installer.update.updating_binary_stage").to_string();
        self.console_lines.clear();

        let _ = thread::Builder::new()
            .name("update-binary-stage".to_string())
            .spawn(move || run_update_binary_stage(root_dir, &tx));
    }

    fn start_external_binary_update(&mut self, target: ExternalUpdateTarget) {
        let (tx, rx) = mpsc::channel();
        self.worker_rx = Some(rx);
        self.page = UpdatePage::Running;
        self.state = UpdateState::Running;
        self.current_operation = t!("installer.update.checking_installed_copy_stage").to_string();
        self.stage_progress = 0.0;
        self.stage_label = t!("installer.update.getting_version_stage").to_string();
        self.overall_progress = 0.0;
        self.overall_label = tf!("installer.update.updating_target_status", target = target.root_dir.display());
        self.console_lines.clear();

        let _ = thread::Builder::new()
            .name("external-update-binary-stage".to_string())
            .spawn(move || run_external_update_binary_stage(target, &tx));
    }

    fn start_continuation_update(&mut self, torch_selection: Option<TorchInstallSelection>) {
        let (tx, rx) = mpsc::channel();
        let root_dir = config::program_dir();
        self.worker_rx = Some(rx);
        self.page = UpdatePage::Running;
        self.state = UpdateState::Running;
        self.current_operation = t!("installer.update.continue_update_stage").to_string();
        self.stage_progress = 0.0;
        self.stage_label = t!("installer.common.preparation").to_string();
        self.overall_progress = 0.0;
        self.overall_label = t!("installer.update.preparing_env_stage").to_string();
        self.torch_prompt = None;
        self.selected_torch_index = None;

        let _ = thread::Builder::new()
            .name("update-continuation-stage".to_string())
            .spawn(move || run_update_continuation_stage(root_dir, torch_selection, &tx));
    }

    fn poll_worker_events(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(rx) = &self.worker_rx {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }

        for event in events {
            match event {
                UpdateWorkerEvent::Step(text) => {
                    self.current_operation = text;
                }
                UpdateWorkerEvent::ConsoleLine(line) => {
                    self.console_lines.push(line);
                    if self.console_lines.len() > 2000 {
                        self.console_lines.drain(0..200);
                    }
                }
                UpdateWorkerEvent::Progress {
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
                UpdateWorkerEvent::TorchChoiceRequired(prompt) => {
                    self.page = UpdatePage::TorchChoice;
                    self.state = UpdateState::WaitingForTorchChoice;
                    self.current_operation = t!("installer.update.stage_choose_pytorch").to_string();
                    self.stage_label = t!("installer.update.choose_pytorch_backend_hint").to_string();
                    self.overall_label = prompt.summary.clone();
                    self.selected_torch_index = Some(prompt.recommended_index);
                    self.torch_prompt = Some(prompt);
                }
                UpdateWorkerEvent::NoUpdate {
                    local_version,
                    remote_version,
                } => {
                    self.page = UpdatePage::Check;
                    self.state = UpdateState::NoUpdate;
                    self.current_operation = t!("installer.update.no_update_needed_stage").to_string();
                    self.stage_progress = 1.0;
                    self.stage_label = tf!("installer.update.installed_version_label", local_version = local_version);
                    self.overall_progress = 1.0;
                    self.overall_label = tf!("installer.update.latest_release_label", remote_version = remote_version);
                }
                UpdateWorkerEvent::RelaunchStarted => {
                    self.state = UpdateState::Relaunching;
                    self.current_operation = t!("installer.update.restart_to_new_version_stage").to_string();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                UpdateWorkerEvent::Finished(Ok(())) => {
                    self.page = UpdatePage::Completed;
                    self.state = UpdateState::Completed;
                    self.current_operation = t!("installer.update.update_complete_stage").to_string();
                    self.stage_progress = 1.0;
                    self.stage_label = t!("installer.common.ready").to_string();
                    self.overall_progress = 1.0;
                    self.overall_label = t!("installer.update.can_exit_to_launcher").to_string();
                }
                UpdateWorkerEvent::Finished(Err(err)) => {
                    self.page = UpdatePage::Running;
                    self.state = UpdateState::Error {
                        message: err.clone(),
                    };
                    self.current_operation = t!("installer.update.update_error_stage").to_string();
                    self.stage_label = t!("installer.update.stage_failed").to_string();
                    self.overall_label = err;
                }
            }
        }
    }

    fn draw_check_page(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(44.0);
            ui.heading(t!("installer.update.window_title"));
            ui.add_space(8.0);
            ui.label(tf!("installer.update.current_version_label", arg = self.local_version));
            ui.add_space(18.0);

            match &self.state {
                UpdateState::Checking => {
                    ui.spinner();
                    ui.add_space(8.0);
                    ui.label(t!("installer.update.checking_update_status"));
                }
                UpdateState::NoUpdate => {
                    ui.label(t!("installer.update.no_updates_status"));
                    ui.add_space(14.0);
                    if ui.button(t!("installer.update.exit_button")).clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                UpdateState::Available { remote_version } => {
                    ui.colored_label(
                        egui::Color32::from_rgb(120, 220, 120),
                        tf!("installer.update.new_version_available_status", remote_version = remote_version),
                    );
                    ui.add_space(14.0);
                    if ui.button(t!("installer.update.start_update_button")).clicked() {
                        self.start_binary_update();
                    }
                }
                UpdateState::Error { message } => {
                    ui.colored_label(egui::Color32::from_rgb(235, 125, 125), message);
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        if ui.button(t!("installer.update.retry_check_button")).clicked() {
                            self.start_check();
                        }
                        if ui.button(t!("installer.update.exit_button")).clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                UpdateState::Running
                | UpdateState::WaitingForTorchChoice
                | UpdateState::Relaunching
                | UpdateState::Completed => {}
            }
        });
    }

    fn draw_running_page(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.heading(t!("installer.update.window_title"));
            ui.add_space(10.0);
            ui.label(&self.current_operation);
            ui.add_space(8.0);
            ui.label(&self.stage_label);
            ui.add(egui::ProgressBar::new(self.stage_progress).show_percentage());
            ui.add_space(8.0);
            ui.label(&self.overall_label);
            ui.add(egui::ProgressBar::new(self.overall_progress).show_percentage());
            ui.add_space(12.0);

            if let UpdateState::Error { message } = &self.state {
                ui.colored_label(egui::Color32::from_rgb(235, 125, 125), message);
                ui.add_space(8.0);
                if ui.button(t!("installer.update.exit_button")).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.console_lines {
                        ui.monospace(line);
                    }
                });
        });
    }

    fn draw_torch_choice_page(&mut self, ui: &mut egui::Ui) {
        let Some(prompt) = self.torch_prompt.clone() else {
            self.start_continuation_update(Some(TorchInstallSelection::SkipCpu));
            return;
        };

        ui.vertical(|ui| {
            ui.heading(t!("installer.update.pytorch_update_label"));
            ui.add_space(8.0);
            ui.label(prompt.summary);
            ui.add_space(12.0);

            let cpu_selected = self.selected_torch_index.is_none();
            if ui
                .selectable_label(cpu_selected, "CPU PyTorch")
                .on_hover_text(t!("installer.update.install_cpu_pytorch_button"))
                .clicked()
            {
                self.selected_torch_index = None;
            }

            for (idx, option) in prompt.options.iter().enumerate() {
                let selected = self.selected_torch_index == Some(idx);
                if ui
                    .selectable_label(selected, &option.label)
                    .on_hover_text(format!("PyTorch wheel tag: {}", option.wheel_tag))
                    .clicked()
                {
                    self.selected_torch_index = Some(idx);
                }
            }

            ui.add_space(14.0);
            if ui.button(t!("installer.update.continue_update_button")).clicked() {
                let selection = self
                    .selected_torch_index
                    .and_then(|idx| prompt.options.get(idx).cloned())
                    .map(TorchInstallSelection::InstallGpu)
                    .unwrap_or(TorchInstallSelection::SkipCpu);
                self.start_continuation_update(Some(selection));
            }
        });
    }

    fn draw_completed_page(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(70.0);
            ui.heading(t!("installer.update.update_complete_stage"));
            ui.add_space(12.0);
            ui.label(t!("installer.update.can_exit_to_launcher_period"));
            ui.add_space(18.0);
            if ui.button(t!("installer.update.exit_to_launcher_button")).clicked() {
                if let Ok(mut outcome) = self.output.lock() {
                    *outcome = UpdateWindowOutcome::LaunchLauncher;
                }
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

impl eframe::App for UpdateApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35: `App::ui` receives the window-root `Ui`; keep a borrowed `Context` handle for
        // worker polling / repaint scheduling, and build the root `CentralPanel` on `ui` below.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.poll_check_result();
        self.poll_worker_events(ctx);

        egui::CentralPanel::default().show(ui, |ui| match self.page {
            UpdatePage::Check => self.draw_check_page(ui),
            UpdatePage::Running => self.draw_running_page(ui),
            UpdatePage::TorchChoice => self.draw_torch_choice_page(ui),
            UpdatePage::Completed => self.draw_completed_page(ui),
        });

        if matches!(self.state, UpdateState::Checking | UpdateState::Running) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

fn fetch_latest_app_release_tag() -> Result<String, String> {
    // Shared installer helper: retries transient network failures with backoff.
    let body = github_api_get(APP_RELEASES_API, "ManhwaStudio/update-window")
        .map_err(|e| tf!("installer.update.get_releases_error", e = e))?;
    let releases: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| tf!("installer.update.parse_releases_error", e = e))?;
    let releases = releases
        .as_array()
        .ok_or_else(|| t!("installer.update.unexpected_releases_format_error").to_string())?;

    for release in releases {
        let tag = release
            .get("tag_name")
            .and_then(|value| value.as_str())
            .or_else(|| release.get("name").and_then(|value| value.as_str()))
            .unwrap_or("")
            .trim();
        if tag.is_empty() {
            continue;
        }

        let has_required_assets = release
            .get("assets")
            .and_then(|value| value.as_array())
            .map(|assets| {
                let has_zip = assets.iter().any(|asset| {
                    asset.get("name").and_then(|value| value.as_str()) == Some(APP_ZIP_ASSET_NAME)
                });
                let has_binary = assets.iter().any(|asset| {
                    asset.get("name").and_then(|value| value.as_str())
                        == Some(platform_binary_asset_name())
                });
                has_zip && has_binary
            })
            .unwrap_or(false);
        if has_required_assets {
            return Ok(tag.to_string());
        }
    }

    Err(tf!("installer.update.no_release_with_assets_error", app_zip_asset_name = APP_ZIP_ASSET_NAME, platform_binary_asset_name = platform_binary_asset_name()))
}

/// Returns the GitHub release asset name for the current platform's executable.
///
/// Windows uses `manhwastudio_rs.exe`, macOS uses the suffixed
/// `manhwastudio_rs_macos` (to avoid colliding with the Linux asset), and every
/// other target (Linux) uses the bare `manhwastudio_rs`.
fn platform_binary_asset_name() -> &'static str {
    if cfg!(target_os = "windows") {
        WINDOWS_BINARY_ASSET_NAME
    } else if cfg!(target_os = "macos") {
        MACOS_BINARY_ASSET_NAME
    } else {
        LINUX_BINARY_ASSET_NAME
    }
}

fn is_remote_newer(local: &str, remote: &str) -> bool {
    compare_versions(remote, local).is_gt()
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    let left_parts = parse_version_to_parts(left);
    let right_parts = parse_version_to_parts(right);

    for (left_part, right_part) in left_parts.iter().zip(right_parts.iter()) {
        let ordering = compare_version_part(left_part, right_part);
        if !ordering.is_eq() {
            return ordering;
        }
    }

    left_parts.len().cmp(&right_parts.len())
}

fn parse_version_to_parts(version: &str) -> Vec<VersionPart> {
    normalize_version(version)
        .split(['.', '-', '+', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| match part.parse::<u64>() {
            Ok(number) => VersionPart::Number(number),
            Err(_) => VersionPart::Text(part.to_ascii_lowercase()),
        })
        .collect()
}

fn normalize_version(version: &str) -> &str {
    let trimmed = version.trim();
    trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed)
}

fn compare_version_part(left: &VersionPart, right: &VersionPart) -> Ordering {
    match (left, right) {
        (VersionPart::Number(left), VersionPart::Number(right)) => left.cmp(right),
        (VersionPart::Text(left), VersionPart::Text(right)) => left.cmp(right),
        (VersionPart::Number(_), VersionPart::Text(_)) => Ordering::Greater,
        (VersionPart::Text(_), VersionPart::Number(_)) => Ordering::Less,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum VersionPart {
    Number(u64),
    Text(String),
}

#[cfg(test)]
mod tests {
    use super::{
        LINUX_BINARY_ASSET_NAME, MACOS_BINARY_ASSET_NAME, WINDOWS_BINARY_ASSET_NAME,
        compare_versions, is_remote_newer,
    };
    use std::cmp::Ordering;

    #[test]
    fn platform_binary_asset_names_are_distinct_and_non_empty() {
        // `platform_binary_asset_name()` selects between these at compile time via
        // `cfg!`, so verify the underlying constants directly: each must be a distinct,
        // non-empty asset name so the updater never downloads the wrong platform build.
        let names = [
            LINUX_BINARY_ASSET_NAME,
            MACOS_BINARY_ASSET_NAME,
            WINDOWS_BINARY_ASSET_NAME,
        ];
        assert!(names.iter().all(|name| !name.is_empty()));
        for (i, left) in names.iter().enumerate() {
            for right in &names[i + 1..] {
                assert_ne!(left, right);
            }
        }
    }

    #[test]
    fn compares_numeric_release_versions() {
        assert_eq!(compare_versions("v3.5.0", "3.4.9"), Ordering::Greater);
        assert_eq!(compare_versions("3.4.0", "v3.4.0"), Ordering::Equal);
        assert_eq!(compare_versions("3.3.9", "3.4.0"), Ordering::Less);
    }

    #[test]
    fn detects_remote_update_against_local_version() {
        assert!(is_remote_newer("3.4.0", "v3.4.1"));
        assert!(!is_remote_newer("3.4.0", "v3.4.0"));
        assert!(!is_remote_newer("3.4.1", "v3.4.0"));
    }
}
