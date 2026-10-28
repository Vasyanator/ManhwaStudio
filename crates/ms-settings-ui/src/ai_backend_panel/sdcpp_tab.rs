/*
FILE OVERVIEW: crates/ms-settings-ui/src/ai_backend_panel/sdcpp_tab.rs
The «Stable-Diffusion.cpp» tab of the AI pane: which sd.cpp build this machine uses, whether
it is installed, whether the host meets its requirements, the explicit install / cancel /
remove of its package, and — once installed — the device probe, the device and CPU-thread
choice, the library version and stopping the worker process.

Key structures:
- `SdCppTabState`: the picked build, the refresh-on-open latch, the install / remove job slot
  and the last VC++-link error.
- `PackageJob` / `JobKind`: the one install or remove the tab runs at a time (the job pattern
  of the ORT download in `ort_build.rs`).

Key functions:
- `draw_sdcpp_tab`: renders the intro, the build combo, the status (installed / requirement
  problem / VC++ runtime link) and the job buttons with their progress.
- `draw_runtime_section`: «Найти устройства» (automatic once per open), version line, device
  combo (`draw_device_combo`) with the missing-device warning, threads slider, «Остановить
  процесс sd.cpp» while `worker_running`.
- `device_options`, `device_label`, `saved_device_missing`: the device choice rules (pure,
  unit-tested).
- `sdcpp_stage_label`: the localized text of a package stage (pure, unit-tested).
- `size_mib_label`: an archive size in MiB with one decimal (pure, unit-tested).

Notes:
Desktop only (declared under `cfg(not(target_arch = "wasm32"))` in `mod.rs`); the tab is
listed only where `PlatformFeature::SdCppRuntime` is available. Every probe, config write,
install and remove runs on an `ms_thread` worker: the GUI thread reads
`ms_sdcpp::snapshot()` (one short lock) and the job slot; the probe answer reaches it only
through that status (`SdCppService::probe` publishes it). Device and threads are read once from
`user_config` on a worker and saved through `ms_config::save_sdcpp_device` / `save_sdcpp_threads`.
Under `--no-ai` (`AiBackendHandle::local_ai.sdcpp`) the tab is shown but disabled.
*/

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ms_local_ai::local_ai::{AiRouteKind, LocalAiOffReason};
use ms_native_package::{PackageProgress, PackageStage};
use ms_config::{SdCppChoice, SdCppDeviceChoice};
use ms_sdcpp::{DeviceInfo, ProbeReport, RequirementProblem, SdCppBuild, SdCppError, SdCppStatus};
use ms_widgets::{WheelComboBox, WheelSlider};

use crate::ai_backend_supervisor::AiBackendHandle;

/// What the tab's background job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    /// Download, verify and unpack the build's package (`ms_sdcpp::install`).
    Install,
    /// Delete the build's package (`ms_sdcpp::remove`).
    Remove,
}

/// The tab's one install or remove job: written by the worker, read by the GUI thread.
#[derive(Debug, Default)]
struct PackageJob {
    /// The running or last job, with the build it acts on; `None` before the first one.
    current: Option<(JobKind, SdCppBuild)>,
    /// Whether the worker runs.
    running: bool,
    /// The latest install progress (cleared when the job ends).
    progress: Option<PackageProgress>,
    /// How the last job failed, localized (`None` while running and after a success).
    error: Option<String>,
    /// Whether the last job was a cancelled install (shown as a warning, not an error; its
    /// staged bytes are kept and the next install resumes them).
    cancelled: bool,
    /// Cancel flag of the running install; a fresh flag per job.
    cancel: Arc<AtomicBool>,
}

/// Per-pane state of the «Stable-Diffusion.cpp» tab.
#[derive(Debug)]
pub(super) struct SdCppTabState {
    /// The build shown in the combo; `None` until seeded from the refreshed status.
    picked: Option<SdCppBuild>,
    /// Asks the status worker for a refresh at the next draw (set on open).
    refresh_pending: bool,
    /// The install / remove job slot.
    job: Arc<Mutex<PackageJob>>,
    /// Why opening the VC++ runtime download link failed (localized), if it did.
    vc_redist_error: Option<String>,
    /// Runs the automatic device probe at the next draw that finds the build installed (set
    /// on open; cleared when the probe starts).
    auto_probe_pending: bool,
    /// Whether a probe worker runs (its result lands in the sd.cpp status).
    probe_running: Arc<AtomicBool>,
    /// Whether an unload worker («Остановить процесс sd.cpp») runs.
    stop_running: Arc<AtomicBool>,
    /// The persisted device / threads, read once off the GUI thread; `None` until it lands.
    choice_read: Arc<Mutex<Option<SdCppChoice>>>,
    /// Whether the one-shot choice read has started.
    choice_read_started: bool,
    /// The device shown in the combo; `None` until seeded from `choice_read`.
    device: Option<SdCppDeviceChoice>,
    /// The thread count shown on the slider (`0` = auto); `None` until seeded.
    threads: Option<u32>,
}

impl Default for SdCppTabState {
    /// A fresh tab refreshes the status and probes the devices on its first draw.
    fn default() -> Self {
        Self {
            picked: None,
            refresh_pending: true,
            job: Arc::default(),
            vc_redist_error: None,
            auto_probe_pending: true,
            probe_running: Arc::default(),
            stop_running: Arc::default(),
            choice_read: Arc::default(),
            choice_read_started: false,
            device: None,
            threads: None,
        }
    }
}

impl SdCppTabState {
    /// Marks the tab as just opened: the next draw asks the status worker to re-probe the
    /// install and the requirements, and probes the devices once the build is installed.
    pub(super) fn request_refresh_on_open(&mut self) {
        self.refresh_pending = true;
        self.auto_probe_pending = true;
    }
}

/// Locks the job slot. A poisoned lock still holds a consistent job: every update is a set
/// of plain field stores that cannot panic halfway.
fn lock_job(job: &Mutex<PackageJob>) -> std::sync::MutexGuard<'_, PackageJob> {
    job.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The localized text of a package stage (a retry names its countdown and attempt).
fn sdcpp_stage_label(stage: PackageStage) -> String {
    match stage {
        PackageStage::Probing => t!("ai_backend.sdcpp.stage_probing").to_owned(),
        PackageStage::WaitingForOtherDownload => t!("ai_backend.sdcpp.stage_waiting").to_owned(),
        PackageStage::Downloading => t!("ai_backend.sdcpp.stage_downloading").to_owned(),
        PackageStage::Retrying { attempt, max_attempts, delay_secs } => {
            tf!("ai_backend.sdcpp.stage_retrying", seconds = delay_secs, attempt = attempt, max = max_attempts)
        }
        PackageStage::Verifying => t!("ai_backend.sdcpp.stage_verifying").to_owned(),
        PackageStage::Extracting => t!("ai_backend.sdcpp.stage_extracting").to_owned(),
        PackageStage::Done => t!("ai_backend.sdcpp.stage_done").to_owned(),
    }
}

/// `bytes` in MiB with one decimal, rounded half up (`"38.2"`), in integer arithmetic. The
/// house convention for download sizes is binary units labelled "MB" / «МБ» with one decimal
/// (`ms-tab-translation` `ocr_model_download::format_bytes`, crate-private there); the pinned
/// archives are tens of MiB, so this one unit suffices.
fn size_mib_label(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    // Tenths of a MiB, rounded: (bytes * 10 + MIB / 2) / MIB, saturating for absurd sizes.
    let tenths = bytes.saturating_mul(10).saturating_add(MIB / 2) / MIB;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// Renders the «Stable-Diffusion.cpp» tab against the process-global sd.cpp status and this
/// pane's `state`. Disabled (with the reason) when the process may not use sd.cpp.
pub(super) fn draw_sdcpp_tab(ui: &mut egui::Ui, handle: &AiBackendHandle, state: &mut SdCppTabState) {
    if state.refresh_pending {
        state.refresh_pending = false;
        ms_sdcpp::request_refresh();
    }

    ui.heading(t!("ai_backend.tab_sdcpp"));
    ui.small(t!("ai_backend.sdcpp.intro_hint"));

    if let Err(off) = &handle.local_ai.sdcpp {
        let message = match off.reason {
            LocalAiOffReason::DisabledByFlag => t!("ai_backend.sdcpp.disabled_no_ai").to_owned(),
            LocalAiOffReason::UnavailableOnPlatform(_) => off.user_message(),
        };
        ui.colored_label(ms_theme::status::WARNING, message);
    }
    // The route-availability owner decides whether this process may run sd.cpp at all; under
    // `--no-ai` nothing here may start a worker process, install or remove (the tab is only
    // shown, disabled).
    let allowed = AiRouteKind::SdCpp.availability(&handle.local_ai).is_ok();

    // The catalog has no package for this OS / architecture: say so instead of waiting for a
    // selection that can never come.
    if ms_sdcpp::builds_here().is_empty() {
        ui.colored_label(ms_theme::status::WARNING, t!("ai_backend.sdcpp.no_build_here_status"));
        return;
    }

    let status = ms_sdcpp::snapshot();
    if state.picked.is_none() {
        state.picked = status.selected;
    }
    ui.add_enabled_ui(allowed, |ui| draw_tab_body(ui, state, &status, allowed));
}

/// Whether the tab should start its automatic device probe now: only when the process may
/// use sd.cpp (`allowed`; never under `--no-ai`, a probe starts a worker process), an
/// automatic probe is pending for this open, and neither a probe nor an install / remove job
/// runs.
fn auto_probe_due(allowed: bool, pending: bool, probe_running: bool, job_running: bool) -> bool {
    allowed && pending && !probe_running && !job_running
}

/// The tab body below the intro: build combo, status lines and the job controls. `allowed` is
/// the sd.cpp route availability: when `false` (`--no-ai`) no worker, probe, install or
/// remove is started, even from code paths a disabled UI would not stop.
fn draw_tab_body(ui: &mut egui::Ui, state: &mut SdCppTabState, status: &SdCppStatus, allowed: bool) {
    let Some(picked) = state.picked else {
        // No refresh answered yet ("nothing offered here" returned earlier): nothing to pick
        // from.
        checking_line(ui);
        return;
    };
    let job_running = lock_job(&state.job).running;

    draw_build_combo(ui, state, picked, job_running);
    let picked = state.picked.unwrap_or(picked);

    // The status describes the persisted selection; right after a pick it still names the old
    // build until the save and the refresh landed.
    let installed = if status.selected == Some(picked) { status.installed } else { None };
    match installed {
        None => checking_line(ui),
        Some(true) => {
            ui.colored_label(ms_theme::status::SUCCESS, tf!("ai_backend.sdcpp.status_installed", build = picked.label()));
        }
        Some(false) => {
            ui.label(t!("ai_backend.sdcpp.status_not_installed"));
        }
    }
    if status.selected == Some(picked) {
        draw_requirement_problem(ui, state, status);
    }

    draw_job_controls(ui, state, picked, installed, allowed);
    if installed == Some(true) {
        draw_runtime_section(ui, state, picked, status, job_running, allowed);
    }
}

/// The installed build's runtime settings: «Найти устройства» (automatic once per open), the
/// library version, the device combo with its missing-device warning, the CPU threads and
/// «Остановить процесс sd.cpp» while a worker process runs. The probe, the config read and
/// every write run on workers; the probe's answer is read from `status`.
fn draw_runtime_section(ui: &mut egui::Ui, state: &mut SdCppTabState, picked: SdCppBuild, status: &SdCppStatus, job_running: bool, allowed: bool) {
    ui.separator();
    start_choice_read(state);
    if let Some(choice) = state.choice_read.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        state.device.get_or_insert_with(|| choice.device.clone());
        state.threads.get_or_insert(choice.threads);
    }

    let probe_running = state.probe_running.load(Ordering::SeqCst);
    if auto_probe_due(allowed, state.auto_probe_pending, probe_running, job_running) {
        state.auto_probe_pending = false;
        start_probe(&state.probe_running, picked);
    }
    ui.horizontal(|ui| {
        if ui.add_enabled(allowed && !probe_running && !job_running, egui::Button::new(t!("ai_backend.sdcpp.find_devices_button"))).clicked() {
            start_probe(&state.probe_running, picked);
        }
        if probe_running {
            ui.spinner();
            ui.label(t!("ai_backend.sdcpp.probing_status"));
        }
    });

    let report: Option<&ProbeReport> = match &status.probe {
        Some(Ok(report)) => Some(report),
        Some(Err(err)) => {
            ui.colored_label(ms_theme::status::ERROR, tf!("ai_backend.sdcpp.probe_failed_status", error = err));
            None
        }
        None => None,
    };
    if let Some(report) = report {
        ui.small(tf!("ai_backend.sdcpp.version_status", version = report.sd_version, commit = report.sd_commit));
    }

    draw_device_combo(ui, state, report);
    if let (Some(report), Some(threads)) = (report, state.threads.as_mut()) {
        // A saved count above this machine's cores stays representable (the range grows to it)
        // instead of being clamped and silently rewritten.
        let max = report.physical_cores.max(*threads);
        let response = ui.add(WheelSlider::new(threads, 0..=max).text(t!("ai_backend.sdcpp.threads_label")));
        if *threads == 0 {
            ui.small(t!("ai_backend.sdcpp.threads_auto"));
        }
        // One write per gesture: on release of a drag, or per click / wheel step.
        if response.drag_stopped() || (response.changed() && !response.dragged()) {
            spawn_save_threads(*threads);
        }
    }

    if status.worker_running {
        ui.label(t!("ai_backend.sdcpp.worker_running_status"));
        let stopping = state.stop_running.load(Ordering::SeqCst);
        if ui.add_enabled(allowed && !stopping, egui::Button::new(t!("ai_backend.sdcpp.stop_worker_button"))).clicked() {
            spawn_unload(&state.stop_running);
        }
    }
}

/// The device combo: «Авто» plus every probed device (`name — description`), and the saved
/// device when the last probe did not find it (with a warning naming it). A pick is saved
/// off-thread. Hidden until the persisted choice was read.
fn draw_device_combo(ui: &mut egui::Ui, state: &mut SdCppTabState, report: Option<&ProbeReport>) {
    let Some(current) = state.device.clone() else {
        return;
    };
    let devices = report.map_or(&[][..], |report| report.devices.as_slice());
    let mut options = device_options(devices);
    if !options.contains(&current) {
        options.push(current.clone());
    }
    let mut selected = current.clone();
    WheelComboBox::from_label(t!("ai_backend.sdcpp.device_label"))
        .id_salt("ai_backend.sdcpp.device_label")
        .selected_text(device_label(&selected))
        .show_ui(ui, |ui| {
            for option in &options {
                ui.selectable_value(&mut selected, option.clone(), device_label(option));
            }
        });
    if report.is_some() && saved_device_missing(&current, devices) {
        ui.colored_label(
            ms_theme::status::WARNING,
            tf!("ai_backend.sdcpp.device_missing_status", device = device_label(&current), auto = t!("ai_backend.sdcpp.device_auto")),
        );
    }
    if selected != current {
        spawn_save_device(selected.clone());
        state.device = Some(selected);
    }
}

/// «Авто» followed by one named choice per probed device, in probe order.
fn device_options(devices: &[DeviceInfo]) -> Vec<SdCppDeviceChoice> {
    std::iter::once(SdCppDeviceChoice::Auto)
        .chain(devices.iter().map(|device| SdCppDeviceChoice::Named { name: device.name.clone(), description: device.description.clone() }))
        .collect()
}

/// The combo text of a device choice: «Авто», or `name — description` (just the name when
/// the description is empty).
fn device_label(choice: &SdCppDeviceChoice) -> String {
    match choice {
        SdCppDeviceChoice::Auto => t!("ai_backend.sdcpp.device_auto").to_owned(),
        SdCppDeviceChoice::Named { name, description } if description.trim().is_empty() => name.clone(),
        SdCppDeviceChoice::Named { name, description } => format!("{name} — {description}"),
    }
}

/// Whether the saved device is a named one the probe did not report. Devices are matched by
/// name (the key sd.cpp selects by); «Авто» is never missing.
fn saved_device_missing(saved: &SdCppDeviceChoice, devices: &[DeviceInfo]) -> bool {
    match saved {
        SdCppDeviceChoice::Auto => false,
        SdCppDeviceChoice::Named { name, .. } => !devices.iter().any(|device| &device.name == name),
    }
}

/// Starts the one-shot read of the persisted sd.cpp choice (device, threads) on a worker.
fn start_choice_read(state: &mut SdCppTabState) {
    if state.choice_read_started {
        return;
    }
    state.choice_read_started = true;
    let slot = state.choice_read.clone();
    let spawned = ms_thread::Builder::new().name("sdcpp-choice-read".to_owned()).spawn(move || {
        // An unreadable config reads as the defaults (auto device, auto threads); the reader
        // logs malformed values itself.
        let config = match ms_config::load_raw_user_settings_for_startup() {
            Ok(config) => config,
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[ai-backend-panel] cannot read user_config for the sd.cpp device / threads; showing the defaults: {err:#}"));
                serde_json::Value::Null
            }
        };
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(ms_config::sdcpp_choice_from_user_settings(&config));
    });
    if let Err(err) = spawned {
        ms_log::runtime_log::log_error(format!("[ai-backend-panel] failed to start the sd.cpp choice read thread: {err}"));
    }
}

/// Probes `build` on a worker (`SdCppService::probe` publishes the answer into the status)
/// unless a probe of this tab runs.
fn start_probe(running: &Arc<AtomicBool>, build: SdCppBuild) {
    if running.swap(true, Ordering::SeqCst) {
        return;
    }
    let flag = running.clone();
    let spawned = ms_thread::Builder::new().name("sdcpp-probe".to_owned()).spawn(move || {
        // Nothing cancels a probe from the tab; the worker it starts is short-lived.
        let cancel = AtomicBool::new(false);
        // The result is published into the status by the service (and logged on failure).
        if let Ok(report) = ms_sdcpp::service().probe(build, &cancel) {
            ms_log::runtime_log::log_info(format!("[ai-backend-panel] sd.cpp probe of '{}': {} device(s)", build.wire(), report.devices.len()));
        }
        flag.store(false, Ordering::SeqCst);
    });
    if let Err(err) = spawned {
        ms_log::runtime_log::log_error(format!("[ai-backend-panel] failed to start the sd.cpp probe thread: {err}"));
        running.store(false, Ordering::SeqCst);
    }
}

/// Stops the sd.cpp worker process on a worker thread (`unload` may wait for it to exit).
fn spawn_unload(running: &Arc<AtomicBool>) {
    if running.swap(true, Ordering::SeqCst) {
        return;
    }
    let flag = running.clone();
    let spawned = ms_thread::Builder::new().name("sdcpp-unload".to_owned()).spawn(move || {
        ms_sdcpp::service().unload();
        flag.store(false, Ordering::SeqCst);
    });
    if let Err(err) = spawned {
        ms_log::runtime_log::log_error(format!("[ai-backend-panel] failed to start the sd.cpp unload thread: {err}"));
        running.store(false, Ordering::SeqCst);
    }
}

/// Persists the device choice as `General.sdcpp_device` on a worker, then asks the status
/// worker to refresh (the editors re-read the choice when the status revision moves).
fn spawn_save_device(device: SdCppDeviceChoice) {
    let path = ms_config::user_config_path();
    let spawned = ms_thread::Builder::new().name("sdcpp-device-save".to_owned()).spawn(move || {
        if let Err(err) = ms_config::save_sdcpp_device(&path, &device) {
            ms_log::runtime_log::log_error(format!("[ai-backend-panel] failed to persist sdcpp_device={device:?}: {err}"));
        }
        // The editors' readiness (device found / missing) follows the status revision.
        ms_sdcpp::request_refresh();
    });
    if let Err(err) = spawned {
        ms_log::runtime_log::log_error(format!("[ai-backend-panel] failed to start the sd.cpp device save thread: {err}"));
    }
}

/// Persists the CPU thread count as `General.sdcpp_threads` on a worker (`0` = auto), then asks
/// the status worker to refresh.
fn spawn_save_threads(threads: u32) {
    let path = ms_config::user_config_path();
    let spawned = ms_thread::Builder::new().name("sdcpp-threads-save".to_owned()).spawn(move || {
        if let Err(err) = ms_config::save_sdcpp_threads(&path, threads) {
            ms_log::runtime_log::log_error(format!("[ai-backend-panel] failed to persist sdcpp_threads={threads}: {err}"));
        }
        // Readers that re-read the choice on a status revision see the new count.
        ms_sdcpp::request_refresh();
    });
    if let Err(err) = spawned {
        ms_log::runtime_log::log_error(format!("[ai-backend-panel] failed to start the sd.cpp threads save thread: {err}"));
    }
}

/// A spinner with the "checking" status text.
fn checking_line(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(t!("ai_backend.sdcpp.status_checking"));
    });
}

/// The build combo over the builds the catalog offers here. A pick is saved off-thread
/// (`General.sdcpp_build`) and the status is refreshed after the write. Disabled while a job
/// runs, so the job and the shown build never diverge.
fn draw_build_combo(ui: &mut egui::Ui, state: &mut SdCppTabState, picked: SdCppBuild, job_running: bool) {
    let mut selected = picked;
    ui.add_enabled_ui(!job_running, |ui| {
        WheelComboBox::from_label(t!("ai_backend.sdcpp.build_label"))
            .id_salt("ai_backend.sdcpp.build_label")
            .selected_text(selected.label())
            .show_ui(ui, |ui| {
                for build in ms_sdcpp::builds_here() {
                    ui.selectable_value(&mut selected, build, build.label());
                }
            });
    });
    if selected != picked {
        state.picked = Some(selected);
        spawn_save_build(selected);
    }
}

/// Persists `build` as `General.sdcpp_build` on a worker, then asks the status worker to
/// refresh (whatever the write's result: a failed write re-confirms the old selection).
fn spawn_save_build(build: SdCppBuild) {
    let path = ms_config::user_config_path();
    let spawned = ms_thread::Builder::new().name("sdcpp-build-save".to_owned()).spawn(move || {
        if let Err(err) = ms_config::save_sdcpp_build(&path, Some(build.wire())) {
            ms_log::runtime_log::log_error(format!("[ai-backend-panel] failed to persist sdcpp_build='{}': {err}", build.wire()));
        }
        ms_sdcpp::request_refresh();
    });
    if let Err(err) = spawned {
        ms_log::runtime_log::log_error(format!("[ai-backend-panel] failed to start the sd.cpp build save thread: {err}"));
    }
}

/// The selected build's requirement problem (if the host fails one) and, for a missing or
/// old VC++ runtime, the button that opens Microsoft's download link.
fn draw_requirement_problem(ui: &mut egui::Ui, state: &mut SdCppTabState, status: &SdCppStatus) {
    let Some(Err(problem)) = &status.requirements else {
        return;
    };
    ui.colored_label(ms_theme::status::ERROR, problem.to_string());
    let vc_runtime = match problem {
        RequirementProblem::VcRuntimeMissing { .. } | RequirementProblem::VcRuntimeTooOld { .. } => true,
        RequirementProblem::NotGlibc | RequirementProblem::GlibcTooOld { .. } | RequirementProblem::VulkanLoaderMissing { .. } => false,
    };
    if !vc_runtime {
        return;
    }
    if ui.button(t!("ai_backend.sdcpp.vc_redist_button")).clicked() {
        // `open_url` only spawns the handler process (no wait), so it is fine on the GUI thread.
        state.vc_redist_error = match ms_platform::shell_open::open_url(ms_sdcpp::VC_REDIST_URL) {
            Ok(()) => None,
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[ai-backend-panel] could not open the VC++ runtime link {}: {err}", ms_sdcpp::VC_REDIST_URL));
                Some(err.user_message())
            }
        };
    }
    if let Some(error) = &state.vc_redist_error {
        ui.colored_label(ms_theme::status::ERROR, error);
    }
}

/// Install (with the archive size) / Cancel / Remove (disabled with a hint while a job or an
/// sd.cpp generation runs), the running job's progress and the last job's failure.
fn draw_job_controls(ui: &mut egui::Ui, state: &SdCppTabState, picked: SdCppBuild, installed: Option<bool>, allowed: bool) {
    let (running, current, progress, error, cancelled, cancel) = {
        let job = lock_job(&state.job);
        (job.running, job.current, job.progress, job.error.clone(), job.cancelled, job.cancel.clone())
    };

    ui.horizontal_wrapped(|ui| match installed {
        Some(false) => {
            let size = ms_sdcpp::row_here(picked).map(|row| row.size).unwrap_or_default();
            let install = egui::Button::new(tf!("ai_backend.sdcpp.install_button", size = size_mib_label(size)));
            if ui.add_enabled(allowed && !running, install).clicked() {
                start_job(&state.job, JobKind::Install, picked);
            }
        }
        Some(true) => {
            // A generation has the package's libraries loaded: removing them under it is
            // refused here (one atomic load, cheap per frame).
            let generating = ms_sdcpp::service().is_generating();
            let hint = if generating { t!("ai_backend.sdcpp.remove_generating_hint") } else { t!("ai_backend.sdcpp.remove_blocked_hint") };
            let remove = ui
                .add_enabled(allowed && !running && !generating, egui::Button::new(t!("ai_backend.sdcpp.remove_button")))
                .on_disabled_hover_text(hint);
            if remove.clicked() {
                start_job(&state.job, JobKind::Remove, picked);
            }
        }
        None => {}
    });

    if running {
        draw_progress(ui, progress);
        if current.is_some_and(|(kind, _)| kind == JobKind::Install) {
            // The worker polls the flag in the busy wait, the transfer, the retry wait and the
            // hash pass, then ends with `Cancelled`.
            let requested = cancel.load(Ordering::Relaxed);
            if ui.add_enabled(!requested, egui::Button::new(t!("ai_backend.sdcpp.cancel_button"))).clicked() {
                cancel.store(true, Ordering::Relaxed);
            }
        }
        return;
    }
    // A failure belongs to the build it happened on; after another pick it is stale.
    if current.is_some_and(|(_, build)| build == picked)
        && let Some(error) = error
    {
        let color = if cancelled { ms_theme::status::WARNING } else { ms_theme::status::ERROR };
        ui.colored_label(color, error);
    }
}

/// The running job's progress: a determinate bar when the size is known (and not in a retry
/// countdown), else a spinner with the stage.
fn draw_progress(ui: &mut egui::Ui, progress: Option<PackageProgress>) {
    let Some(progress) = progress else {
        ui.spinner();
        return;
    };
    let stage = sdcpp_stage_label(progress.stage);
    let retrying = matches!(progress.stage, PackageStage::Retrying { .. });
    match progress.total {
        Some(total) if total > 0 && !retrying => {
            let done = progress.downloaded.min(total);
            // Per-mille in integer arithmetic (`done <= total`, so 0..=1000, which fits a u16
            // and converts to f32 exactly); the bar needs no finer step.
            let permille = u16::try_from(done.saturating_mul(1000) / total).unwrap_or(1000);
            let fraction = f32::from(permille) / 1000.0;
            ui.add(egui::ProgressBar::new(fraction).text(format!("{stage} {}%", permille / 10)));
        }
        Some(_) | None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("{stage}…"));
            });
        }
    }
}

/// Starts `kind` for `build` on a worker unless a job runs. The worker records progress, the
/// localized failure (or the cancel) and clears `running` when done; `ms_sdcpp::install` /
/// `remove` refresh the status themselves.
fn start_job(slot: &Arc<Mutex<PackageJob>>, kind: JobKind, build: SdCppBuild) {
    let cancel = {
        let mut job = lock_job(slot);
        if job.running {
            return;
        }
        job.current = Some((kind, build));
        job.running = true;
        job.progress = None;
        job.error = None;
        job.cancelled = false;
        job.cancel = Arc::new(AtomicBool::new(false));
        job.cancel.clone()
    };
    let worker_slot = slot.clone();
    let name = match kind {
        JobKind::Install => "sdcpp-install",
        JobKind::Remove => "sdcpp-remove",
    };
    let spawned = ms_thread::Builder::new().name(name.to_owned()).spawn(move || {
        let result = match kind {
            JobKind::Install => {
                let progress_slot = worker_slot.clone();
                let mut report = |progress: PackageProgress| lock_job(&progress_slot).progress = Some(progress);
                ms_sdcpp::install(build, &cancel, &mut report).map(|_dir| ())
            }
            JobKind::Remove => ms_sdcpp::remove(build),
        };
        finish_job(&worker_slot, kind, build, result);
    });
    if let Err(err) = spawned {
        ms_log::runtime_log::log_error(format!("[ai-backend-panel] failed to start the {name} thread: {err}"));
        let mut job = lock_job(slot);
        job.running = false;
        job.error = Some(tf!("ai_backend.sdcpp.worker_start_error", error = err));
    }
}

/// Records how a job ended (the worker's last step).
fn finish_job(slot: &Mutex<PackageJob>, kind: JobKind, build: SdCppBuild, result: Result<(), SdCppError>) {
    let mut job = lock_job(slot);
    job.running = false;
    job.progress = None;
    match result {
        Ok(()) => {
            ms_log::runtime_log::log_info(format!("[ai-backend-panel] sd.cpp {kind:?} of build '{}' finished", build.wire()));
        }
        Err(err) => {
            job.cancelled = matches!(err, SdCppError::Cancelled);
            if !job.cancelled {
                // The log keeps the typed (English) form; the tab shows the localized one.
                ms_log::runtime_log::log_warn(format!("[ai-backend-panel] sd.cpp {kind:?} of build '{}' failed: {err:?}", build.wire()));
            }
            job.error = Some(err.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{auto_probe_due, device_label, device_options, saved_device_missing, sdcpp_stage_label, size_mib_label};
    use ms_config::SdCppDeviceChoice;
    use ms_native_package::PackageStage;
    use ms_sdcpp::DeviceInfo;

    fn device(name: &str, description: &str) -> DeviceInfo {
        DeviceInfo { name: name.to_owned(), description: description.to_owned() }
    }

    fn named(name: &str, description: &str) -> SdCppDeviceChoice {
        SdCppDeviceChoice::Named { name: name.to_owned(), description: description.to_owned() }
    }

    /// The automatic probe (it starts a worker process) never runs under `--no-ai`, and
    /// otherwise only once per open while nothing else runs.
    #[test]
    fn auto_probe_is_gated_on_the_route_and_idle_jobs() {
        assert!(auto_probe_due(true, true, false, false));
        assert!(!auto_probe_due(false, true, false, false), "--no-ai must never start a probe");
        assert!(!auto_probe_due(true, false, false, false), "already probed this open");
        assert!(!auto_probe_due(true, true, true, false), "a probe runs");
        assert!(!auto_probe_due(true, true, false, true), "an install / remove runs");
    }

    /// «Авто» leads, then every probed device in probe order with its description.
    #[test]
    fn device_options_start_with_auto_and_keep_probe_order() {
        assert_eq!(device_options(&[]), vec![SdCppDeviceChoice::Auto]);
        let devices = [device("Vulkan0", "Intel UHD 770"), device("Vulkan1", "AMD Radeon RX 7900 XTX"), device("CPU", "")];
        assert_eq!(
            device_options(&devices),
            vec![SdCppDeviceChoice::Auto, named("Vulkan0", "Intel UHD 770"), named("Vulkan1", "AMD Radeon RX 7900 XTX"), named("CPU", "")]
        );
        assert_eq!(device_label(&named("Vulkan1", "AMD Radeon RX 7900 XTX")), "Vulkan1 — AMD Radeon RX 7900 XTX");
        assert_eq!(device_label(&named("CPU", " ")), "CPU");
    }

    /// A saved named device is missing iff no probed device carries its name (the
    /// description may differ); «Авто» is never missing.
    #[test]
    fn missing_device_rule_matches_by_name() {
        let devices = [device("Vulkan0", "Intel UHD 770"), device("Vulkan1", "AMD Radeon")];
        assert!(!saved_device_missing(&SdCppDeviceChoice::Auto, &devices));
        assert!(!saved_device_missing(&SdCppDeviceChoice::Auto, &[]));
        assert!(!saved_device_missing(&named("Vulkan1", "an older driver string"), &devices));
        assert!(saved_device_missing(&named("Vulkan2", "AMD Radeon"), &devices));
        assert!(saved_device_missing(&named("Vulkan0", "Intel UHD 770"), &[]));
    }

    /// The stage table: every stage maps to its own `ai_backend.sdcpp.stage_*` key. The test
    /// binary installs no catalog (other tests of this crate rely on that), so a label is its
    /// key verbatim; key existence in `en` / `ru` is guarded by `ms-i18n`'s key validation.
    #[test]
    fn stage_labels_map_each_stage_to_its_own_key() {
        let table = [
            (PackageStage::Probing, "ai_backend.sdcpp.stage_probing"),
            (PackageStage::WaitingForOtherDownload, "ai_backend.sdcpp.stage_waiting"),
            (PackageStage::Downloading, "ai_backend.sdcpp.stage_downloading"),
            (PackageStage::Retrying { attempt: 2, max_attempts: 5, delay_secs: 7 }, "ai_backend.sdcpp.stage_retrying"),
            (PackageStage::Verifying, "ai_backend.sdcpp.stage_verifying"),
            (PackageStage::Extracting, "ai_backend.sdcpp.stage_extracting"),
            (PackageStage::Done, "ai_backend.sdcpp.stage_done"),
        ];
        for (stage, key) in table {
            let label = sdcpp_stage_label(stage);
            // An installed catalog would translate it; without one the key comes back.
            assert!(label == key || !label.starts_with("ai_backend."), "{stage:?} -> {label}");
        }
    }

    /// Sizes are MiB with one decimal, rounded half up.
    #[test]
    fn size_label_is_mib_with_one_decimal() {
        assert_eq!(size_mib_label(0), "0.0");
        assert_eq!(size_mib_label(1024 * 1024), "1.0");
        assert_eq!(size_mib_label(1024 * 1024 * 3 / 2), "1.5");
        assert_eq!(size_mib_label(40_084_224), "38.2");
        assert!(!size_mib_label(u64::MAX).is_empty());
    }
}
