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
*/

use crate::ai_install_probe::{
    AiComputationsReport, AiPackageProbe, detect_ai_install_type_from_report,
    spawn_ai_computations_probe,
};
use crate::config;
use crate::gpu_utils::{
    DirectMlAccelerator, GpuArchitecture, LinuxDriverStatus, RocmInstallationStatus,
    RocmSupportValidation, RuntimeVersion, detect_amd_gpu, detect_amd_gpu_architectures_linux,
    detect_cuda_runtime_version, detect_directml_accelerators_windows,
    detect_nvidia_compute_capability, detect_nvidia_gpu, detect_nvidia_gpu_architecture,
    detect_rocm_installation_linux, detect_rocm_runtime_version, linux_driver_status,
    rocm_7_2_supported_llvm_targets, validate_rocm_7_2_support_linux,
};
use crate::installer::install::{
    InstallEvent, TorchChoicePrompt, TorchInstallSelection, TorchPreflightResult,
};
use crate::installer::utils;
use crate::launcher::pages::base::{self, PageNavAction};
use crate::launcher::theme;
use crate::python_manager::{self, PythonShellKind};
use crate::runtime_log;
use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Frame, Key, Layout, RichText, ScrollArea, Sense,
    Stroke, TextEdit, TextStyle, Ui, Vec2,
};
use rfd::FileDialog;
use serde_json::Value;
#[cfg(target_os = "linux")]
use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

const STATUS_ERROR: Color32 = Color32::from_rgb(214, 104, 104);
const TAB_ACTIVE_FILL: Color32 = Color32::from_rgba_premultiplied(72, 72, 78, 176);
const TAB_IDLE_FILL: Color32 = theme::BUTTON_FILL;
const TAB_HOVER_FILL: Color32 = theme::BUTTON_HOVERED;
const TAB_STROKE: Color32 = theme::BUTTON_STROKE;
const TAB_WARNING_FILL: Color32 = Color32::from_rgba_premultiplied(120, 88, 18, 188);
const TAB_WARNING_HOVER_FILL: Color32 = Color32::from_rgba_premultiplied(150, 112, 24, 208);
const TAB_WARNING_STROKE: Color32 = Color32::from_rgba_premultiplied(236, 197, 76, 170);
const SETTINGS_CARD_EDGE_GAP: f32 = 18.0;
const CONSOLE_MIN_HEIGHT: f32 = 320.0;
const CONSOLE_INPUT_ROWS: usize = 2;
#[derive(Debug)]
enum PythonConsoleEvent {
    Output(String),
    Error(String),
}

#[derive(Debug)]
struct PythonConsoleRuntime {
    child: Child,
    command_tx: Sender<String>,
    event_rx: Receiver<PythonConsoleEvent>,
    terminated: bool,
}

#[derive(Debug, Default)]
struct PythonConsoleState {
    output: String,
    input: String,
    runtime: Option<PythonConsoleRuntime>,
    attempted_start: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    General,
    SystemInfo,
    AiComputations,
    TorchUpgrade,
    PythonEnvironment,
}

pub struct SettingsPageState {
    active_tab: SettingsTab,
    projects_dir_input: String,
    saved_projects_dir: String,
    status: SettingsStatus,
    python_console: PythonConsoleState,
    ai_probe: AiComputationsProbeState,
    system_info_probe: SystemInfoProbeState,
    ai_install_type: config::AiInstallType,
    torch_upgrade: TorchUpgradeState,
}

enum SettingsStatus {
    Idle,
    Info(String),
    Success(String),
    Error(String),
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
}

#[derive(Debug, Clone, Copy)]
enum AiPackageStatusView<'a> {
    Checking,
    Torch(&'a AiPackageProbe),
    OnnxRuntime(&'a AiPackageProbe),
}

impl SettingsPageState {
    pub fn new(projects_root: PathBuf, ai_install_type: config::AiInstallType) -> Self {
        let projects_dir = normalize_projects_dir_value(&projects_root.to_string_lossy());
        Self {
            active_tab: SettingsTab::General,
            projects_dir_input: projects_dir.clone(),
            saved_projects_dir: projects_dir,
            status: SettingsStatus::Idle,
            python_console: PythonConsoleState::default(),
            ai_probe: AiComputationsProbeState::default(),
            system_info_probe: SystemInfoProbeState::default(),
            ai_install_type,
            torch_upgrade: TorchUpgradeState::default(),
        }
    }

    pub fn set_projects_root(&mut self, projects_root: PathBuf) {
        let normalized = normalize_projects_dir_value(&projects_root.to_string_lossy());
        self.projects_dir_input = normalized.clone();
        self.saved_projects_dir = normalized;
    }

    pub fn close_python_console(&mut self) {
        if let Some(runtime) = self.python_console.runtime.as_mut() {
            runtime.terminate();
        }
        self.python_console.runtime = None;
        self.python_console.attempted_start = false;
        self.python_console.input.clear();
        self.python_console.output.clear();
    }

    pub fn set_ai_install_type(&mut self, ai_install_type: config::AiInstallType) {
        self.ai_install_type = ai_install_type;
        if ai_install_type == config::AiInstallType::None
            && self.active_tab == SettingsTab::TorchUpgrade
        {
            self.active_tab = SettingsTab::General;
        }
    }

    pub fn show(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        let mut action = None;
        if let Some(back_action) = base::show_page_shell(ui, |ui| {
            ui.add_space(16.0);
            let available_width = ui.available_width();
            let card_width = (available_width - SETTINGS_CARD_EDGE_GAP * 2.0).max(700.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(card_width);
                ui.set_min_height(420.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Настройки").size(24.0).strong());
                    ui.add_space(18.0);

                    self.show_tab_bar(ui);
                    ui.add_space(18.0);

                    ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| match self.active_tab {
                            SettingsTab::General => self.show_general_tab(ui, &mut action),
                            SettingsTab::SystemInfo => self.show_system_info_tab(ui),
                            SettingsTab::AiComputations => {
                                if let Some(tab_action) = self.show_ai_computations_tab(ui) {
                                    action = Some(tab_action);
                                }
                            }
                            SettingsTab::TorchUpgrade => {
                                if let Some(tab_action) = self.show_torch_upgrade_tab(ui) {
                                    action = Some(tab_action);
                                }
                            }
                            SettingsTab::PythonEnvironment => {
                                self.show_python_environment_tab(ui);
                            }
                        });
                });
            });
        }) {
            action = Some(back_action);
        }

        action
    }

    fn pick_projects_dir(&mut self) {
        let current = normalize_projects_dir_value(&self.projects_dir_input);
        let start_dir = if Path::new(&current).is_dir() {
            PathBuf::from(current)
        } else {
            config::default_projects_root()
        };
        let Some(selected_dir) = FileDialog::new().set_directory(start_dir).pick_folder() else {
            return;
        };

        self.projects_dir_input = normalize_projects_dir_value(&selected_dir.to_string_lossy());
        self.status =
            SettingsStatus::Info("Папка выбрана. Нажмите «Сохранить папку проектов».".to_string());
    }

    fn save_projects_root(&mut self) -> Result<PathBuf, String> {
        let normalized = normalize_projects_dir_value(&self.projects_dir_input);
        persist_projects_root(&normalized).map_err(|err| {
            runtime_log::log_error(format!(
                "[launcher-settings] failed to save projects root '{}': {err:#}",
                normalized
            ));
            format!("Не удалось сохранить папку проектов: {err}")
        })?;

        self.projects_dir_input = normalized.clone();
        self.saved_projects_dir = normalized.clone();
        self.status = SettingsStatus::Success("Папка проектов сохранена.".to_string());
        Ok(PathBuf::from(normalized))
    }

    fn clear_success_status(&mut self) {
        if matches!(self.status, SettingsStatus::Success(_)) {
            self.status = SettingsStatus::Idle;
        }
    }

    fn show_tab_bar(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            self.show_tab_button(ui, SettingsTab::General, "Общие настройки");
            self.show_tab_button(ui, SettingsTab::SystemInfo, "Информация о системе");
            self.show_tab_button(ui, SettingsTab::AiComputations, "ИИ вычисления");
            match self.ai_install_type {
                config::AiInstallType::Base => self.show_tab_button_highlighted(
                    ui,
                    SettingsTab::TorchUpgrade,
                    "Обновить до полной версии",
                ),
                config::AiInstallType::Full => self.show_tab_button(
                    ui,
                    SettingsTab::TorchUpgrade,
                    "Установить другую версию PyTorch",
                ),
                config::AiInstallType::None => {}
            }
            self.show_tab_button(ui, SettingsTab::PythonEnvironment, "Python окружение");
        });
    }

    fn show_tab_button(&mut self, ui: &mut Ui, tab: SettingsTab, label: &str) {
        self.show_tab_button_impl(ui, tab, label, false);
    }

    fn show_tab_button_highlighted(&mut self, ui: &mut Ui, tab: SettingsTab, label: &str) {
        self.show_tab_button_impl(ui, tab, label, true);
    }

    fn show_tab_button_impl(&mut self, ui: &mut Ui, tab: SettingsTab, label: &str, warning: bool) {
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
        let actual_fill = if hovered && !selected && warning {
            TAB_WARNING_HOVER_FILL
        } else if hovered && !selected {
            TAB_HOVER_FILL
        } else {
            fill
        };
        ui.painter().rect(
            rect,
            CornerRadius::same(10),
            actual_fill,
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

    fn show_system_info_tab(&mut self, ui: &mut Ui) {
        self.ensure_system_info_probe_started(ui);
        self.poll_system_info_probe(ui);

        ui.horizontal(|ui| {
            ui.label(theme::status(
                "Системная информация собирается в фоновом потоке.",
                theme::TEXT_MUTED,
            ));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let enabled = !matches!(self.system_info_probe.status, SystemInfoStatus::Running);
                if theme::launcher_button(ui, "Обновить", Vec2::new(112.0, 34.0), enabled).clicked()
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
                    "Фоновая проверка системной информации завершилась без ответа.".to_string(),
                );
                ui.ctx().request_repaint();
            }
        }
    }

    fn show_system_info_placeholder(&self, ui: &mut Ui) {
        self.show_info_card(ui, "CPU и память", |ui| {
            self.show_info_row(ui, "Статус", "Проверяется...");
        });
        ui.add_space(10.0);
        self.show_info_card(ui, "DirectML / CUDA / ROCm", |ui| {
            self.show_info_row(ui, "Статус", "Проверяется...");
        });
    }

    fn show_system_info_report(&self, ui: &mut Ui, report: &SystemInfoReport) {
        self.show_info_card(ui, "CPU и память", |ui| {
            self.show_info_row(ui, "CPU", &report.cpu.name);
            self.show_info_row(
                ui,
                "Ядра",
                &format_core_count(report.cpu.physical_cores, report.cpu.logical_cores),
            );
            self.show_info_row(ui, "RAM", &format_memory_total(report.memory.total_bytes));
        });

        ui.add_space(10.0);
        self.show_info_card(ui, "DirectML / CUDA / ROCm", |ui| {
            self.show_info_row(
                ui,
                "NVIDIA",
                if report.gpu.nvidia_detected {
                    "обнаружена"
                } else {
                    "не обнаружена"
                },
            );
            self.show_info_row(
                ui,
                "CUDA",
                &report
                    .gpu
                    .cuda_version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "не обнаружена".to_string()),
            );
            self.show_info_row(
                ui,
                "NVIDIA SM",
                &report
                    .gpu
                    .nvidia_compute_capability
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "не определён".to_string()),
            );
            if let Some(architecture) = &report.gpu.nvidia_architecture {
                self.show_info_row(
                    ui,
                    "Архитектура NVIDIA",
                    &format_gpu_architecture(architecture),
                );
            }

            ui.add_space(8.0);
            self.show_info_row(
                ui,
                "AMD",
                if report.gpu.amd_detected {
                    "обнаружена"
                } else {
                    "не обнаружена"
                },
            );
            self.show_info_row(
                ui,
                "ROCm",
                &report
                    .gpu
                    .rocm_version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "не обнаружен".to_string()),
            );
            if let Some(installation) = &report.gpu.rocm_installation {
                self.show_info_row(
                    ui,
                    "ROCm в системе",
                    if installation.present {
                        "найден"
                    } else {
                        "не найден"
                    },
                );
            }
            if let Some(driver) = &report.gpu.linux_driver_status {
                self.show_info_row(ui, "amdgpu", bool_status(driver.amdgpu_loaded));
                self.show_info_row(ui, "/dev/kfd", bool_status(driver.kfd_available));
            }
            self.show_info_row(
                ui,
                "AMD архитектуры",
                &format_architecture_list(&report.gpu.amd_architectures),
            );
            self.show_info_row(
                ui,
                "ROCm 7.2 targets",
                &rocm_7_2_supported_llvm_targets().join(", "),
            );
            if let Some(validation) = &report.gpu.rocm_validation {
                let text = if validation.supported {
                    format!("поддерживается: {}", validation.reason)
                } else {
                    format!("не подтверждено: {}", validation.reason)
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

    fn show_general_tab(&mut self, ui: &mut Ui, action: &mut Option<PageNavAction>) {
        ui.label(theme::status("Папка с проектами:", theme::TEXT_MUTED));
        ui.horizontal(|ui| {
            let input_width = (ui.available_width() - 112.0).max(260.0);
            let response = ui.add_sized(
                [input_width, ui.spacing().interact_size.y.max(34.0)],
                egui::TextEdit::singleline(&mut self.projects_dir_input)
                    .hint_text("Выберите папку с проектами"),
            );
            if response.changed() {
                self.clear_success_status();
            }
            if theme::launcher_button(ui, "Обзор", egui::vec2(100.0, 34.0), true).clicked() {
                self.pick_projects_dir();
            }
        });

        ui.add_space(8.0);
        ui.label(theme::footer(
            "Используется страницами «Открыть главу», «Импорт главы», «Экспорт главы», а также окнами «Новый проект» и PSD-импорт.",
        ));

        ui.add_space(8.0);
        self.show_status(ui);

        ui.add_space(18.0);
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            let normalized_input = normalize_projects_dir_value(&self.projects_dir_input);
            let changed = normalized_input != self.saved_projects_dir;
            if theme::launcher_button(
                ui,
                "Сохранить папку проектов",
                egui::vec2(220.0, 36.0),
                changed,
            )
            .clicked()
            {
                match self.save_projects_root() {
                    Ok(projects_root) => {
                        *action = Some(PageNavAction::ProjectsRootChanged(projects_root));
                    }
                    Err(err) => {
                        self.status = SettingsStatus::Error(err);
                    }
                }
            }
        });
    }

    fn show_ai_computations_tab(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        self.ensure_ai_probe_started(ui);
        let action = self.poll_ai_probe(ui);

        ui.horizontal(|ui| {
            ui.label(theme::status(
                "Проверка выполняется из найденного Python-окружения через прямой импорт модулей.",
                theme::TEXT_MUTED,
            ));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let enabled = !matches!(self.ai_probe.status, AiProbeStatus::Running);
                if theme::launcher_button(ui, "Обновить", Vec2::new(112.0, 34.0), enabled).clicked()
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
                    "Фоновая проверка ИИ окружения завершилась без ответа.".to_string(),
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
                            ui.label(theme::status("Проверяется...", theme::TEXT_MUTED));
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
                            "Скомпилировал с поддержкой: проверяется",
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
            let label = package.version.as_deref().unwrap_or("версия неизвестна");
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
            ui.label(theme::status("Не установлен", STATUS_ERROR));
        }
    }

    fn show_ai_package_support(&self, ui: &mut Ui, values: &[String]) {
        let support_text = if values.is_empty() {
            "не определено".to_string()
        } else {
            values.join(", ")
        };
        ui.label(theme::status(
            &format!("Скомпилировал с поддержкой: {support_text}"),
            theme::TEXT_MUTED,
        ));
    }

    fn show_torch_upgrade_tab(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        self.poll_torch_upgrade(ui);
        let action = self
            .torch_upgrade
            .pending_ai_install_type_action
            .take()
            .map(PageNavAction::AiInstallTypeChanged);
        let installing_full_dependencies = self.ai_install_type == config::AiInstallType::Base;

        let description = if installing_full_dependencies {
            "Выберите PyTorch wheel. После PyTorch будут установлены полные torch-зависимости."
        } else {
            "Выберите другую версию PyTorch. Полные зависимости повторно устанавливаться не будут."
        };
        ui.label(theme::status(description, theme::TEXT_MUTED));
        ui.add_space(12.0);

        match self.torch_upgrade.status.clone() {
            TorchUpgradeStatus::Idle => {
                if theme::launcher_button(
                    ui,
                    "Проверить доступные версии PyTorch",
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
                        "Проверяем GPU / CUDA / ROCm...",
                        theme::TEXT_MUTED,
                    ));
                });
            }
            TorchUpgradeStatus::Choice(prompt) => {
                ui.label(theme::status(&prompt.summary, theme::TEXT_MAIN));
                ui.add_space(8.0);
                if !prompt.options.is_empty() {
                    ui.label(theme::status(
                        &format!(
                            "Рекомендуется: {}",
                            prompt.options[prompt.recommended_index].label
                        ),
                        theme::TEXT_MUTED,
                    ));
                    ui.add_space(8.0);
                    let options = prompt.options.clone();
                    for (idx, option) in options.into_iter().enumerate() {
                        let title = if idx == prompt.recommended_index {
                            format!("{} (Рекомендуется)", option.label)
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
                if theme::launcher_button(ui, "Оставить на CPU", Vec2::new(220.0, 34.0), true)
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
                ui.label(theme::status("Установка завершена.", theme::TEXT_MAIN));
                self.show_torch_upgrade_progress(ui);
                ui.add_space(8.0);
                if theme::launcher_button(ui, "Выбрать другую версию", Vec2::new(230.0, 34.0), true)
                    .clicked()
                {
                    self.start_torch_upgrade_preflight(ui);
                }
            }
            TorchUpgradeStatus::Error(message) => {
                ui.label(theme::status(&message, STATUS_ERROR));
                self.show_torch_upgrade_progress(ui);
                ui.add_space(8.0);
                if theme::launcher_button(ui, "Повторить", Vec2::new(140.0, 34.0), true).clicked()
                {
                    self.start_torch_upgrade_preflight(ui);
                }
            }
        }

        action
    }

    fn show_torch_upgrade_progress(&self, ui: &mut Ui) {
        ui.label(theme::status(
            &format!("Этап: {}", self.torch_upgrade.stage_label),
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

    fn start_torch_upgrade_preflight(&mut self, ui: &Ui) {
        let (tx, rx) = mpsc::channel();
        self.torch_upgrade = TorchUpgradeState {
            status: TorchUpgradeStatus::Preparing,
            rx: Some(rx),
            pending_ai_install_type_action: None,
            stage_progress: 0.0,
            stage_label: "Проверка GPU / CUDA / ROCm".to_string(),
            overall_progress: 0.0,
            overall_label: "Подготовка выбора PyTorch".to_string(),
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
        self.torch_upgrade.stage_label = "Запуск установки PyTorch".to_string();
        self.torch_upgrade.overall_progress = 0.0;
        self.torch_upgrade.overall_label = "Установка запущена".to_string();
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
                                self.torch_upgrade.status = TorchUpgradeStatus::Error(format!(
                                    "Установка завершена, но не удалось сохранить Full: {err}"
                                ));
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
                    .hint_text("Введите команду окружения"),
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
            "Enter отправляет команду. Ctrl+Enter оставляет перевод строки во вводе.",
        ));
    }

    fn ensure_python_console_started(&mut self, ui: &Ui) {
        if self.python_console.runtime.is_some() || self.python_console.attempted_start {
            return;
        }

        self.python_console.attempted_start = true;
        self.python_console
            .output
            .push_str("Запуск shell и активация Python-окружения...\n");

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
                    .push_str(&format!("Не удалось запустить консоль: {err}\n"));
            }
        }
    }

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
                        .push_str(&format!("\n[shell завершён: {}]\n", status));
                    received_any = true;
                }
                Ok(None) => {}
                Err(err) => {
                    runtime.terminated = true;
                    self.python_console
                        .output
                        .push_str(&format!("\n[не удалось получить статус shell: {err}]\n"));
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
                .push_str("[shell ещё не запущен]\n");
            return;
        };
        if runtime.terminated {
            self.python_console
                .output
                .push_str("[shell уже завершён]\n");
            return;
        }
        if let Err(err) = runtime.send_command(command) {
            self.python_console
                .output
                .push_str(&format!("[ошибка отправки команды: {err}]\n"));
            runtime_log::log_error(format!(
                "[launcher-settings] failed to send python console command: {err}"
            ));
        } else {
            ui.ctx().request_repaint();
        }
    }

    fn show_status(&self, ui: &mut Ui) {
        match &self.status {
            SettingsStatus::Idle => {
                ui.label(theme::status(
                    "Изменения применяются сразу после сохранения.",
                    theme::TEXT_MUTED,
                ));
            }
            SettingsStatus::Info(message) => {
                ui.label(theme::status(message, theme::TEXT_MUTED));
            }
            SettingsStatus::Success(message) => {
                ui.label(theme::status(message, theme::STATUS_SUCCESS));
            }
            SettingsStatus::Error(message) => {
                ui.label(theme::status(message, STATUS_ERROR));
            }
        }
    }
}

impl Drop for SettingsPageState {
    fn drop(&mut self) {
        if let Some(runtime) = self.python_console.runtime.as_mut() {
            runtime.terminate();
        }
    }
}

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
            .map_err(|err| format!("не удалось запустить shell для Python-окружения: {err}"))?;
        let stdin = child.stdin.take().ok_or_else(|| {
            "shell запущен без stdin, интерактивная консоль недоступна".to_string()
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "shell запущен без stdout, вывод консоли недоступен".to_string())?;
        let stderr = child.stderr.take().ok_or_else(|| {
            "shell запущен без stderr, вывод ошибок консоли недоступен".to_string()
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
                self.send_command(shell_echo_command(&format!(
                    "Python-окружение не найдено: {err}"
                )))?;
            }
        }
        Ok(())
    }

    fn send_command(&self, command: String) -> Result<(), String> {
        self.command_tx
            .send(command)
            .map_err(|err| format!("канал shell-команд закрыт: {err}"))
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

fn normalize_projects_dir_value(raw_value: &str) -> String {
    let trimmed = raw_value.trim();
    if trimmed.is_empty() {
        return config::default_projects_root()
            .to_string_lossy()
            .into_owned();
    }
    PathBuf::from(trimmed).to_string_lossy().into_owned()
}

fn persist_projects_root(projects_dir: &str) -> anyhow::Result<()> {
    let mut cfg = config::load_user_config()?;
    cfg.set_path(
        &["General", config::GENERAL_PROJECTS_DIR_KEY],
        Value::String(projects_dir.to_string()),
    )?;
    Ok(())
}

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
        let message = format!("Не удалось запустить фоновую проверку системы: {err}");
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
        name: detect_cpu_name().unwrap_or_else(|| "CPU не определён".to_string()),
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

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
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

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
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

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn detect_total_memory_bytes() -> Option<u64> {
    None
}

fn format_core_count(physical: Option<usize>, logical: usize) -> String {
    match physical {
        Some(value) => format!("{value} физических / {logical} логических"),
        None => format!("{logical} логических"),
    }
}

fn format_memory_total(total_bytes: Option<u64>) -> String {
    let Some(bytes) = total_bytes else {
        return "не определено".to_string();
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
        "не определена".to_string()
    } else {
        parts.join(" / ")
    }
}

fn format_architecture_list(architectures: &[GpuArchitecture]) -> String {
    if architectures.is_empty() {
        return "не определены".to_string();
    }
    architectures
        .iter()
        .map(format_gpu_architecture)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_directml_accelerators(accelerators: &[DirectMlAccelerator]) -> String {
    if !cfg!(target_os = "windows") {
        return "доступно только на Windows".to_string();
    }
    if accelerators.is_empty() {
        return "совместимые ускорители не обнаружены".to_string();
    }
    accelerators
        .iter()
        .map(|accelerator| accelerator.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

fn bool_status(value: bool) -> &'static str {
    if value { "да" } else { "нет" }
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

fn spawn_console_writer_thread(
    stdin: std::process::ChildStdin,
    command_rx: Receiver<String>,
    event_tx: Sender<PythonConsoleEvent>,
) {
    thread::spawn(move || {
        let mut writer = BufWriter::new(stdin);
        for command in command_rx {
            if let Err(err) = writer.write_all(command.as_bytes()) {
                let _ = event_tx.send(PythonConsoleEvent::Error(format!(
                    "\n[ошибка записи в shell: {err}]\n"
                )));
                return;
            }
            if let Err(err) = writer.write_all(shell_line_ending().as_bytes()) {
                let _ = event_tx.send(PythonConsoleEvent::Error(format!(
                    "\n[ошибка завершения строки shell: {err}]\n"
                )));
                return;
            }
            if let Err(err) = writer.flush() {
                let _ = event_tx.send(PythonConsoleEvent::Error(format!(
                    "\n[ошибка flush shell: {err}]\n"
                )));
                return;
            }
        }
    });
}

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
                    let _ = event_tx.send(PythonConsoleEvent::Error(format!(
                        "\n[ошибка чтения shell: {err}]\n"
                    )));
                    return;
                }
            }
        }
    });
}

fn trim_single_trailing_newline(value: &mut String) {
    if value.ends_with("\r\n") {
        value.truncate(value.len().saturating_sub(2));
        return;
    }
    if value.ends_with('\n') {
        value.pop();
    }
}

#[cfg(target_os = "windows")]
fn build_python_console_shell_command() -> Command {
    let mut command = Command::new("powershell");
    command
        .arg("-NoLogo")
        .arg("-NoExit")
        .arg("-ExecutionPolicy")
        .arg("Bypass");
    command
}

#[cfg(target_os = "windows")]
fn configure_shell_encoding_command() -> String {
    "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; [Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $OutputEncoding = [System.Text.Encoding]::UTF8".to_string()
}

#[cfg(not(target_os = "windows"))]
fn configure_shell_encoding_command() -> String {
    "export LANG=C.UTF-8; export LC_ALL=C.UTF-8".to_string()
}

#[cfg(not(target_os = "windows"))]
fn build_python_console_shell_command() -> Command {
    Command::new("sh")
}

#[cfg(target_os = "windows")]
fn apply_hidden_process_flags(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn apply_hidden_process_flags(_command: &mut Command) {}

#[cfg(target_os = "windows")]
fn shell_line_ending() -> &'static str {
    "\r\n"
}

#[cfg(not(target_os = "windows"))]
fn shell_line_ending() -> &'static str {
    "\n"
}

#[cfg(target_os = "windows")]
fn change_directory_command(path: &Path) -> String {
    format!("Set-Location -LiteralPath '{}'", powershell_escape(path))
}

#[cfg(not(target_os = "windows"))]
fn change_directory_command(path: &Path) -> String {
    format!("cd '{}'", sh_escape(path))
}

#[cfg(target_os = "windows")]
fn python_shell_kind() -> PythonShellKind {
    PythonShellKind::PowerShell
}

#[cfg(not(target_os = "windows"))]
fn python_shell_kind() -> PythonShellKind {
    PythonShellKind::PosixSh
}

#[cfg(target_os = "windows")]
fn shell_echo_command(message: &str) -> String {
    format!("Write-Output '{}'", powershell_escape_str(message))
}

#[cfg(not(target_os = "windows"))]
fn shell_echo_command(message: &str) -> String {
    format!("printf '%s\n' '{}'", sh_escape_str(message))
}

#[cfg(target_os = "windows")]
fn powershell_escape(path: &Path) -> String {
    powershell_escape_str(&path.to_string_lossy())
}

#[cfg(target_os = "windows")]
fn powershell_escape_str(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(not(target_os = "windows"))]
fn sh_escape(path: &Path) -> String {
    sh_escape_str(&path.to_string_lossy())
}

#[cfg(not(target_os = "windows"))]
fn sh_escape_str(value: &str) -> String {
    value.replace('\'', r"'\''")
}
