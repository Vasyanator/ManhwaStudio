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
  archive refresh;
- compare the running program's version (`HostVersion::version`, i.e. `MS_APP_VERSION`) with the
  tag of the release `release::fetch_latest_app_release` selects (the release feed and its
  selection live in `release.rs`). Versions are ordered only by
  `ms_config::version_format::compare_release_versions` / `is_remote_newer`.

Notes:
Updater work runs on background workers. The GUI thread only polls channels and draws progress or
choice state. Every worker shares the window's cancel flag, which `Drop for UpdateApp` sets when the
window closes; downloads, release requests and command retry waits stop on it.
*/

use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use ms_thread as thread;
use web_time::Duration;

use ms_config as config;
use ms_config::version_format::is_remote_newer;

use crate::HostVersion;
use crate::window::prepare_window_context;

use super::install::TorchInstallSelection;
use super::release::fetch_latest_app_release;
use super::utils::{
    ExternalUpdateTarget, UpdateWorkerEvent, load_embedded_icon_data,
    run_external_update_binary_stage, run_update_binary_stage, run_update_continuation_stage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateWindowOutcome {
    Exit,
    LaunchLauncher,
}

/// Opens the update window for the running copy. `host` carries the application's own
/// version, which this crate cannot read for itself (see [`HostVersion`]).
pub fn run_update_window(force_update_available: bool, host: HostVersion) -> Result<UpdateWindowOutcome, String> {
    run_update_window_internal(
        UpdateMode::Initial {
            force_update_available,
        },
        host,
    )
}

/// Resumes update work after the executable has been replaced. `host` as above.
pub fn run_continue_update_window(host: HostVersion) -> Result<UpdateWindowOutcome, String> {
    run_update_window_internal(UpdateMode::Continue, host)
}

/// Updates an install found elsewhere on disk (`target`), not the running copy.
/// `host` is still the RUNNING copy's version — it is what the window displays.
pub fn run_external_install_update_window(
    target: ExternalUpdateTarget,
    host: HostVersion,
) -> Result<UpdateWindowOutcome, String> {
    run_update_window_internal(UpdateMode::External { target }, host)
}

fn run_update_window_internal(mode: UpdateMode, host: HostVersion) -> Result<UpdateWindowOutcome, String> {
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
            prepare_window_context(&cc.egui_ctx);
            Ok(Box::new(UpdateApp::new(mode, output_for_app, host)))
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
    /// The running program's version (`MS_APP_VERSION`): shown to the user and compared with
    /// the selected release's tag.
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
    /// Cancel flag shared with every worker this window starts (release check, binary and
    /// continuation stages); set when the window is dropped, i.e. when it closes, so a
    /// download or a pip retry wait stops instead of running on behind a closed window.
    cancel: Arc<AtomicBool>,
}

impl Drop for UpdateApp {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl UpdateApp {
    fn new(mode: UpdateMode, output: Arc<Mutex<UpdateWindowOutcome>>, host: HostVersion) -> Self {
        let mut app = Self {
            local_version: host.version.to_string(),
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
            cancel: Arc::new(AtomicBool::new(false)),
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
        let cancel = Arc::clone(&self.cancel);
        let (tx, rx) = mpsc::channel();
        self.pending_check = Some(rx);
        self.page = UpdatePage::Check;
        self.state = UpdateState::Checking;

        if let Err(err) = thread::Builder::new()
            .name("update-window-version-check".to_string())
            .spawn(move || {
                let result =
                    fetch_latest_app_release_tag(&cancel).map(|remote_version| UpdateCheckResult {
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

        let cancel = Arc::clone(&self.cancel);
        let _ = thread::Builder::new()
            .name("update-binary-stage".to_string())
            .spawn(move || run_update_binary_stage(root_dir, &tx, &cancel));
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

        let cancel = Arc::clone(&self.cancel);
        let _ = thread::Builder::new()
            .name("external-update-binary-stage".to_string())
            .spawn(move || run_external_update_binary_stage(target, &tx, &cancel));
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

        let cancel = Arc::clone(&self.cancel);
        // The running (freshly installed) binary's version selects the release whose archive
        // the stage installs.
        let own_version = self.local_version.clone();
        let _ = thread::Builder::new()
            .name("update-continuation-stage".to_string())
            .spawn(move || run_update_continuation_stage(root_dir, &own_version, torch_selection, &tx, &cancel));
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
                        ms_theme::status::SUCCESS,
                        tf!("installer.update.new_version_available_status", remote_version = remote_version),
                    );
                    ui.add_space(14.0);
                    if ui.button(t!("installer.update.start_update_button")).clicked() {
                        self.start_binary_update();
                    }
                }
                UpdateState::Error { message } => {
                    ui.colored_label(ms_theme::status::ERROR, message);
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
                ui.colored_label(ms_theme::status::ERROR, message);
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

/// The tag of the newest updatable release (`release::select_app_release`: archive, this
/// platform's binary and `SHA256SUMS`). `cancel` stops the request and its retry waits
/// (window closed).
fn fetch_latest_app_release_tag(cancel: &AtomicBool) -> Result<String, String> {
    fetch_latest_app_release("update-window", cancel).map(|release| release.tag)
}
