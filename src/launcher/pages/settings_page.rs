/*
File: src/launcher/pages/settings_page.rs

Purpose:
Launcher settings page for global launcher options.

Main responsibilities:
- render the Rust launcher settings card in the same shell/theme as other pages;
- split launcher settings into tabs without blocking the fullscreen page shell;
- edit and persist the projects root stored in `user_config.json`;
- show system CPU/RAM/core and accelerator information from a background probe;
- probe AI Python packages through the shared startup/settings probe path;
- reconcile `General.ai_install_type` to `Full` when PyTorch is actually importable;
- run the launcher-side PyTorch/full-dependency upgrade flow through installer backend helpers;
- host a background-driven shell console for the detected Python environment;
- keep `pip` console commands usable via the active env or `uv pip` fallback;
- notify the launcher runtime when the projects root changes so dependent pages refresh.

Notes:
Config edits stay synchronous because they are tiny, but the Python environment console runs in
background worker threads so the launcher UI never blocks on shell I/O.

The tab set, ordering, tab labels, and the shared General/AiBackend/Tutorials sections come from the
shared section registry (`crate::settings_shared`): `active_tab` is a `SettingsSectionId`, the tab
bar iterates `sections_for(SettingsSurface::Launcher)`, and the shared panels are owned as one
`SharedSettingsPanels`. The launcher-exclusive sections (SystemInfo/AiComputations/TorchUpgrade/
PythonEnvironment) keep their local renderers here; the dynamic TorchUpgrade hide/relabel logic is
applied inline in the tab bar.
*/

use crate::ai_backend_supervisor::AiBackendHandle;
use crate::ai_install_probe::{
    AiComputationsReport, AiPackageProbe, detect_ai_install_type_from_report,
    spawn_ai_computations_probe,
};
use crate::config;
// GPU/system diagnostics types + probes. `gpu_utils` compiles on wasm with the
// command primitive stubbed, so on web the system-information tab renders and
// simply reports "nothing detected"; the import is target-neutral.
use crate::gpu_utils::{
    DirectMlAccelerator, GpuArchitecture, LinuxDriverStatus, RocmInstallationStatus,
    RocmSupportValidation, RuntimeVersion, detect_amd_gpu, detect_amd_gpu_architectures_linux,
    detect_apple_gpu, detect_cuda_runtime_version, detect_directml_accelerators_windows,
    detect_nvidia_compute_capability, detect_nvidia_gpu, detect_nvidia_gpu_architecture,
    detect_rocm_installation_linux, detect_rocm_runtime_version, linux_driver_status,
    rocm_7_2_supported_llvm_targets, validate_rocm_7_2_support_linux,
};
// Installer types/helpers drive the native PyTorch/full-dependency upgrade
// flow. The installer subsystem is desktop-only, so these are gated to native;
// on web the whole Torch-upgrade tab is a stub.
#[cfg(not(target_arch = "wasm32"))]
use crate::installer::install::{
    InstallEvent, TorchChoicePrompt, TorchInstallSelection, TorchPreflightResult,
};
#[cfg(not(target_arch = "wasm32"))]
use crate::installer::utils;
use crate::launcher::pages::base::{self, PageNavAction};
use crate::launcher::theme;
use crate::settings_shared::{
    SettingsSectionId, SettingsSurface, SharedSettingsPanels, sections_for, title_key,
};
#[cfg(feature = "tutorial")]
use crate::tutorial::TutorialProgressHandle;
// Used only by the native Python-environment console (shell spawning); gated to
// native alongside it.
#[cfg(not(target_arch = "wasm32"))]
use crate::python_manager::{self, PythonShellKind};
use crate::runtime_log;
// Only used to timestamp exported log filenames in the native save flow.
#[cfg(not(target_arch = "wasm32"))]
use chrono::Local;
use egui::{
    Align, Align2, Area, Color32, CornerRadius, FontId, Frame, Layout, Order, RichText, ScrollArea,
    Sense, Stroke, Ui, Vec2,
};
// `Key`/`TextEdit`/`TextStyle` are used only by the native Python-console tab.
#[cfg(not(target_arch = "wasm32"))]
use egui::{Key, TextEdit, TextStyle};
// Native folder picker for the projects-root field; no OS dialog on web.
#[cfg(not(target_arch = "wasm32"))]
use rfd::FileDialog;
use serde_json::Value;
#[cfg(target_os = "linux")]
use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::fs;
// I/O traits used only by the native Python-environment console threads.
#[cfg(not(target_arch = "wasm32"))]
use std::io::{BufRead, BufReader, BufWriter, Write};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
// `Path` is only referenced by native folder-picking and shell-path helpers.
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
// Process spawning for the native Python console; unavailable on web.
#[cfg(not(target_arch = "wasm32"))]
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
// `Sender` is used only by the native console channels.
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::Sender;
use ms_thread as thread;

const STATUS_ERROR: Color32 = Color32::from_rgb(214, 104, 104);
const TAB_ACTIVE_FILL: Color32 = Color32::from_rgba_premultiplied(72, 72, 78, 176);
const TAB_IDLE_FILL: Color32 = theme::BUTTON_FILL;
const TAB_STROKE: Color32 = theme::BUTTON_STROKE;
const TAB_WARNING_FILL: Color32 = Color32::from_rgba_premultiplied(120, 88, 18, 188);
const TAB_WARNING_STROKE: Color32 = Color32::from_rgba_premultiplied(236, 197, 76, 170);
const SETTINGS_CARD_EDGE_GAP: f32 = 18.0;
// Layout constants for the native Python-console tab only.
#[cfg(not(target_arch = "wasm32"))]
const CONSOLE_MIN_HEIGHT: f32 = 320.0;
#[cfg(not(target_arch = "wasm32"))]
const CONSOLE_INPUT_ROWS: usize = 2;
// The Python-environment console spawns a native OS shell; it has no web
// equivalent, so its state types are compiled out on wasm.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
enum PythonConsoleEvent {
    Output(String),
    Error(String),
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct PythonConsoleRuntime {
    child: Child,
    command_tx: Sender<String>,
    event_rx: Receiver<PythonConsoleEvent>,
    terminated: bool,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
struct PythonConsoleState {
    output: String,
    input: String,
    runtime: Option<PythonConsoleRuntime>,
    attempted_start: bool,
}

pub struct SettingsPageState {
    /// Currently selected settings section. Sourced from the shared section
    /// registry (`crate::settings_shared`); only sections listed for
    /// `SettingsSurface::Launcher` can ever be active here.
    active_tab: SettingsSectionId,
    /// The three shared "double-interface" panel states (General / AiBackend /
    /// Tutorials), owned as ONE instance so the launcher renders exactly the same
    /// widgets as the studio settings tab. See `crate::settings_shared`.
    shared: SharedSettingsPanels,
    // Native Python-environment console; no OS shell on web.
    #[cfg(not(target_arch = "wasm32"))]
    python_console: PythonConsoleState,
    ai_probe: AiComputationsProbeState,
    system_info_probe: SystemInfoProbeState,
    ai_install_type: config::AiInstallType,
    // Native PyTorch upgrade flow driven by the desktop installer.
    #[cfg(not(target_arch = "wasm32"))]
    torch_upgrade: TorchUpgradeState,
    log_popup_open: bool,
    /// Shared app-global backend handle, passed by reference into the shared
    /// `AiBackend` panel each frame so the launcher exposes the same backend
    /// controls as the studio settings tab.
    ai_backend: AiBackendHandle,
}

#[derive(Debug, Clone, Copy)]
enum LogKind {
    Current,
    Previous,
}

#[derive(Debug, Default)]
struct AiComputationsProbeState {
    status: AiProbeStatus,
    rx: Option<Receiver<Result<AiComputationsReport, String>>>,
}

#[derive(Debug, Default)]
struct SystemInfoProbeState {
    status: SystemInfoStatus,
    rx: Option<Receiver<Result<SystemInfoReport, String>>>,
}

// Torch-upgrade state carries installer events; the installer subsystem is
// desktop-only, so this and its status enum are compiled out on wasm.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
struct TorchUpgradeState {
    status: TorchUpgradeStatus,
    rx: Option<Receiver<InstallEvent>>,
    pending_ai_install_type_action: Option<config::AiInstallType>,
    stage_progress: f32,
    stage_label: String,
    overall_progress: f32,
    overall_label: String,
    console_lines: Vec<String>,
}

#[derive(Debug, Default)]
enum AiProbeStatus {
    #[default]
    Idle,
    Running,
    Ready(AiComputationsReport),
    Error(String),
}

#[derive(Debug, Default)]
enum SystemInfoStatus {
    #[default]
    Idle,
    Running,
    Ready(Box<SystemInfoReport>),
    Error(String),
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default, Clone)]
enum TorchUpgradeStatus {
    #[default]
    Idle,
    Preparing,
    Choice(TorchChoicePrompt),
    Running,
    Completed,
    Error(String),
}

#[derive(Debug, Clone)]
struct SystemInfoReport {
    cpu: CpuInfoReport,
    memory: MemoryInfoReport,
    gpu: GpuInfoReport,
}

#[derive(Debug, Clone)]
struct CpuInfoReport {
    name: String,
    physical_cores: Option<usize>,
    logical_cores: usize,
}

#[derive(Debug, Clone)]
struct MemoryInfoReport {
    total_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
struct GpuInfoReport {
    nvidia_detected: bool,
    amd_detected: bool,
    cuda_version: Option<RuntimeVersion>,
    nvidia_compute_capability: Option<RuntimeVersion>,
    nvidia_architecture: Option<GpuArchitecture>,
    rocm_version: Option<RuntimeVersion>,
    linux_driver_status: Option<LinuxDriverStatus>,
    rocm_installation: Option<RocmInstallationStatus>,
    amd_architectures: Vec<GpuArchitecture>,
    rocm_validation: Option<RocmSupportValidation>,
    directml_accelerators: Vec<DirectMlAccelerator>,
    apple_gpu: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum AiPackageStatusView<'a> {
    Checking,
    Torch(&'a AiPackageProbe),
    OnnxRuntime(&'a AiPackageProbe),
}

impl SettingsPageState {
    pub fn new(
        projects_root: PathBuf,
        ai_install_type: config::AiInstallType,
        ai_backend: AiBackendHandle,
        #[cfg(feature = "tutorial")] tutorial_progress: TutorialProgressHandle,
    ) -> Self {
        // Build the shared panel container, then seed its General widget from the
        // passed root so the page opens clean.
        let mut shared = SharedSettingsPanels::new(
            #[cfg(feature = "tutorial")]
            tutorial_progress,
        );
        shared.set_projects_root(&projects_root.to_string_lossy());
        Self {
            active_tab: SettingsSectionId::General,
            shared,
            #[cfg(not(target_arch = "wasm32"))]
            python_console: PythonConsoleState::default(),
            ai_probe: AiComputationsProbeState::default(),
            system_info_probe: SystemInfoProbeState::default(),
            ai_install_type,
            #[cfg(not(target_arch = "wasm32"))]
            torch_upgrade: TorchUpgradeState::default(),
            log_popup_open: false,
            ai_backend,
        }
    }

    pub fn set_projects_root(&mut self, projects_root: PathBuf) {
        self.shared
            .set_projects_root(&projects_root.to_string_lossy());
    }

    /// Terminates and resets the Python-environment console.
    ///
    /// Native only. On web there is no console to close, so the web twin is a
    /// no-op that keeps the launcher's call site target-agnostic.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn close_python_console(&mut self) {
        if let Some(runtime) = self.python_console.runtime.as_mut() {
            runtime.terminate();
        }
        self.python_console.runtime = None;
        self.python_console.attempted_start = false;
        self.python_console.input.clear();
        self.python_console.output.clear();
    }

    /// Web twin of `close_python_console`: no Python console exists on web.
    #[cfg(target_arch = "wasm32")]
    pub fn close_python_console(&mut self) {}

    pub fn set_ai_install_type(&mut self, ai_install_type: config::AiInstallType) {
        self.ai_install_type = ai_install_type;
        if ai_install_type == config::AiInstallType::None
            && self.active_tab == SettingsSectionId::TorchUpgrade
        {
            self.active_tab = SettingsSectionId::General;
        }
    }

    pub fn show(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        let mut action = None;
        let mut save_log_button_rect = None;
        if let Some(back_action) = base::show_page_shell(ui, |ui| {
            ui.add_space(16.0);
            let available_width = ui.available_width();
            let card_width = (available_width - SETTINGS_CARD_EDGE_GAP * 2.0).max(700.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(card_width);
                ui.set_min_height(420.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(t!("launcher.settings.heading")).size(24.0).strong());
                    ui.add_space(18.0);

                    save_log_button_rect = Some(self.show_tab_bar(ui));
                    ui.add_space(18.0);

                    ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| match self.active_tab {
                            // Shared sections render through the shared panel container.
                            // `Tutorials` produces no launcher outcome; `General` may
                            // report a saved projects root.
                            #[cfg(feature = "tutorial")]
                            id @ (SettingsSectionId::General
                            | SettingsSectionId::AiBackend
                            | SettingsSectionId::Tutorials) => {
                                let outcome = self.shared.draw(
                                    id,
                                    ui,
                                    SettingsSurface::Launcher,
                                    &self.ai_backend,
                                );
                                if let Some(root) = outcome.projects_dir_saved {
                                    action = Some(PageNavAction::ProjectsRootChanged(root));
                                }
                                // The launcher has no MemoryManager; the memory profile is
                                // already persisted by the shared widget, so
                                // `outcome.memory_profile_changed` is intentionally ignored.
                            }
                            #[cfg(not(feature = "tutorial"))]
                            id @ (SettingsSectionId::General | SettingsSectionId::AiBackend) => {
                                let outcome = self.shared.draw(
                                    id,
                                    ui,
                                    SettingsSurface::Launcher,
                                    &self.ai_backend,
                                );
                                if let Some(root) = outcome.projects_dir_saved {
                                    action = Some(PageNavAction::ProjectsRootChanged(root));
                                }
                                // The launcher has no MemoryManager; the memory profile is
                                // already persisted by the shared widget, so
                                // `outcome.memory_profile_changed` is intentionally ignored.
                            }
                            SettingsSectionId::SystemInfo => self.show_system_info_tab(ui),
                            SettingsSectionId::AiComputations => {
                                if let Some(tab_action) = self.show_ai_computations_tab(ui) {
                                    action = Some(tab_action);
                                }
                            }
                            SettingsSectionId::TorchUpgrade => {
                                if let Some(tab_action) = self.show_torch_upgrade_tab(ui) {
                                    action = Some(tab_action);
                                }
                            }
                            SettingsSectionId::PythonEnvironment => {
                                self.show_python_environment_tab(ui);
                            }
                            // Studio-only sections are never listed for the launcher
                            // surface, so `active_tab` can never hold one; render nothing.
                            SettingsSectionId::CanvasRibbon
                            | SettingsSectionId::Typesetting
                            | SettingsSectionId::Hotkeys => {
                                debug_assert!(
                                    false,
                                    "launcher settings active_tab holds studio-only section {:?}",
                                    self.active_tab
                                );
                            }
                        });
                });
            });
        }) {
            action = Some(back_action);
        }

        self.show_save_log_popup(ui, save_log_button_rect);

        action
    }

    fn show_tab_bar(&mut self, ui: &mut Ui) -> egui::Rect {
        let mut save_log_rect = egui::Rect::NOTHING;
        ui.horizontal_wrapped(|ui| {
            // Build the tab bar from the shared section registry (single source of
            // truth for which sections the launcher shows and in what order).
            for descriptor in sections_for(SettingsSurface::Launcher) {
                let id = descriptor.id;
                if id == SettingsSectionId::TorchUpgrade {
                    // The Torch-upgrade tab is dynamic: hidden when no AI is
                    // installed, and relabeled + highlighted by install type.
                    match self.ai_install_type {
                        config::AiInstallType::Base => self.show_tab_button_highlighted(
                            ui,
                            id,
                            t!("launcher.settings.upgrade_to_full_button"),
                        ),
                        config::AiInstallType::Full => self.show_tab_button(
                            ui,
                            id,
                            t!("launcher.settings.install_other_pytorch_button"),
                        ),
                        config::AiInstallType::None => {}
                    }
                } else {
                    // All other sections use their static per-surface title key,
                    // resolved to the active locale at runtime (`t!` needs a literal).
                    let label = crate::i18n_resolve::resolve_key(title_key(
                        id,
                        SettingsSurface::Launcher,
                    ));
                    self.show_tab_button(ui, id, label);
                }
            }

            let response = show_two_line_button(
                ui,
                t!("launcher.settings.save_log_button"),
                t!("launcher.settings.save_log_hint"),
                egui::vec2(300.0, 40.0),
                self.log_popup_open,
            );
            save_log_rect = response.rect;
            if response.clicked() {
                self.log_popup_open = !self.log_popup_open;
            }
        });
        save_log_rect
    }

    fn show_tab_button(&mut self, ui: &mut Ui, tab: SettingsSectionId, label: &str) {
        self.show_tab_button_impl(ui, tab, label, false);
    }

    fn show_tab_button_highlighted(&mut self, ui: &mut Ui, tab: SettingsSectionId, label: &str) {
        self.show_tab_button_impl(ui, tab, label, true);
    }

    fn show_tab_button_impl(
        &mut self,
        ui: &mut Ui,
        tab: SettingsSectionId,
        label: &str,
        warning: bool,
    ) {
        let selected = self.active_tab == tab;
        let fill = if selected {
            TAB_ACTIVE_FILL
        } else if warning {
            TAB_WARNING_FILL
        } else {
            TAB_IDLE_FILL
        };
        let text_color = if selected {
            theme::TEXT_MAIN
        } else {
            theme::TEXT_MUTED
        };
        let desired_width = if label.chars().count() > 24 {
            280.0
        } else {
            190.0
        };
        let desired_size = egui::vec2(desired_width, 36.0);
        let (rect, response) = ui.allocate_exact_size(desired_size, Sense::click());
        let hovered = response.hovered();
        let draw_rect = if hovered {
            rect.expand(theme::BUTTON_HOVER_EXPANSION)
        } else {
            rect
        };
        ui.painter().rect(
            draw_rect,
            CornerRadius::same(10),
            fill,
            Stroke::new(
                1.0,
                if warning {
                    TAB_WARNING_STROKE
                } else {
                    TAB_STROKE
                },
            ),
            egui::StrokeKind::Middle,
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(14.0),
            text_color,
        );
        if response.clicked() {
            self.active_tab = tab;
        }
    }

    fn show_save_log_popup(&mut self, ui: &mut Ui, button_rect: Option<egui::Rect>) {
        if !self.log_popup_open {
            return;
        }
        let Some(button_rect) = button_rect.filter(|rect| rect.is_finite()) else {
            self.log_popup_open = false;
            return;
        };

        const POPUP_WIDTH: f32 = 320.0;
        const POPUP_BUTTON_HEIGHT: f32 = 50.0;
        const POPUP_GAP: f32 = 8.0;

        let popup_height = POPUP_BUTTON_HEIGHT * 2.0 + POPUP_GAP + 24.0;
        let screen = ui.ctx().content_rect();
        let popup_x = (button_rect.center().x - POPUP_WIDTH * 0.5)
            .clamp(screen.left() + 8.0, (screen.right() - POPUP_WIDTH - 8.0).max(screen.left() + 8.0));
        let popup_y = (button_rect.min.y - popup_height - POPUP_GAP).max(screen.top() + 8.0);
        let popup_pos = egui::pos2(popup_x, popup_y);

        let mut save_kind = None;
        let popup_response = Area::new("settings_save_log_popup".into())
            .order(Order::Foreground)
            .fixed_pos(popup_pos)
            .show(ui.ctx(), |ui| {
                Frame::new()
                    .fill(Color32::from_rgb(24, 24, 28))
                    .stroke(Stroke::new(1.0, theme::CARD_STROKE))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(POPUP_WIDTH);
                        ui.vertical(|ui| {
                            if show_two_line_button(
                                ui,
                                t!("launcher.settings.current_log_label"),
                                t!("launcher.settings.current_log_hint"),
                                egui::vec2(POPUP_WIDTH, POPUP_BUTTON_HEIGHT),
                                false,
                            )
                            .clicked()
                            {
                                save_kind = Some(LogKind::Current);
                            }
                            ui.add_space(POPUP_GAP);
                            if show_two_line_button(
                                ui,
                                t!("launcher.settings.previous_log_label"),
                                t!("launcher.settings.previous_log_hint"),
                                egui::vec2(POPUP_WIDTH, POPUP_BUTTON_HEIGHT),
                                false,
                            )
                            .clicked()
                            {
                                save_kind = Some(LogKind::Previous);
                            }
                        });
                    });
            });

        if let Some(kind) = save_kind {
            self.log_popup_open = false;
            self.save_log_file(kind);
            return;
        }

        let clicked_outside = ui.ctx().input(|input| {
            input.pointer.any_pressed()
                && !button_rect.contains(input.pointer.interact_pos().unwrap_or_default())
                && !popup_response
                    .response
                    .rect
                    .contains(input.pointer.interact_pos().unwrap_or_default())
        });
        if clicked_outside {
            self.log_popup_open = false;
        }
    }

    /// Copies the selected runtime log to a user-chosen file via the OS save
    /// dialog.
    ///
    /// Native only. The web twin reports that log export is unavailable (no OS
    /// save dialog / filesystem on web).
    #[cfg(not(target_arch = "wasm32"))]
    fn save_log_file(&mut self, kind: LogKind) {
        let log_dir = config::data_dir();
        let (source_name, label) = match kind {
            LogKind::Current => ("last.log", "current"),
            LogKind::Previous => ("previous.log", "previous"),
        };
        let source_path = log_dir.join(source_name);
        let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
        let default_name = format!("manhwastudio_{label}_log_{timestamp}.log");

        let Some(save_path) = FileDialog::new()
            .set_file_name(&default_name)
            .add_filter(t!("launcher.settings.log_files_filter"), &["log"])
            .save_file()
        else {
            return;
        };

        match std::fs::copy(&source_path, &save_path) {
            Ok(_) => {
                runtime_log::log_info(format!(
                    "[launcher-settings] saved '{}' log to '{}'",
                    source_name,
                    save_path.display()
                ));
            }
            Err(err) => {
                runtime_log::log_error(format!(
                    "[launcher-settings] failed to save '{}' log to '{}': {err}",
                    source_name,
                    save_path.display()
                ));
            }
        }
    }

    /// Web twin of `save_log_file`: no OS save dialog or filesystem on web.
    #[cfg(target_arch = "wasm32")]
    fn save_log_file(&mut self, _kind: LogKind) {
        runtime_log::log_error("Сохранение лога недоступно в веб-версии.".to_string());
    }

    fn show_system_info_tab(&mut self, ui: &mut Ui) {
        self.ensure_system_info_probe_started(ui);
        self.poll_system_info_probe(ui);

        ui.horizontal(|ui| {
            ui.label(theme::status(
                t!("launcher.settings.system_info_collecting"),
                theme::TEXT_MUTED,
            ));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let enabled = !matches!(self.system_info_probe.status, SystemInfoStatus::Running);
                if theme::launcher_button(ui, t!("launcher.common.refresh_button"), Vec2::new(112.0, 34.0), enabled).clicked()
                {
                    self.start_system_info_probe(ui);
                }
            });
        });

        ui.add_space(12.0);
        match &self.system_info_probe.status {
            SystemInfoStatus::Idle | SystemInfoStatus::Running => {
                self.show_system_info_placeholder(ui);
            }
            SystemInfoStatus::Ready(report) => self.show_system_info_report(ui, report),
            SystemInfoStatus::Error(message) => {
                ui.label(theme::status(message, STATUS_ERROR));
            }
        }
    }

    fn ensure_system_info_probe_started(&mut self, ui: &Ui) {
        if matches!(self.system_info_probe.status, SystemInfoStatus::Idle) {
            self.start_system_info_probe(ui);
        }
    }

    fn start_system_info_probe(&mut self, ui: &Ui) {
        self.system_info_probe.status = SystemInfoStatus::Running;
        self.system_info_probe.rx = Some(spawn_system_info_probe());
        ui.ctx().request_repaint();
    }

    fn poll_system_info_probe(&mut self, ui: &Ui) {
        let Some(rx) = self.system_info_probe.rx.take() else {
            return;
        };

        match rx.try_recv() {
            Ok(Ok(report)) => {
                self.system_info_probe.status = SystemInfoStatus::Ready(Box::new(report));
                ui.ctx().request_repaint();
            }
            Ok(Err(err)) => {
                runtime_log::log_error(format!(
                    "[launcher-settings] system info probe failed: {err}"
                ));
                self.system_info_probe.status = SystemInfoStatus::Error(err);
                ui.ctx().request_repaint();
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.system_info_probe.rx = Some(rx);
                ui.ctx().request_repaint();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.system_info_probe.status = SystemInfoStatus::Error(
                    t!("launcher.settings.system_info_no_response").to_string(),
                );
                ui.ctx().request_repaint();
            }
        }
    }

    fn show_system_info_placeholder(&self, ui: &mut Ui) {
        self.show_info_card(ui, t!("launcher.settings.cpu_memory_label"), |ui| {
            self.show_info_row(ui, t!("launcher.settings.status_label"), t!("launcher.settings.checking_status"));
        });
        ui.add_space(10.0);
        self.show_info_card(ui, t!("launcher.settings.video_accelerators_label"), |ui| {
            self.show_info_row(ui, t!("launcher.settings.status_label"), t!("launcher.settings.checking_status"));
        });
    }

    fn show_system_info_report(&self, ui: &mut Ui, report: &SystemInfoReport) {
        self.show_info_card(ui, t!("launcher.settings.cpu_memory_label"), |ui| {
            self.show_info_row(ui, "CPU", &report.cpu.name);
            self.show_info_row(
                ui,
                t!("launcher.settings.cores_label"),
                &format_core_count(report.cpu.physical_cores, report.cpu.logical_cores),
            );
            self.show_info_row(ui, "RAM", &format_memory_total(report.memory.total_bytes));
        });

        ui.add_space(10.0);
        self.show_info_card(ui, t!("launcher.settings.video_accelerators_label"), |ui| {
            if let Some(apple) = &report.gpu.apple_gpu {
                self.show_info_row(ui, "Apple GPU (Metal)", apple);
                ui.add_space(8.0);
            }
            self.show_info_row(
                ui,
                "NVIDIA",
                if report.gpu.nvidia_detected {
                    t!("launcher.settings.detected_female")
                } else {
                    t!("launcher.settings.not_detected_female")
                },
            );
            self.show_info_row(
                ui,
                "CUDA",
                &report
                    .gpu
                    .cuda_version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| t!("launcher.settings.not_detected_female").to_string()),
            );
            self.show_info_row(
                ui,
                "NVIDIA SM",
                &report
                    .gpu
                    .nvidia_compute_capability
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| t!("launcher.settings.not_determined_male").to_string()),
            );
            if let Some(architecture) = &report.gpu.nvidia_architecture {
                self.show_info_row(
                    ui,
                    t!("launcher.settings.nvidia_arch_label"),
                    &format_gpu_architecture(architecture),
                );
            }

            ui.add_space(8.0);
            self.show_info_row(
                ui,
                "AMD",
                if report.gpu.amd_detected {
                    t!("launcher.settings.detected_female")
                } else {
                    t!("launcher.settings.not_detected_female")
                },
            );
            self.show_info_row(
                ui,
                "ROCm",
                &report
                    .gpu
                    .rocm_version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| t!("launcher.settings.not_detected_masc").to_string()),
            );
            if let Some(installation) = &report.gpu.rocm_installation {
                self.show_info_row(
                    ui,
                    t!("launcher.settings.rocm_in_system_label"),
                    if installation.present {
                        t!("launcher.settings.found_masc")
                    } else {
                        t!("launcher.settings.not_found_masc")
                    },
                );
            }
            if let Some(driver) = &report.gpu.linux_driver_status {
                self.show_info_row(ui, "amdgpu", bool_status(driver.amdgpu_loaded));
                self.show_info_row(ui, "/dev/kfd", bool_status(driver.kfd_available));
            }
            self.show_info_row(
                ui,
                t!("launcher.settings.amd_arch_label"),
                &format_architecture_list(&report.gpu.amd_architectures),
            );
            self.show_info_row(
                ui,
                "ROCm 7.2 targets",
                &rocm_7_2_supported_llvm_targets().join(", "),
            );
            if let Some(validation) = &report.gpu.rocm_validation {
                let text = if validation.supported {
                    tf!("launcher.settings.supported_status", validation = validation.reason)
                } else {
                    tf!("launcher.settings.not_confirmed_status", validation = validation.reason)
                };
                self.show_info_row(ui, "ROCm 7.2", &text);
            }

            ui.add_space(8.0);
            self.show_info_row(
                ui,
                "DirectML",
                &format_directml_accelerators(&report.gpu.directml_accelerators),
            );
        });
    }

    fn show_info_card(&self, ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui)) {
        Frame::new()
            .fill(Color32::from_rgba_premultiplied(12, 12, 16, 168))
            .stroke(Stroke::new(1.0, theme::BUTTON_STROKE))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(title)
                        .size(18.0)
                        .strong()
                        .color(theme::TEXT_MAIN),
                );
                ui.add_space(8.0);
                body(ui);
            });
    }

    fn show_info_row(&self, ui: &mut Ui, label: &str, value: &str) {
        ui.horizontal_wrapped(|ui| {
            ui.set_min_height(24.0);
            ui.add_sized(
                [170.0, 20.0],
                egui::Label::new(theme::status(label, theme::TEXT_MUTED)),
            );
            ui.label(theme::status(value, theme::TEXT_MAIN));
        });
    }

    fn show_ai_computations_tab(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        self.ensure_ai_probe_started(ui);
        let action = self.poll_ai_probe(ui);

        ui.horizontal(|ui| {
            ui.label(theme::status(
                t!("launcher.settings.ai_check_hint"),
                theme::TEXT_MUTED,
            ));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let enabled = !matches!(self.ai_probe.status, AiProbeStatus::Running);
                if theme::launcher_button(ui, t!("launcher.common.refresh_button"), Vec2::new(112.0, 34.0), enabled).clicked()
                {
                    self.start_ai_probe(ui);
                }
            });
        });

        ui.add_space(12.0);
        match &self.ai_probe.status {
            AiProbeStatus::Idle | AiProbeStatus::Running => {
                self.show_ai_package_card(ui, "PyTorch", AiPackageStatusView::Checking);
                ui.add_space(10.0);
                self.show_ai_package_card(ui, "ONNX Runtime", AiPackageStatusView::Checking);
            }
            AiProbeStatus::Ready(report) => {
                self.show_ai_package_card(ui, "PyTorch", AiPackageStatusView::Torch(&report.torch));
                ui.add_space(10.0);
                self.show_ai_package_card(
                    ui,
                    "ONNX Runtime",
                    AiPackageStatusView::OnnxRuntime(&report.onnxruntime),
                );
            }
            AiProbeStatus::Error(message) => {
                ui.label(theme::status(message, STATUS_ERROR));
            }
        }

        ui.add_space(28.0);
        ui.separator();
        ui.add_space(220.0);
        action
    }

    fn ensure_ai_probe_started(&mut self, ui: &Ui) {
        if matches!(self.ai_probe.status, AiProbeStatus::Idle) {
            self.start_ai_probe(ui);
        }
    }

    fn start_ai_probe(&mut self, ui: &Ui) {
        self.ai_probe.status = AiProbeStatus::Running;
        self.ai_probe.rx = Some(spawn_ai_computations_probe(config::program_dir()));
        ui.ctx().request_repaint();
    }

    fn poll_ai_probe(&mut self, ui: &Ui) -> Option<PageNavAction> {
        let rx = self.ai_probe.rx.take()?;

        let mut action = None;
        match rx.try_recv() {
            Ok(Ok(report)) => {
                if let Some(install_type) = update_ai_install_type_from_probe(&report) {
                    action = Some(PageNavAction::AiInstallTypeChanged(install_type));
                }
                self.ai_probe.status = AiProbeStatus::Ready(report);
                ui.ctx().request_repaint();
            }
            Ok(Err(err)) => {
                runtime_log::log_error(format!(
                    "[launcher-settings] AI computations probe failed: {err}"
                ));
                self.ai_probe.status = AiProbeStatus::Error(err);
                ui.ctx().request_repaint();
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.ai_probe.rx = Some(rx);
                ui.ctx().request_repaint();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.ai_probe.status = AiProbeStatus::Error(
                    t!("launcher.settings.ai_env_no_response").to_string(),
                );
                ui.ctx().request_repaint();
            }
        }
        action
    }

    fn show_ai_package_card(&self, ui: &mut Ui, title: &str, status: AiPackageStatusView<'_>) {
        Frame::new()
            .fill(Color32::from_rgba_premultiplied(12, 12, 16, 168))
            .stroke(Stroke::new(1.0, theme::BUTTON_STROKE))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(title)
                            .size(18.0)
                            .strong()
                            .color(theme::TEXT_MAIN),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| match status {
                        AiPackageStatusView::Checking => {
                            ui.label(theme::status(t!("launcher.settings.checking_status"), theme::TEXT_MUTED));
                        }
                        AiPackageStatusView::Torch(package)
                        | AiPackageStatusView::OnnxRuntime(package) => {
                            self.show_ai_package_version(ui, package);
                        }
                    });
                });
                ui.add_space(8.0);
                match status {
                    AiPackageStatusView::Checking => {
                        ui.label(theme::status(
                            t!("launcher.settings.compiled_support_checking"),
                            theme::TEXT_MUTED,
                        ));
                    }
                    AiPackageStatusView::Torch(package) => {
                        self.show_ai_package_support(ui, package.support.as_slice());
                    }
                    AiPackageStatusView::OnnxRuntime(package) => {
                        self.show_ai_package_support(ui, package.providers.as_slice());
                    }
                }
            });
    }

    fn show_ai_package_version(&self, ui: &mut Ui, package: &AiPackageProbe) {
        if package.installed {
            let label = package.version.as_deref().unwrap_or(t!("launcher.settings.version_unknown"));
            let color = if package.import_error.is_some() {
                STATUS_ERROR
            } else {
                theme::TEXT_MAIN
            };
            let response = ui.label(theme::status(label, color));
            if let Some(import_error) = &package.import_error {
                response.on_hover_text(import_error);
            }
        } else {
            ui.label(theme::status(t!("launcher.settings.not_installed"), STATUS_ERROR));
        }
    }

    fn show_ai_package_support(&self, ui: &mut Ui, values: &[String]) {
        let support_text = if values.is_empty() {
            t!("launcher.settings.not_determined_neuter").to_string()
        } else {
            values.join(", ")
        };
        ui.label(theme::status(
            &tf!("launcher.settings.compiled_support_status", support_text = support_text),
            theme::TEXT_MUTED,
        ));
    }

    /// Renders the PyTorch upgrade tab and drives the installer worker.
    ///
    /// Native only. The web twin renders an "unavailable on web" notice because
    /// the desktop installer subsystem is compiled out on wasm.
    #[cfg(not(target_arch = "wasm32"))]
    fn show_torch_upgrade_tab(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        self.poll_torch_upgrade(ui);
        let action = self
            .torch_upgrade
            .pending_ai_install_type_action
            .take()
            .map(PageNavAction::AiInstallTypeChanged);
        let installing_full_dependencies = self.ai_install_type == config::AiInstallType::Base;

        let description = if installing_full_dependencies {
            t!("launcher.settings.pytorch_choose_wheel_hint")
        } else {
            t!("launcher.settings.pytorch_choose_other_hint")
        };
        ui.label(theme::status(description, theme::TEXT_MUTED));
        ui.add_space(12.0);

        match self.torch_upgrade.status.clone() {
            TorchUpgradeStatus::Idle => {
                if theme::launcher_button(
                    ui,
                    t!("launcher.settings.check_pytorch_versions_button"),
                    Vec2::new(300.0, 36.0),
                    true,
                )
                .clicked()
                {
                    self.start_torch_upgrade_preflight(ui);
                }
            }
            TorchUpgradeStatus::Preparing => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(theme::status(
                        t!("launcher.settings.checking_gpu_status"),
                        theme::TEXT_MUTED,
                    ));
                });
            }
            TorchUpgradeStatus::Choice(prompt) => {
                ui.label(theme::status(&prompt.summary, theme::TEXT_MAIN));
                ui.add_space(8.0);
                if !prompt.options.is_empty() {
                    ui.label(theme::status(
                        &tf!("launcher.settings.recommended_label", prompt = prompt.options[prompt.recommended_index].label),
                        theme::TEXT_MUTED,
                    ));
                    ui.add_space(8.0);
                    let options = prompt.options.clone();
                    for (idx, option) in options.into_iter().enumerate() {
                        let title = if idx == prompt.recommended_index {
                            tf!("launcher.settings.option_recommended_marker", option = option.label)
                        } else {
                            option.label.clone()
                        };
                        if theme::launcher_button(ui, &title, Vec2::new(320.0, 34.0), true)
                            .clicked()
                        {
                            self.start_torch_upgrade_install(
                                ui,
                                TorchInstallSelection::InstallGpu(option),
                                installing_full_dependencies,
                            );
                        }
                        ui.add_space(6.0);
                    }
                }
                if theme::launcher_button(ui, t!("launcher.settings.keep_cpu_button"), Vec2::new(220.0, 34.0), true)
                    .clicked()
                {
                    self.start_torch_upgrade_install(
                        ui,
                        TorchInstallSelection::SkipCpu,
                        installing_full_dependencies,
                    );
                }
            }
            TorchUpgradeStatus::Running => {
                self.show_torch_upgrade_progress(ui);
            }
            TorchUpgradeStatus::Completed => {
                ui.label(theme::status(t!("launcher.settings.install_complete"), theme::TEXT_MAIN));
                self.show_torch_upgrade_progress(ui);
                ui.add_space(8.0);
                if theme::launcher_button(ui, t!("launcher.settings.choose_other_version_button"), Vec2::new(230.0, 34.0), true)
                    .clicked()
                {
                    self.start_torch_upgrade_preflight(ui);
                }
            }
            TorchUpgradeStatus::Error(message) => {
                ui.label(theme::status(&message, STATUS_ERROR));
                self.show_torch_upgrade_progress(ui);
                ui.add_space(8.0);
                if theme::launcher_button(ui, t!("launcher.settings.retry_button"), Vec2::new(140.0, 34.0), true).clicked()
                {
                    self.start_torch_upgrade_preflight(ui);
                }
            }
        }

        action
    }

    /// Web twin of `show_torch_upgrade_tab`: the desktop installer that performs
    /// PyTorch upgrades has no web counterpart.
    #[cfg(target_arch = "wasm32")]
    fn show_torch_upgrade_tab(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        ui.label(theme::status(
            t!("launcher.settings.pytorch_web_unsupported"),
            theme::TEXT_MUTED,
        ));
        None
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn show_torch_upgrade_progress(&self, ui: &mut Ui) {
        ui.label(theme::status(
            &tf!("launcher.settings.torch_stage_status", arg = self.torch_upgrade.stage_label),
            theme::TEXT_MUTED,
        ));
        ui.add(egui::ProgressBar::new(self.torch_upgrade.stage_progress).show_percentage());
        ui.label(theme::status(
            &self.torch_upgrade.overall_label,
            theme::TEXT_MUTED,
        ));
        ui.add(egui::ProgressBar::new(self.torch_upgrade.overall_progress).show_percentage());
        ui.add_space(10.0);
        Frame::new()
            .fill(Color32::from_rgba_premultiplied(8, 8, 10, 190))
            .stroke(Stroke::new(1.0, theme::BUTTON_STROKE))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.set_min_height(220.0);
                ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .max_height(260.0)
                    .show(ui, |ui| {
                        for line in &self.torch_upgrade.console_lines {
                            ui.monospace(line);
                        }
                    });
            });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_torch_upgrade_preflight(&mut self, ui: &Ui) {
        let (tx, rx) = mpsc::channel();
        self.torch_upgrade = TorchUpgradeState {
            status: TorchUpgradeStatus::Preparing,
            rx: Some(rx),
            pending_ai_install_type_action: None,
            stage_progress: 0.0,
            stage_label: t!("launcher.settings.stage_check_gpu").to_string(),
            overall_progress: 0.0,
            overall_label: t!("launcher.settings.stage_prepare_pytorch_choice").to_string(),
            console_lines: Vec::new(),
        };
        let _ = thread::Builder::new()
            .name("launcher-torch-upgrade-preflight".to_string())
            .spawn(move || {
                let result = utils::detect_torch_preflight();
                let _ = tx.send(InstallEvent::TorchPreflightReady(result));
            });
        ui.ctx().request_repaint();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_torch_upgrade_install(
        &mut self,
        ui: &Ui,
        selection: TorchInstallSelection,
        install_full_dependencies: bool,
    ) {
        let (tx, rx) = mpsc::channel();
        let root_dir = config::program_dir();
        self.torch_upgrade.status = TorchUpgradeStatus::Running;
        self.torch_upgrade.rx = Some(rx);
        self.torch_upgrade.pending_ai_install_type_action = None;
        self.torch_upgrade.stage_progress = 0.0;
        self.torch_upgrade.stage_label = t!("launcher.settings.stage_start_pytorch_install").to_string();
        self.torch_upgrade.overall_progress = 0.0;
        self.torch_upgrade.overall_label = t!("launcher.settings.stage_install_started").to_string();
        self.torch_upgrade.console_lines.clear();

        let _ = thread::Builder::new()
            .name("launcher-torch-upgrade-install".to_string())
            .spawn(move || {
                let result = utils::run_torch_upgrade_worker(
                    root_dir,
                    selection,
                    install_full_dependencies,
                    &tx,
                );
                let _ = tx.send(InstallEvent::Finished(result));
            });
        ui.ctx().request_repaint();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn poll_torch_upgrade(&mut self, ui: &Ui) {
        let Some(rx) = self.torch_upgrade.rx.take() else {
            return;
        };

        let mut keep_rx = true;
        while let Ok(event) = rx.try_recv() {
            match event {
                InstallEvent::Step(text) => {
                    self.torch_upgrade.stage_label = text;
                }
                InstallEvent::ConsoleLine(line) => {
                    self.torch_upgrade.console_lines.push(line);
                    if self.torch_upgrade.console_lines.len() > 2000 {
                        self.torch_upgrade.console_lines.drain(0..200);
                    }
                }
                InstallEvent::Progress {
                    stage_value,
                    stage_label,
                    overall_value,
                    overall_label,
                } => {
                    self.torch_upgrade.stage_progress = stage_value.clamp(0.0, 1.0);
                    self.torch_upgrade.stage_label = stage_label;
                    self.torch_upgrade.overall_progress = overall_value.clamp(0.0, 1.0);
                    self.torch_upgrade.overall_label = overall_label;
                }
                InstallEvent::TorchPreflightReady(result) => match result {
                    TorchPreflightResult::Skip { reason } => {
                        self.torch_upgrade
                            .console_lines
                            .push(format!("[PyTorch] {reason}"));
                        self.torch_upgrade.status = TorchUpgradeStatus::Choice(TorchChoicePrompt {
                            options: Vec::new(),
                            recommended_index: 0,
                            summary: reason,
                        });
                        keep_rx = false;
                    }
                    TorchPreflightResult::Choose(prompt) => {
                        self.torch_upgrade.overall_label = prompt.summary.clone();
                        self.torch_upgrade.status = TorchUpgradeStatus::Choice(prompt);
                        keep_rx = false;
                    }
                },
                InstallEvent::Finished(Ok(())) => {
                    keep_rx = false;
                    if self.ai_install_type == config::AiInstallType::Base {
                        match persist_ai_install_type(config::AiInstallType::Full) {
                            Ok(()) => {
                                self.ai_install_type = config::AiInstallType::Full;
                                self.torch_upgrade.pending_ai_install_type_action =
                                    Some(config::AiInstallType::Full);
                            }
                            Err(err) => {
                                self.torch_upgrade.status = TorchUpgradeStatus::Error(tf!("launcher.settings.install_saved_full_error", err = err));
                                continue;
                            }
                        }
                    }
                    self.torch_upgrade.status = TorchUpgradeStatus::Completed;
                    self.torch_upgrade.stage_progress = 1.0;
                    self.torch_upgrade.overall_progress = 1.0;
                }
                InstallEvent::Finished(Err(err)) => {
                    keep_rx = false;
                    self.torch_upgrade.status = TorchUpgradeStatus::Error(err);
                }
            }
        }

        if keep_rx {
            self.torch_upgrade.rx = Some(rx);
        }
        ui.ctx().request_repaint();
    }

    /// Renders the interactive Python-environment console tab.
    ///
    /// Native only: it spawns and talks to an OS shell. The web twin renders an
    /// "unavailable on web" notice.
    #[cfg(not(target_arch = "wasm32"))]
    fn show_python_environment_tab(&mut self, ui: &mut Ui) {
        self.ensure_python_console_started(ui);
        self.poll_python_console(ui);

        Frame::new()
            .fill(Color32::from_rgba_premultiplied(12, 12, 16, 168))
            .stroke(Stroke::new(1.0, theme::BUTTON_STROKE))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.set_min_height(CONSOLE_MIN_HEIGHT);
                let console_text_width = ui.available_width();
                ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(console_text_width);
                        ui.add(egui::Label::new(console_output_layout_job(
                            ui,
                            self.python_console.output.as_str(),
                            console_text_width,
                        )));
                    });
            });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let input_width = (ui.available_width() - 112.0).max(260.0);
            let response = ui.add_sized(
                [input_width, 56.0],
                TextEdit::multiline(&mut self.python_console.input)
                    .desired_rows(CONSOLE_INPUT_ROWS)
                    .font(TextStyle::Monospace)
                    .hint_text(t!("launcher.settings.console_command_placeholder")),
            );
            let submit_from_button =
                theme::launcher_button(ui, "Enter", egui::vec2(100.0, 40.0), true).clicked();
            let submit_from_key = response.has_focus()
                && ui.input(|input| {
                    input.key_pressed(Key::Enter)
                        && !input.modifiers.ctrl
                        && !input.modifiers.command
                        && !input.modifiers.alt
                });
            if submit_from_key {
                trim_single_trailing_newline(&mut self.python_console.input);
            }
            if submit_from_button || submit_from_key {
                self.submit_python_console_command(ui);
                response.request_focus();
            }
        });
        ui.add_space(6.0);
        ui.label(theme::footer(
            t!("launcher.settings.console_enter_hint"),
        ));
    }

    /// Web twin of `show_python_environment_tab`: no OS shell exists on web.
    #[cfg(target_arch = "wasm32")]
    fn show_python_environment_tab(&mut self, ui: &mut Ui) {
        ui.label(theme::status(
            t!("launcher.settings.console_web_unsupported"),
            theme::TEXT_MUTED,
        ));
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn ensure_python_console_started(&mut self, ui: &Ui) {
        if self.python_console.runtime.is_some() || self.python_console.attempted_start {
            return;
        }

        self.python_console.attempted_start = true;
        self.python_console
            .output
            .push_str(t!("launcher.settings.console_starting_status"));

        match PythonConsoleRuntime::spawn(config::program_dir()) {
            Ok(runtime) => {
                runtime_log::log_info("[launcher-settings] python console started");
                self.python_console.runtime = Some(runtime);
                ui.ctx().request_repaint();
            }
            Err(err) => {
                runtime_log::log_error(format!(
                    "[launcher-settings] failed to start python console: {err}"
                ));
                self.python_console
                    .output
                    .push_str(&tf!("launcher.settings.console_start_error", err = err));
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn poll_python_console(&mut self, ui: &Ui) {
        let Some(runtime) = self.python_console.runtime.as_mut() else {
            return;
        };

        let mut received_any = false;
        while let Ok(event) = runtime.event_rx.try_recv() {
            received_any = true;
            match event {
                PythonConsoleEvent::Output(text) => self.python_console.output.push_str(&text),
                PythonConsoleEvent::Error(text) => {
                    self.python_console.output.push_str(&text);
                }
            }
        }

        if !runtime.terminated {
            match runtime.child.try_wait() {
                Ok(Some(status)) => {
                    runtime.terminated = true;
                    self.python_console
                        .output
                        .push_str(&tf!("launcher.settings.shell_finished_status", status = status));
                    received_any = true;
                }
                Ok(None) => {}
                Err(err) => {
                    runtime.terminated = true;
                    self.python_console
                        .output
                        .push_str(&tf!("launcher.settings.shell_status_error", err = err));
                    runtime_log::log_error(format!(
                        "[launcher-settings] failed to poll python console process: {err}"
                    ));
                    received_any = true;
                }
            }
        }

        if received_any {
            ui.ctx().request_repaint();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn submit_python_console_command(&mut self, ui: &Ui) {
        let command = self.python_console.input.trim_end().to_string();
        self.python_console.input.clear();
        if command.is_empty() {
            return;
        }

        self.python_console
            .output
            .push_str(&format!("> {command}\n"));

        let Some(runtime) = self.python_console.runtime.as_mut() else {
            self.python_console
                .output
                .push_str(t!("launcher.settings.shell_not_started"));
            return;
        };
        if runtime.terminated {
            self.python_console
                .output
                .push_str(t!("launcher.settings.shell_already_finished"));
            return;
        }
        if let Err(err) = runtime.send_command(command) {
            self.python_console
                .output
                .push_str(&tf!("launcher.settings.shell_send_error", err = err));
            runtime_log::log_error(format!(
                "[launcher-settings] failed to send python console command: {err}"
            ));
        } else {
            ui.ctx().request_repaint();
        }
    }

}

// Only needed to tear down the native Python console; no drop work on web.
#[cfg(not(target_arch = "wasm32"))]
impl Drop for SettingsPageState {
    fn drop(&mut self) {
        if let Some(runtime) = self.python_console.runtime.as_mut() {
            runtime.terminate();
        }
    }
}

fn show_two_line_button(
    ui: &mut Ui,
    title: &str,
    subtitle: &str,
    size: Vec2,
    active: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let hovered = response.hovered();
    let fill = if active { TAB_ACTIVE_FILL } else { TAB_IDLE_FILL };
    let draw_rect = if hovered {
        rect.expand(theme::BUTTON_HOVER_EXPANSION)
    } else {
        rect
    };
    ui.painter().rect(
        draw_rect,
        CornerRadius::same(10),
        fill,
        Stroke::new(1.0, TAB_STROKE),
        egui::StrokeKind::Middle,
    );
    let center = rect.center();
    ui.painter().text(
        egui::pos2(center.x, center.y - 9.0),
        Align2::CENTER_CENTER,
        title,
        FontId::proportional(14.0),
        theme::TEXT_MAIN,
    );
    ui.painter().text(
        egui::pos2(center.x, center.y + 9.0),
        Align2::CENTER_CENTER,
        subtitle,
        FontId::proportional(11.0),
        theme::TEXT_MUTED,
    );
    response
}

// Console text layout is only used by the native Python-console tab.
#[cfg(not(target_arch = "wasm32"))]
fn console_output_layout_job(ui: &Ui, output: &str, wrap_width: f32) -> egui::text::LayoutJob {
    let font_id = TextStyle::Monospace.resolve(ui.style());
    let mut job = egui::text::LayoutJob::simple(
        output.to_string(),
        font_id,
        theme::TEXT_MAIN,
        wrap_width.max(1.0),
    );
    job.wrap.break_anywhere = true;
    job
}

#[cfg(not(target_arch = "wasm32"))]
impl PythonConsoleRuntime {
    fn spawn(app_dir: PathBuf) -> Result<Self, String> {
        let mut command = build_python_console_shell_command();
        apply_hidden_process_flags(&mut command);
        command
            .current_dir(&app_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|err| tf!("launcher.settings.shell_start_env_error", err = err))?;
        let stdin = child.stdin.take().ok_or_else(|| {
            t!("launcher.settings.shell_no_stdin").to_string()
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| t!("launcher.settings.shell_no_stdout").to_string())?;
        let stderr = child.stderr.take().ok_or_else(|| {
            t!("launcher.settings.shell_no_stderr").to_string()
        })?;

        let (command_tx, command_rx) = mpsc::channel::<String>();
        let (event_tx, event_rx) = mpsc::channel::<PythonConsoleEvent>();

        spawn_console_writer_thread(stdin, command_rx, event_tx.clone());
        spawn_console_reader_thread(stdout, event_tx.clone(), false);
        spawn_console_reader_thread(stderr, event_tx.clone(), true);

        let runtime = Self {
            child,
            command_tx,
            event_rx,
            terminated: false,
        };
        runtime.bootstrap(app_dir)?;
        Ok(runtime)
    }

    fn bootstrap(&self, app_dir: PathBuf) -> Result<(), String> {
        self.send_command(configure_shell_encoding_command())?;
        self.send_command(change_directory_command(&app_dir))?;
        match python_manager::detect_python_environment(&app_dir) {
            Ok(environment) => {
                runtime_log::log_info(format!(
                    "[launcher-settings] activating python environment in '{}'",
                    app_dir.display()
                ));
                for command in
                    python_manager::activation_commands(&environment, python_shell_kind())
                {
                    self.send_command(command)?;
                }
                self.send_command(python_manager::configure_pip_fallback_command(
                    python_shell_kind(),
                ))?;
                self.send_command(python_manager::python_ready_probe_command(
                    python_shell_kind(),
                ))?;
            }
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[launcher-settings] python environment not found for console: {err}"
                ));
                self.send_command(shell_echo_command(&tf!("launcher.settings.python_env_not_found_error", err = err)))?;
            }
        }
        Ok(())
    }

    fn send_command(&self, command: String) -> Result<(), String> {
        self.command_tx
            .send(command)
            .map_err(|err| tf!("launcher.settings.shell_channel_closed_error", err = err))
    }

    fn terminate(&mut self) {
        if self.terminated {
            return;
        }
        if let Err(err) = self.child.kill() {
            runtime_log::log_warn(format!(
                "[launcher-settings] failed to kill python console process: {err}"
            ));
        }
        self.terminated = true;
    }
}

// Only invoked from the native Torch-upgrade completion path.
#[cfg(not(target_arch = "wasm32"))]
fn persist_ai_install_type(install_type: config::AiInstallType) -> anyhow::Result<()> {
    let mut cfg = config::load_user_config()?;
    cfg.set_path(
        &["General", config::GENERAL_AI_INSTALL_TYPE_KEY],
        Value::String(install_type.as_str().to_string()),
    )?;
    Ok(())
}

fn update_ai_install_type_from_probe(
    report: &AiComputationsReport,
) -> Option<config::AiInstallType> {
    if detect_ai_install_type_from_report(report) != config::AiInstallType::Full {
        return None;
    }

    let mut cfg = match config::load_user_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[launcher-settings] failed to load user config for AI install type update: {err:#}"
            ));
            return None;
        }
    };
    if config::AiInstallType::from_user_settings(&cfg.data) == config::AiInstallType::Full {
        return None;
    }
    if let Err(err) = cfg.set_path(
        &["General", config::GENERAL_AI_INSTALL_TYPE_KEY],
        Value::String(config::AiInstallType::Full.as_str().to_string()),
    ) {
        runtime_log::log_warn(format!(
            "[launcher-settings] failed to persist AI install type upgrade to Full: {err:#}"
        ));
        return None;
    }
    Some(config::AiInstallType::Full)
}

fn spawn_system_info_probe() -> Receiver<Result<SystemInfoReport, String>> {
    let (tx, rx) = mpsc::channel();
    let spawn_result = thread::Builder::new()
        .name("launcher-system-info-probe".to_string())
        .spawn(move || {
            let result = collect_system_info_report();
            if tx.send(result).is_err() {
                runtime_log::log_warn(
                    "[launcher-settings] system info probe result receiver was dropped",
                );
            }
        });

    if let Err(err) = spawn_result {
        let (fallback_tx, fallback_rx) = mpsc::channel();
        let message = tf!("launcher.settings.start_system_check_error", err = err);
        if fallback_tx.send(Err(message)).is_err() {
            runtime_log::log_warn(
                "[launcher-settings] failed to send system info probe spawn error to UI",
            );
        }
        return fallback_rx;
    }

    rx
}

fn collect_system_info_report() -> Result<SystemInfoReport, String> {
    runtime_log::log_info("[launcher-settings] collecting system information");
    Ok(SystemInfoReport {
        cpu: collect_cpu_info(),
        memory: collect_memory_info(),
        gpu: collect_gpu_info(),
    })
}

fn collect_cpu_info() -> CpuInfoReport {
    let logical_cores = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    CpuInfoReport {
        name: detect_cpu_name().unwrap_or_else(|| t!("launcher.settings.cpu_not_determined").to_string()),
        physical_cores: detect_physical_core_count(),
        logical_cores,
    }
}

fn collect_memory_info() -> MemoryInfoReport {
    MemoryInfoReport {
        total_bytes: detect_total_memory_bytes(),
    }
}

fn collect_gpu_info() -> GpuInfoReport {
    GpuInfoReport {
        nvidia_detected: detect_nvidia_gpu(),
        amd_detected: detect_amd_gpu(),
        cuda_version: detect_cuda_runtime_version(),
        nvidia_compute_capability: detect_nvidia_compute_capability(),
        nvidia_architecture: detect_nvidia_gpu_architecture(),
        rocm_version: detect_rocm_runtime_version(),
        linux_driver_status: cfg!(target_os = "linux").then(linux_driver_status),
        rocm_installation: cfg!(target_os = "linux").then(detect_rocm_installation_linux),
        amd_architectures: if cfg!(target_os = "linux") {
            detect_amd_gpu_architectures_linux()
        } else {
            Vec::new()
        },
        rocm_validation: cfg!(target_os = "linux").then(validate_rocm_7_2_support_linux),
        directml_accelerators: detect_directml_accelerators_windows(),
        apple_gpu: detect_apple_gpu(),
    }
}

#[cfg(target_os = "linux")]
fn detect_cpu_name() -> Option<String> {
    let content = fs::read_to_string("/proc/cpuinfo").ok()?;
    content
        .lines()
        .find_map(|line| line.strip_prefix("model name"))
        .and_then(|tail| {
            tail.split_once(':')
                .map(|(_, value)| value.trim().to_string())
        })
        .filter(|name| !name.is_empty())
}

#[cfg(target_os = "windows")]
fn detect_cpu_name() -> Option<String> {
    command_output(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name)",
        ],
    )
    .map(|name| name.trim().to_string())
    .filter(|name| !name.is_empty())
}

#[cfg(target_os = "macos")]
fn detect_cpu_name() -> Option<String> {
    command_output("sysctl", &["-n", "machdep.cpu.brand_string"])
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn detect_cpu_name() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn detect_physical_core_count() -> Option<usize> {
    let content = fs::read_to_string("/proc/cpuinfo").ok()?;
    let mut pairs = HashSet::new();
    let mut current_physical: Option<String> = None;
    let mut current_core: Option<String> = None;

    for line in content.lines().chain(std::iter::once("")) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let (Some(physical), Some(core)) = (current_physical.take(), current_core.take()) {
                pairs.insert((physical, core));
            }
            current_physical = None;
            current_core = None;
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            match key.trim() {
                "physical id" => current_physical = Some(value.trim().to_string()),
                "core id" => current_core = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }

    if pairs.is_empty() {
        None
    } else {
        Some(pairs.len())
    }
}

#[cfg(target_os = "windows")]
fn detect_physical_core_count() -> Option<usize> {
    let output = command_output(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_Processor | Measure-Object -Property NumberOfCores -Sum).Sum",
        ],
    )?;
    output.trim().parse::<usize>().ok()
}

#[cfg(target_os = "macos")]
fn detect_physical_core_count() -> Option<usize> {
    command_output("sysctl", &["-n", "hw.physicalcpu"])
        .and_then(|output| output.trim().parse::<usize>().ok())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn detect_physical_core_count() -> Option<usize> {
    None
}

#[cfg(target_os = "linux")]
fn detect_total_memory_bytes() -> Option<u64> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;
    content.lines().find_map(|line| {
        let rest = line.strip_prefix("MemTotal:")?.trim();
        let kb_text = rest.split_whitespace().next()?;
        let kb = kb_text.parse::<u64>().ok()?;
        kb.checked_mul(1024)
    })
}

#[cfg(target_os = "windows")]
fn detect_total_memory_bytes() -> Option<u64> {
    let output = command_output(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory",
        ],
    )?;
    output.trim().parse::<u64>().ok()
}

#[cfg(target_os = "macos")]
fn detect_total_memory_bytes() -> Option<u64> {
    command_output("sysctl", &["-n", "hw.memsize"])
        .and_then(|output| output.trim().parse::<u64>().ok())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn detect_total_memory_bytes() -> Option<u64> {
    None
}

fn format_core_count(physical: Option<usize>, logical: usize) -> String {
    match physical {
        Some(value) => tf!("launcher.settings.cores_phys_logical", value = value, logical = logical),
        None => tf!("launcher.settings.cores_logical", logical = logical),
    }
}

fn format_memory_total(total_bytes: Option<u64>) -> String {
    let Some(bytes) = total_bytes else {
        return t!("launcher.settings.not_determined_neuter").to_string();
    };
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        let tenths = u128::from(bytes) * 10 / u128::from(GIB);
        let whole = tenths / 10;
        let fraction = tenths % 10;
        format!("{whole}.{fraction} GiB")
    } else {
        let mib = u128::from(bytes) / u128::from(MIB);
        format!("{mib} MiB")
    }
}

fn format_gpu_architecture(architecture: &GpuArchitecture) -> String {
    let mut parts = Vec::new();
    if let Some(name) = architecture
        .name
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(name.to_string());
    }
    if let Some(family) = architecture
        .architecture
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(family.to_string());
    }
    if let Some(target) = architecture
        .llvm_target
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(target.to_string());
    }
    if parts.is_empty() {
        t!("launcher.settings.not_determined_fem").to_string()
    } else {
        parts.join(" / ")
    }
}

fn format_architecture_list(architectures: &[GpuArchitecture]) -> String {
    if architectures.is_empty() {
        return t!("launcher.settings.not_determined_plural").to_string();
    }
    architectures
        .iter()
        .map(format_gpu_architecture)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_directml_accelerators(accelerators: &[DirectMlAccelerator]) -> String {
    if !cfg!(target_os = "windows") {
        return t!("launcher.settings.windows_only").to_string();
    }
    if accelerators.is_empty() {
        return t!("launcher.settings.no_compatible_accelerators").to_string();
    }
    accelerators
        .iter()
        .map(|accelerator| accelerator.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

fn bool_status(value: bool) -> &'static str {
    if value { t!("launcher.settings.yes") } else { t!("launcher.settings.no") }
}

#[cfg(target_os = "macos")]
fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(command)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(target_os = "windows")]
fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(command);
    apply_hidden_process_flags(&mut cmd);
    let output = cmd
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stdout.trim().is_empty() {
        stderr.to_string()
    } else if stderr.trim().is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    };

    if text.trim().is_empty() {
        None
    } else {
        Some(text.trim().to_string())
    }
}

// The following console/shell helpers exist only for the native Python console
// (OS shell spawning) and are compiled out on web.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_console_writer_thread(
    stdin: std::process::ChildStdin,
    command_rx: Receiver<String>,
    event_tx: Sender<PythonConsoleEvent>,
) {
    thread::spawn(move || {
        let mut writer = BufWriter::new(stdin);
        for command in command_rx {
            if let Err(err) = writer.write_all(command.as_bytes()) {
                let _ = event_tx.send(PythonConsoleEvent::Error(tf!("launcher.settings.shell_write_error", err = err)));
                return;
            }
            if let Err(err) = writer.write_all(shell_line_ending().as_bytes()) {
                let _ = event_tx.send(PythonConsoleEvent::Error(tf!("launcher.settings.shell_newline_error", err = err)));
                return;
            }
            if let Err(err) = writer.flush() {
                let _ = event_tx.send(PythonConsoleEvent::Error(tf!("launcher.settings.shell_flush_error", err = err)));
                return;
            }
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_console_reader_thread(
    stream: impl std::io::Read + Send + 'static,
    event_tx: Sender<PythonConsoleEvent>,
    is_stderr: bool,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => return,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buffer).into_owned();
                    let payload = if is_stderr {
                        format!("[stderr] {text}")
                    } else {
                        text
                    };
                    if event_tx.send(PythonConsoleEvent::Output(payload)).is_err() {
                        return;
                    }
                }
                Err(err) => {
                    let _ = event_tx.send(PythonConsoleEvent::Error(tf!("launcher.settings.shell_read_error", err = err)));
                    return;
                }
            }
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn trim_single_trailing_newline(value: &mut String) {
    if value.ends_with("\r\n") {
        value.truncate(value.len().saturating_sub(2));
        return;
    }
    if value.ends_with('\n') {
        value.pop();
    }
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
fn build_python_console_shell_command() -> Command {
    let mut command = Command::new("powershell");
    command
        .arg("-NoLogo")
        .arg("-NoExit")
        .arg("-ExecutionPolicy")
        .arg("Bypass");
    command
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
fn configure_shell_encoding_command() -> String {
    "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; [Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $OutputEncoding = [System.Text.Encoding]::UTF8".to_string()
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "windows")))]
fn configure_shell_encoding_command() -> String {
    "export LANG=C.UTF-8; export LC_ALL=C.UTF-8".to_string()
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "windows")))]
fn build_python_console_shell_command() -> Command {
    Command::new("sh")
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
fn apply_hidden_process_flags(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "windows")))]
fn apply_hidden_process_flags(_command: &mut Command) {}

#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
fn shell_line_ending() -> &'static str {
    "\r\n"
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "windows")))]
fn shell_line_ending() -> &'static str {
    "\n"
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
fn change_directory_command(path: &Path) -> String {
    format!("Set-Location -LiteralPath '{}'", powershell_escape(path))
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "windows")))]
fn change_directory_command(path: &Path) -> String {
    format!("cd '{}'", sh_escape(path))
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
fn python_shell_kind() -> PythonShellKind {
    PythonShellKind::PowerShell
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "windows")))]
fn python_shell_kind() -> PythonShellKind {
    PythonShellKind::PosixSh
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
fn shell_echo_command(message: &str) -> String {
    format!("Write-Output '{}'", powershell_escape_str(message))
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "windows")))]
fn shell_echo_command(message: &str) -> String {
    format!("printf '%s\n' '{}'", sh_escape_str(message))
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
fn powershell_escape(path: &Path) -> String {
    powershell_escape_str(&path.to_string_lossy())
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
fn powershell_escape_str(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "windows")))]
fn sh_escape(path: &Path) -> String {
    sh_escape_str(&path.to_string_lossy())
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "windows")))]
fn sh_escape_str(value: &str) -> String {
    value.replace('\'', r"'\''")
}
