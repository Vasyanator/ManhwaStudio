/*
File: install.rs

Purpose:
Owns the installer UI and installer-specific window flows.

Main responsibilities:
- draw the egui installation, existing-install, and uninstall progress windows;
- collect install target, dependency profile, and PyTorch choices from the user;
- host the two installer purposes (`InstallerPurpose`): a full application install and the
  environment-repair mode that only provisions the Python environment of an existing root;
- surface actions for a detected existing install, including launching, shortcut creation,
  reinstall, updating that installed executable, replacing the installed executable with the
  running one (executable ONLY), and running this copy standalone;
- start background installer workers and consume their progress events;
- persist the selected AI dependency level into the installed `user_config.json`
  (one `ms_docstore::update` of the install target's document);
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
use web_time::Duration;
#[cfg(target_os = "windows")]
use web_time::Instant;
#[cfg(target_os = "windows")]
use web_time::SystemTime;

use ms_config as config;

// The host version: displayed by the Windows "existing install found" window, and its
// version core written as the Uninstall entry's `DisplayVersion` by a full install.
use crate::HostVersion;
use ms_sysprobe::gpu_utils::RuntimeVersion;
#[cfg(target_os = "windows")]
use ms_sysprobe::python_manager;
use eframe::egui;

use super::utils::*;
#[cfg(target_os = "windows")]
pub use super::utils::{
    run_windows_create_start_menu_shortcut_for_install, run_windows_uninstall_from_current_exe,
};
// Windows records (registry keys, shortcuts, path rules) come from their one owner; errors are
// shown through `IntegrationError::user_message()`, the installer's localized texts.
#[cfg(target_os = "windows")]
use ms_os_integration::windows::registry::reg_read_string;
#[cfg(target_os = "windows")]
use ms_os_integration::windows::shortcut::{
    create_windows_desktop_shortcut, create_windows_start_menu_shortcut, resolve_windows_launcher_target,
};
#[cfg(target_os = "windows")]
use ms_os_integration::windows::values::{app_paths_key, uninstall_key};
#[cfg(target_os = "windows")]
use ms_os_integration::windows::{is_windows_all_users_install_dir, normalize_windows_path};

pub(super) const INSTALL_SUBDIR_NAME: &str = "ManhwaStudio";
const TELEGRAM_INVITE_URL: &str = "https://t.me/SelfTranslators";
const DISCORD_INVITE_URL: &str = "https://discord.gg/mZjZszwDbH";
pub(super) const EMBEDDED_APP_ICON_ICO: &[u8] = include_bytes!("../../../app_icon.ico");
pub(super) const EMBEDDED_APP_ICON_PNG: &[u8] = include_bytes!("../../../app_icon_512.png");

pub enum InstallerOutcome {
    Completed,
    LaunchLauncher(PathBuf),
    ElevatedRelaunchStarted,
    Cancelled,
    Failed(String),
}

/// What an installer window is being opened for.
///
/// The two purposes share the same window, screens and progress plumbing but not
/// the same worker or set of side effects.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum InstallerPurpose {
    /// Full application install: pick a location (possibly elevated), deploy the app
    /// archive and executable, create shortcuts/registry entries, then offer to launch
    /// the installed copy. `host` is the RUNNING program's version — the copy being
    /// installed — whose core becomes the Uninstall entry's `DisplayVersion`.
    FullInstall { host: HostVersion },
    /// Repair the managed Python environment of an existing root directory. The
    /// location screen is skipped (the root is fixed), and the worker only provisions
    /// the environment — see `utils::run_environment_repair_worker` for the list of
    /// things this mode must never do.
    EnvironmentRepair,
}

#[cfg(target_os = "windows")]
pub enum ExistingInstallAction {
    NoInstallFound,
    ExitCurrentCopy,
    StartInstaller(PathBuf),
    UpdateInstalled(ExternalUpdateTarget),
    /// Run THIS copy without touching the installed one. The flag that expresses this
    /// (`--ignore-installed`) is consumed during startup routing long before this window
    /// opens and is seeded into a `OnceLock`, so it cannot be turned on in process:
    /// `main.rs` must relaunch this executable with the flag prepended and then exit.
    RunStandalone,
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
    WaitingForReplace,
    /// A shortcut is being written on a worker; the result arrives as
    /// [`ExistingInstallEvent::ShortcutFinished`].
    WaitingForShortcut,
    Error,
}

/// Which shortcut the existing-install window asked its worker to create.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug)]
enum ExistingInstallShortcut {
    /// `ManhwaStudio.lnk` on the current user's Desktop.
    Desktop,
    /// `ManhwaStudio.lnk` in the Start menu of the install's kind; may relaunch elevated for an
    /// all-users install (`run_windows_create_start_menu_shortcut_for_install`).
    StartMenu,
}

#[cfg(target_os = "windows")]
enum ExistingInstallEvent {
    ReinstallFinished(Result<PathBuf, String>),
    /// Result of the background `--version` probe of the installed executable.
    InstalledVersionProbed(Result<String, String>),
    /// Result of replacing the installed executable with the running one.
    ReplaceFinished(Result<(), String>),
    /// Result of a shortcut creation started from this window; the error is user-facing.
    ShortcutFinished(Result<(), String>),
}

/// State of the background `--version` probe of the installed copy.
///
/// The `test` arm of the `cfg` exists so [`installed_version_display`] is compiled and
/// exercised by `cargo test` on non-Windows development hosts; the window itself is
/// Windows-only.
#[cfg(any(target_os = "windows", test))]
enum InstalledVersionProbe {
    /// The worker thread has not answered yet.
    Pending,
    /// The installed copy printed this EXTENDED version string (`MS_APP_VERSION` form).
    /// It is only ever displayed, never compared.
    Known(String),
    /// The probe failed; the reason is logged, and the UI must say "unknown" rather than
    /// invent a version.
    Unknown,
}

/// Text shown for the installed copy's version in the replace hint line.
///
/// Returns the probed string verbatim when known, and a localized placeholder otherwise —
/// distinct ones for "still probing" and "could not be determined", so a slow probe is never
/// mistaken for a failed one.
#[cfg(any(target_os = "windows", test))]
fn installed_version_display(probe: &InstalledVersionProbe) -> String {
    match probe {
        InstalledVersionProbe::Pending => t!("installer.install.version_probing_placeholder").to_string(),
        InstalledVersionProbe::Known(version) => version.clone(),
        InstalledVersionProbe::Unknown => t!("installer.install.version_unknown_placeholder").to_string(),
    }
}

#[cfg(target_os = "windows")]
struct ExistingInstallApp {
    install: ExistingWindowsInstall,
    state: ExistingInstallUiState,
    status_text: String,
    error_text: Option<String>,
    /// Kept so every background job of this window (version probe, reinstall, replace)
    /// reports through the SAME channel instead of replacing the receiver.
    tx: mpsc::Sender<ExistingInstallEvent>,
    rx: mpsc::Receiver<ExistingInstallEvent>,
    installed_version: InstalledVersionProbe,
    result_sink: Arc<Mutex<Option<ExistingInstallAction>>>,
    /// The RUNNING copy's display version, shown next to the installed copy's probed one.
    /// Passed in because this crate cannot read the executable's own version.
    host_version_display: String,
}

/// Opens the full-install window for `root_dir` (or continues an elevated install into
/// `auto_install_target`). `host` is this executable's version, passed in because a library
/// cannot read it; its core is registered as the installed program's `DisplayVersion`.
/// Blocks until the window closes.
///
/// # Errors
/// Returns an error when the window itself cannot be created or its result cannot be read
/// back.
pub fn run_python_installer_window(
    root_dir: &Path,
    auto_install_target: Option<PathBuf>,
    host: HostVersion,
) -> Result<InstallerOutcome, String> {
    run_installer_window(
        root_dir,
        auto_install_target,
        InstallerPurpose::FullInstall { host },
        t!("installer.install.window_title"),
    )
}

/// Opens the installer in environment-repair mode for `root_dir`.
///
/// The window starts directly on the dependency-profile screen (no install-location
/// step) and provisions only the managed Python environment of `root_dir`; it never
/// deploys application files, shortcuts, registry entries, or launches another copy.
/// Blocks until the window closes.
///
/// Returns [`InstallerOutcome::Completed`] when the environment was provisioned,
/// [`InstallerOutcome::Cancelled`] when the user closed the window first, and
/// [`InstallerOutcome::Failed`] with a user-facing message when a stage failed.
///
/// # Errors
/// Returns an error when the window itself cannot be created or its result cannot
/// be read back.
pub fn run_environment_repair_window(root_dir: &Path) -> Result<InstallerOutcome, String> {
    run_installer_window(
        root_dir,
        None,
        InstallerPurpose::EnvironmentRepair,
        t!("installer.repair.window_title"),
    )
}

/// Shared window shell for both installer purposes.
///
/// `auto_install_target` is the elevation-continuation target of a full install; it
/// is unused (and must be `None`) in repair mode, where the target is `root_dir`.
fn run_installer_window(
    root_dir: &Path,
    auto_install_target: Option<PathBuf>,
    purpose: InstallerPurpose,
    window_title: &str,
) -> Result<InstallerOutcome, String> {
    let shared_result = Arc::new(Mutex::new(None::<InstallerOutcome>));
    let shared_result_for_app = Arc::clone(&shared_result);
    let root_dir = root_dir.to_path_buf();
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
        window_title,
        native_options,
        Box::new(move |cc| {
            ms_widgets::ui_fonts::install(&cc.egui_ctx, ms_widgets::ui_fonts::Tier::Core);
            Ok(Box::new(InstallerApp::new(
                root_dir.clone(),
                Arc::clone(&shared_result_for_app),
                auto_install_target.clone(),
                purpose,
            )))
        }),
    )
    .map_err(|e| e.to_string())?;

    let mut guard = shared_result
        .lock()
        .map_err(|_| t!("installer.install.no_install_result_error").to_string())?;
    Ok(guard.take().unwrap_or(InstallerOutcome::Cancelled))
}

#[cfg(target_os = "windows")]
pub fn handle_existing_windows_install(
    current_root: &Path,
    host: HostVersion,
) -> Result<ExistingInstallAction, String> {
    let Some(existing_install) = find_existing_windows_install(current_root)? else {
        return Ok(ExistingInstallAction::NoInstallFound);
    };
    run_existing_windows_install_window(existing_install, host)
}

#[cfg(target_os = "windows")]
fn run_existing_windows_install_window(
    existing_install: ExistingWindowsInstall,
    host: HostVersion,
) -> Result<ExistingInstallAction, String> {
    let result_sink = Arc::new(Mutex::new(None::<ExistingInstallAction>));
    let result_sink_for_app = Arc::clone(&result_sink);
    // Taller than the other startup windows on purpose: the window is not resizable and a
    // CentralPanel clips rather than scrolls, so the seven choices plus the wrapped
    // replace hint and Python-payload warning must fit without a scrollbar.
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([430.0, 580.0])
        .with_min_inner_size([400.0, 560.0])
        .with_max_inner_size([520.0, 660.0])
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
        Box::new(move |cc| {
            ms_widgets::ui_fonts::install(&cc.egui_ctx, ms_widgets::ui_fonts::Tier::Core);
            Ok(Box::new(ExistingInstallApp::new(
                existing_install.clone(),
                result_sink_for_app,
                host,
            )))
        }),
    )
    .map_err(|e| e.to_string())?;

    let mut guard = result_sink
        .lock()
        .map_err(|_| t!("installer.install.no_installed_copy_result_error").to_string())?;
    Ok(guard
        .take()
        .unwrap_or(ExistingInstallAction::ExitCurrentCopy))
}

/// Copies the RUNNING executable over the installed copy's executable.
///
/// Runs on a worker thread. Order matters: the install directory's writability is probed
/// FIRST, so an inaccessible target is refused before anything is created, and only then is
/// the staged copy made and renamed into place (`utils::replace_executable_with_local_file`).
///
/// Deliberately out of scope: `ManhwaStudio.zip`, the installed `installer_files/venv`, the
/// registry entries, and elevation. Only one file changes.
///
/// # Errors
/// Returns a localized message when the running executable cannot be located, when the
/// install directory refuses a write probe, or when the copy or the rename fails.
#[cfg(target_os = "windows")]
fn run_existing_install_replace_worker(install: &ExistingWindowsInstall) -> Result<(), String> {
    let source = env::current_exe().map_err(|e| tf!("installer.utils.determine_current_exe_error", e = e))?;
    match classify_replace_target_access(
        has_write_access_for_install(&install.install_dir),
        is_running_elevated(),
    ) {
        ReplaceTargetAccess::Writable => {}
        ReplaceTargetAccess::Blocked => {
            return Err(tf!("installer.install.replace_target_not_writable_error", install = install.install_dir.display()));
        }
        ReplaceTargetAccess::BlockedWhileElevated => {
            return Err(tf!("installer.install.replace_target_not_writable_elevated_error", install = install.install_dir.display()));
        }
    }
    replace_executable_with_local_file(&source, &install.launcher_path)
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
        tf!("installer.install.start_uninstall_error", install = install.launcher_path.display(), e = e)
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
            return Err(tf!("installer.install.unexpected_uninstall_signal_error", trimmed = trimmed));
        }

        if started_at.elapsed() > timeout {
            return Err(t!("installer.install.uninstall_timeout_error").to_string());
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

    if let Some(all_users) = query_registry_install_dir("HKLM") {
        candidates.push((all_users, t!("installer.install.registry_hklm").to_string()));
    }
    if let Some(current_user) = query_registry_install_dir("HKCU") {
        candidates.push((current_user, t!("installer.install.registry_hkcu").to_string()));
    }
    if let Some(app_path_dir) = query_registry_app_path_install_dir("HKLM") {
        candidates.push((app_path_dir, "App Paths HKLM".to_string()));
    }
    if let Some(app_path_dir) = query_registry_app_path_install_dir("HKCU") {
        candidates.push((app_path_dir, "App Paths HKCU".to_string()));
    }
    if let Ok(local_default) = default_local_install_dir() {
        candidates.push((local_default, t!("installer.install.user_standard_path").to_string()));
    }
    if let Ok(all_users_default) = default_all_users_install_dir() {
        candidates.push((all_users_default, t!("installer.install.all_users_standard_path").to_string()));
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

/// `InstallLocation` of the Uninstall entry under `registry_root`, when set and non-blank.
#[cfg(target_os = "windows")]
fn query_registry_install_dir(registry_root: &str) -> Option<PathBuf> {
    read_registry_dir_value(&uninstall_key(registry_root), "InstallLocation")
}

/// `Path` of the App Paths entry under `registry_root`, when set and non-blank.
#[cfg(target_os = "windows")]
fn query_registry_app_path_install_dir(registry_root: &str) -> Option<PathBuf> {
    read_registry_dir_value(&app_paths_key(registry_root), "Path")
}

/// Best-effort read of a directory-valued `REG_SZ` for install discovery: an absent, blank
/// or unreadable value (access denied, unexpected type) is `None` — discovery only collects
/// candidates, each of which is verified on disk afterwards — and a read failure is logged.
#[cfg(target_os = "windows")]
fn read_registry_dir_value(key: &str, value_name: &str) -> Option<PathBuf> {
    match reg_read_string(key, Some(value_name)) {
        Ok(value) => value.filter(|item| !item.trim().is_empty()).map(PathBuf::from),
        Err(status) => {
            ms_log::runtime_log::log_warn(format!(
                "[windows-install-discovery] could not read {key} value {value_name} (Windows error {status}); candidate skipped"
            ));
            None
        }
    }
}

pub fn spawn_installed_program_copy(install_dir: &Path) -> Result<PathBuf, String> {
    let target_exe = resolve_installed_program_copy_path(install_dir)?;
    let mut cmd = Command::new(&target_exe);
    cmd.current_dir(install_dir);
    apply_windows_no_window(&mut cmd);
    cmd.spawn().map_err(|e| {
        tf!("installer.install.launch_installed_copy_error", target_exe = target_exe.display(), install_dir = install_dir.display(), e = e)
    })?;
    Ok(target_exe)
}

pub fn resolve_installed_program_copy_path(install_dir: &Path) -> Result<PathBuf, String> {
    let current_exe_name = env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|name| name.to_os_string()));
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(name) = current_exe_name {
        candidates.push(install_dir.join(name));
    }
    #[cfg(target_os = "windows")]
    {
        candidates.push(install_dir.join(ms_os_integration::identity::WINDOWS_EXE_NAME));
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
    Err(tf!("installer.install.installed_copy_not_found_error", install_dir = install_dir.display(), listed = listed))
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TorchBackend {
    Cuda,
    Rocm,
}

#[derive(Clone, Debug)]
pub struct TorchWheelOption {
    pub(crate) backend: TorchBackend,
    pub(crate) wheel_tag: String,
    /// Human-readable name of the wheel, shown verbatim in the Torch choice UI (the
    /// launcher's settings page renders the same prompt as the installer window).
    pub label: String,
    pub(crate) version: RuntimeVersion,
}

#[derive(Clone, Debug)]
pub struct TorchChoicePrompt {
    pub options: Vec<TorchWheelOption>,
    pub recommended_index: usize,
    pub summary: String,
}

#[derive(Debug)]
pub enum TorchPreflightResult {
    Skip { reason: String },
    Choose(TorchChoicePrompt),
}

#[derive(Clone, Debug)]
pub enum TorchInstallSelection {
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
        host: HostVersion,
    ) -> Self {
        let status_text = tf!("installer.install.installed_copy_found_status", install = install.install_dir.display(), install_2 = install.source_label);
        let (tx, rx) = mpsc::channel();
        // The installed copy's version is only knowable by RUNNING it with `--version`, which
        // blocks; the GUI thread may only poll this channel (see `MODULE_README.md`).
        let probe_install = install.clone();
        let probe_tx = tx.clone();
        let _ = ms_thread::Builder::new()
            .name("existing-install-version-probe".to_string())
            .spawn(move || {
                let result =
                    query_executable_version(&probe_install.launcher_path, &probe_install.install_dir);
                let _ = probe_tx.send(ExistingInstallEvent::InstalledVersionProbed(result));
            });
        Self {
            install,
            state: ExistingInstallUiState::Choice,
            status_text,
            error_text: None,
            tx,
            rx,
            installed_version: InstalledVersionProbe::Pending,
            result_sink,
            host_version_display: host.display.to_string(),
        }
    }

    /// Starts the background replacement of the installed executable with the running one.
    ///
    /// Switches the window to [`ExistingInstallUiState::WaitingForReplace`]; the outcome
    /// arrives as [`ExistingInstallEvent::ReplaceFinished`]. Only the executable is copied —
    /// the installed Python payload and virtual environment are left untouched on purpose.
    fn start_replace_installed(&mut self) {
        let install = self.install.clone();
        let tx = self.tx.clone();
        self.state = ExistingInstallUiState::WaitingForReplace;
        self.error_text = None;
        self.status_text = tf!("installer.install.replacing_installed_copy_status", install = install.install_dir.display());
        let _ = ms_thread::Builder::new()
            .name("existing-install-replace".to_string())
            .spawn(move || {
                let result = run_existing_install_replace_worker(&install);
                let _ = tx.send(ExistingInstallEvent::ReplaceFinished(result));
            });
    }

    /// Records the "run this copy standalone" choice; `main.rs` performs the relaunch.
    fn choose_run_standalone(&mut self) {
        self.set_result(ExistingInstallAction::RunStandalone);
    }

    fn set_result(&self, result: ExistingInstallAction) {
        if let Ok(mut guard) = self.result_sink.lock() {
            *guard = Some(result);
        }
    }

    /// Starts creating `shortcut` for the installed copy on a named worker: the COM shortcut
    /// write (and, for an all-users Start menu, the UAC relaunch) blocks, and the GUI thread
    /// only polls. Switches the window to [`ExistingInstallUiState::WaitingForShortcut`]; the
    /// outcome arrives as [`ExistingInstallEvent::ShortcutFinished`].
    fn start_create_shortcut(&mut self, shortcut: ExistingInstallShortcut) {
        let install_dir = self.install.install_dir.clone();
        let tx = self.tx.clone();
        self.state = ExistingInstallUiState::WaitingForShortcut;
        self.error_text = None;
        let spawned = ms_thread::Builder::new()
            .name("existing-install-shortcut".to_string())
            .spawn(move || {
                let result = match shortcut {
                    ExistingInstallShortcut::Desktop => {
                        create_windows_desktop_shortcut(&install_dir).map(|_| ()).map_err(|e| e.user_message())
                    }
                    ExistingInstallShortcut::StartMenu => {
                        run_windows_create_start_menu_shortcut_for_install(&install_dir, false)
                    }
                };
                if tx.send(ExistingInstallEvent::ShortcutFinished(result)).is_err() {
                    // The window closed while the worker ran; nobody is left to show the result.
                    ms_log::runtime_log::log_warn(format!(
                        "[existing-install] shortcut result ({shortcut:?}) dropped: the window is closed"
                    ));
                }
            });
        if let Err(err) = spawned {
            // Without a worker nothing was written: report it like a failed creation.
            ms_log::runtime_log::log_error(format!("[existing-install] could not start the shortcut worker: {err}"));
            self.state = ExistingInstallUiState::Error;
            self.error_text = Some(tf!("installer.install.shortcut_worker_start_error", e = err));
        }
    }

    fn launch_installed_copy(&mut self) -> Result<(), String> {
        spawn_installed_program_copy(&self.install.install_dir)?;
        self.set_result(ExistingInstallAction::ExitCurrentCopy);
        Ok(())
    }

    fn start_reinstall(&mut self) {
        let install = self.install.clone();
        let tx = self.tx.clone();
        self.state = ExistingInstallUiState::WaitingForReinstall;
        self.error_text = None;
        self.status_text = tf!("installer.install.removing_installed_copy_status", install = install.install_dir.display());
        let _ = ms_thread::Builder::new()
            .name("existing-install-reinstall".to_string())
            .spawn(move || {
                let result = run_existing_install_reinstall_worker(&install);
                let _ = tx.send(ExistingInstallEvent::ReinstallFinished(result));
            });
    }
}

#[cfg(target_os = "windows")]
impl eframe::App for ExistingInstallApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35: `App::ui` receives the window-root `Ui`; keep a borrowed `Context` handle for
        // the viewport-command calls, and build the root `CentralPanel` on `ui` below.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        let mut queued_events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            queued_events.push(event);
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
                    self.status_text = t!("installer.install.reinstall_not_prepared_error").to_string();
                }
                ExistingInstallEvent::InstalledVersionProbed(Ok(version)) => {
                    self.installed_version = InstalledVersionProbe::Known(version);
                }
                ExistingInstallEvent::InstalledVersionProbed(Err(err)) => {
                    // A failed probe is not an error of the user's action: the version line
                    // says "unknown" and every button stays available (the user may be
                    // repairing exactly this broken install).
                    ms_log::runtime_log::log_warn(format!(
                        "could not query the version of the installed copy '{}': {err}",
                        self.install.launcher_path.display()
                    ));
                    self.installed_version = InstalledVersionProbe::Unknown;
                }
                ExistingInstallEvent::ReplaceFinished(Ok(())) => {
                    self.state = ExistingInstallUiState::Choice;
                    self.error_text = None;
                    self.status_text = tf!("installer.install.replace_succeeded_status", install = self.install.install_dir.display());
                }
                ExistingInstallEvent::ReplaceFinished(Err(err)) => {
                    self.state = ExistingInstallUiState::Error;
                    self.error_text = Some(err);
                    self.status_text = t!("installer.install.replace_failed_status").to_string();
                }
                // Same outcome the synchronous buttons had: success ends this copy's session,
                // a failure stays in the window with the error.
                ExistingInstallEvent::ShortcutFinished(Ok(())) => {
                    self.set_result(ExistingInstallAction::ExitCurrentCopy);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ExistingInstallEvent::ShortcutFinished(Err(err)) => {
                    self.state = ExistingInstallUiState::Error;
                    self.error_text = Some(err);
                }
            }
        }

        let mut close_window = false;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading(t!("installer.install.already_installed_prompt"));
            });
            ui.add_space(10.0);
            ui.small(tf!("installer.install.installed_copy_label", arg = self.install.install_dir.display()));
            ui.small(tf!("installer.install.source_label", arg = self.install.source_label));
            ui.add_space(8.0);
            ui.label(&self.status_text);
            if let Some(error_text) = &self.error_text {
                ui.add_space(8.0);
                ui.colored_label(ms_theme::status::ERROR, error_text);
            }
            ui.add_space(12.0);

            match self.state {
                ExistingInstallUiState::Choice | ExistingInstallUiState::Error => {
                    if ui
                        .add_sized([280.0, 34.0], egui::Button::new(t!("installer.install.launch_installed_button")))
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
                    // Next to "launch the installed copy" because it is the other way to just
                    // START something, and the only choice that changes nothing on disk.
                    if ui
                        .add_sized([280.0, 34.0], egui::Button::new(t!("installer.install.run_standalone_button")))
                        .clicked()
                    {
                        self.choose_run_standalone();
                        close_window = true;
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_sized(
                            [280.0, 34.0],
                            egui::Button::new(t!("installer.common.create_desktop_shortcut_button")),
                        )
                        .clicked()
                    {
                        self.start_create_shortcut(ExistingInstallShortcut::Desktop);
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_sized(
                            [280.0, 34.0],
                            egui::Button::new(t!("installer.install.create_start_menu_shortcut_button")),
                        )
                        .clicked()
                    {
                        self.start_create_shortcut(ExistingInstallShortcut::StartMenu);
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_sized([280.0, 34.0], egui::Button::new(t!("installer.install.update_installed_button")))
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
                    // Placed next to "update the installed copy": both rewrite the installed
                    // executable, and this one is the manual, offline variant of that. It stays
                    // above "reinstall", which is still the more drastic of the two.
                    if ui
                        .add_sized([280.0, 34.0], egui::Button::new(t!("installer.install.replace_installed_button")))
                        .clicked()
                    {
                        self.start_replace_installed();
                    }
                    ui.add_space(4.0);
                    ui.small(tf!(
                        "installer.install.replace_versions_label",
                        current = self.host_version_display.as_str(),
                        installed = installed_version_display(&self.installed_version)
                    ));
                    ui.small(t!("installer.install.replace_python_warning_label"));
                    ui.add_space(6.0);
                    if ui
                        .add_sized([280.0, 34.0], egui::Button::new(t!("installer.install.reinstall_button")))
                        .clicked()
                    {
                        self.start_reinstall();
                    }
                }
                ExistingInstallUiState::WaitingForReinstall => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t!("installer.install.waiting_uninstall_status"));
                    });
                }
                ExistingInstallUiState::WaitingForReplace => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t!("installer.install.waiting_replace_status"));
                    });
                }
                ExistingInstallUiState::WaitingForShortcut => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t!("installer.install.waiting_shortcut_status"));
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
    purpose: InstallerPurpose,
    root_dir: PathBuf,
    install_location_choice: InstallLocationChoice,
    custom_install_base_dir_input: String,
    install_target_dir: Option<PathBuf>,
    launcher_exe_path: Option<PathBuf>,
    #[cfg(target_os = "windows")]
    create_windows_desktop_shortcut: bool,
    #[cfg(target_os = "windows")]
    create_windows_start_menu_shortcut: bool,
    /// Report channel of the finish-screen shortcut worker while it runs
    /// ([`InstallerApp::finish_with_shortcuts`]); the window refuses to close while it is set.
    #[cfg(target_os = "windows")]
    finish_shortcuts_rx: Option<mpsc::Receiver<FinishShortcutsReport>>,
    /// The finish-screen outcome, applied once the shortcut worker reported.
    #[cfg(target_os = "windows")]
    pending_finish_outcome: Option<InstallerOutcome>,
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
        purpose: InstallerPurpose,
    ) -> Self {
        let custom_install_base_dir_input = root_dir.to_string_lossy().to_string();
        let mut app = Self {
            purpose,
            root_dir,
            install_location_choice: InstallLocationChoice::AllUsers,
            custom_install_base_dir_input,
            install_target_dir: None,
            launcher_exe_path: env::current_exe().ok(),
            #[cfg(target_os = "windows")]
            create_windows_desktop_shortcut: true,
            #[cfg(target_os = "windows")]
            create_windows_start_menu_shortcut: true,
            #[cfg(target_os = "windows")]
            finish_shortcuts_rx: None,
            #[cfg(target_os = "windows")]
            pending_finish_outcome: None,
            state: UiState::Idle,
            current_operation: t!("installer.install.waiting_start_status").to_string(),
            stage_progress: 0.0,
            stage_label: t!("installer.install.stage_not_started").to_string(),
            overall_progress: 0.0,
            overall_label: t!("installer.common.initialization").to_string(),
            console_lines: Vec::new(),
            rx: None,
            result_sink,
            torch_choice_prompt: None,
            pending_ai_install_type: config::AiInstallType::None,
            invite_telegram: true,
            invite_discord: false,
        };
        match purpose {
            InstallerPurpose::FullInstall { .. } => {
                if let Some(target_dir) = auto_install_target {
                    app.apply_auto_install_target(target_dir);
                }
            }
            InstallerPurpose::EnvironmentRepair => {
                // The target is fixed to the current root: repair never asks where to
                // install and never shows the location screen.
                let target_dir = app.root_dir.clone();
                app.show_dependency_profile_choice(target_dir);
            }
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
        self.console_lines.push(tf!("installer.install.uac_continuation_status", target_dir = target_dir.display()));
        self.show_dependency_profile_choice(target_dir);
    }

    fn resolved_install_target_dir(&self) -> Result<PathBuf, String> {
        match self.install_location_choice {
            InstallLocationChoice::Local => default_local_install_dir(),
            InstallLocationChoice::AllUsers => default_all_users_install_dir(),
            InstallLocationChoice::Custom => {
                let raw = self.custom_install_base_dir_input.trim();
                if raw.is_empty() {
                    return Err(t!("installer.install.install_dir_not_specified").to_string());
                }
                let base = PathBuf::from(raw);
                if base.is_file() {
                    return Err(tf!("installer.install.path_is_file_error", base = base.display()));
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

    /// Ends the full-install finish screen with `outcome` after writing the shortcuts the user
    /// ticked. Returns `Some(outcome)` when the window may close now (nothing to write, or not
    /// Windows). Otherwise the `.lnk` writes (COM file I/O that can stall on a redirected
    /// Desktop) run on the `install-finish-shortcuts` worker, the screen switches to
    /// `UiState::CreatingShortcuts` and `None` is returned: the window closes with `outcome` once
    /// the worker reports ([`Self::poll_finish_shortcuts`]). A worker that cannot start is logged,
    /// shown in the console, and the window closes without shortcuts.
    fn finish_with_shortcuts(&mut self, outcome: InstallerOutcome) -> Option<InstallerOutcome> {
        #[cfg(target_os = "windows")]
        {
            let Some(target_dir) = self.install_target_dir.clone() else {
                return Some(outcome);
            };
            let desktop = self.create_windows_desktop_shortcut;
            // An all-users install's Start-menu shortcut was already written by the install worker.
            let start_menu = self.create_windows_start_menu_shortcut && !is_windows_all_users_install_dir(&target_dir);
            if !desktop && !start_menu {
                self.apply_finish_shortcuts_report(&target_dir, FinishShortcutsReport::default());
                return Some(outcome);
            }
            let (tx, rx) = mpsc::channel();
            let worker_dir = target_dir.clone();
            let spawned = ms_thread::Builder::new()
                .name("install-finish-shortcuts".to_string())
                .spawn(move || {
                    let report = write_finish_screen_shortcuts(&worker_dir, desktop, start_menu);
                    if tx.send(report).is_err() {
                        // The receiver lives until the report arrives; only a torn-down app drops it.
                        ms_log::runtime_log::log_warn("[install] finish-screen shortcut report dropped: the installer window is gone");
                    }
                });
            match spawned {
                Ok(_detached) => {
                    self.finish_shortcuts_rx = Some(rx);
                    self.pending_finish_outcome = Some(outcome);
                    self.state = UiState::CreatingShortcuts;
                    self.current_operation = t!("installer.install.waiting_shortcuts_status").to_string();
                    None
                }
                Err(err) => {
                    ms_log::runtime_log::log_error(format!("[install] could not start the finish-screen shortcut worker: {err}"));
                    self.console_lines.push(tf!("installer.install.shortcut_worker_start_error", e = err));
                    self.apply_finish_shortcuts_report(&target_dir, FinishShortcutsReport::default());
                    Some(outcome)
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        Some(outcome)
    }

    /// Drains the finish-screen shortcut worker. Returns the outcome chosen on the finish
    /// screen once the worker reported (or ended without a report, which is logged and shown
    /// as "not created"); `None` while it still runs or when none was started.
    #[cfg(target_os = "windows")]
    fn poll_finish_shortcuts(&mut self) -> Option<InstallerOutcome> {
        let rx = self.finish_shortcuts_rx.as_ref()?;
        let report = match rx.try_recv() {
            Ok(report) => report,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                ms_log::runtime_log::log_error("[install] finish-screen shortcut worker ended without a report");
                FinishShortcutsReport::default()
            }
        };
        self.finish_shortcuts_rx = None;
        if let Some(target_dir) = self.install_target_dir.clone() {
            self.apply_finish_shortcuts_report(&target_dir, report);
        }
        self.pending_finish_outcome.take()
    }

    /// Turns the finish-screen shortcut `report` into console lines and the current-operation
    /// status, judged against the user's ticks for the install at `target_dir`.
    #[cfg(target_os = "windows")]
    fn apply_finish_shortcuts_report(&mut self, target_dir: &Path, report: FinishShortcutsReport) {
        let FinishShortcutsReport { created_paths, console_lines } = report;
        self.console_lines.extend(console_lines);
        if !created_paths.is_empty() {
            self.current_operation = tf!("installer.install.shortcuts_created_status", created_paths = created_paths.join(" | "));
        } else if self.create_windows_desktop_shortcut || self.create_windows_start_menu_shortcut {
            self.current_operation = t!("installer.install.shortcuts_not_created").to_string();
        } else {
            self.current_operation = t!("installer.install.shortcuts_skipped").to_string();
        }
        if self.create_windows_start_menu_shortcut && is_windows_all_users_install_dir(target_dir) {
            self.console_lines.push(t!("installer.install.start_menu_already_created_log").to_string());
            if created_paths.is_empty() && !self.create_windows_desktop_shortcut {
                self.current_operation = t!("installer.install.start_menu_already_created").to_string();
            }
        }
    }

    fn start_torch_preflight(&mut self, install_target_dir: PathBuf) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.state = UiState::PreparingTorchChoice;
        self.install_target_dir = Some(install_target_dir.clone());
        self.current_operation = t!("installer.install.checking_gpu_status").to_string();
        self.stage_progress = 0.0;
        self.stage_label = t!("installer.install.stage_prepare_pytorch").to_string();
        self.overall_progress = 0.0;
        self.overall_label = tf!("installer.install.target_folder_label", install_target_dir = install_target_dir.display());
        self.torch_choice_prompt = None;

        let _ = ms_thread::Builder::new()
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
        self.current_operation = t!("installer.install.stage_choose_install_mode").to_string();
        self.stage_progress = 0.0;
        self.stage_label = t!("installer.install.choose_deps_set_hint").to_string();
        self.overall_progress = 0.0;
        self.overall_label = tf!("installer.install.target_folder_label", install_target_dir = install_target_dir.display());
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
        self.current_operation = t!("installer.common.initialization").to_string();
        self.stage_progress = 0.0;
        self.stage_label = t!("installer.common.preparation").to_string();
        self.overall_progress = 0.0;
        self.overall_label = match self.purpose {
            InstallerPurpose::FullInstall { .. } => tf!(
                "installer.install.installing_to_status",
                install_target_dir = install_target_dir.display()
            ),
            InstallerPurpose::EnvironmentRepair => tf!(
                "installer.repair.setting_up_env_status",
                install_target_dir = install_target_dir.display()
            ),
        };
        self.console_lines.clear();
        self.torch_choice_prompt = None;
        self.pending_ai_install_type = match dependency_profile {
            InstallDependencyProfile::Fast => config::AiInstallType::Base,
            InstallDependencyProfile::Full => config::AiInstallType::Full,
        };

        let purpose = self.purpose;
        let _ = ms_thread::Builder::new()
            .name("mini-launcher-python-installer".to_string())
            .spawn(move || {
                let result = match purpose {
                    InstallerPurpose::FullInstall { host } => run_install_worker(
                        root_dir,
                        launcher_exe_path,
                        config::version_format::version_core(host.core),
                        dependency_profile,
                        torch_selection,
                        &tx,
                    ),
                    InstallerPurpose::EnvironmentRepair => run_environment_repair_worker(
                        root_dir,
                        dependency_profile,
                        torch_selection,
                        &tx,
                    ),
                };
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35: `App::ui` receives the window-root `Ui`; keep a borrowed `Context` handle for
        // viewport commands / repaint scheduling, and build the root `CentralPanel` on `ui` below.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
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
                            self.current_operation = t!("installer.install.install_error").to_string();
                            self.stage_label = t!("installer.install.stage_failed").to_string();
                            self.overall_label = t!("installer.install.no_target_folder_error").to_string();
                            self.set_result(InstallerOutcome::Failed(
                                t!("installer.install.no_target_folder_error").to_string(),
                            ));
                        }
                    }
                    TorchPreflightResult::Choose(prompt) => {
                        self.state = UiState::TorchChoice;
                        self.current_operation = t!("installer.install.stage_choose_pytorch").to_string();
                        self.stage_progress = 0.0;
                        self.stage_label = t!("installer.install.choose_pytorch_wheel_hint").to_string();
                        self.overall_progress = 0.0;
                        self.overall_label = prompt.summary.clone();
                        self.torch_choice_prompt = Some(prompt);
                    }
                },
                InstallEvent::Finished(Ok(())) => {
                    let mut persist_error = None;
                    if let Some(install_target_dir) = self.install_target_dir.as_deref() {
                        match persist_ai_install_type_for_install_target(
                            install_target_dir,
                            self.pending_ai_install_type,
                        ) {
                            Ok(()) => self.console_lines.push(tf!("installer.install.ai_type_saved_log", arg = self.pending_ai_install_type.as_str())),
                            Err(err) => {
                                self.console_lines.push(tf!("installer.install.ai_type_save_error_log", err = err));
                                persist_error = Some(err);
                            }
                        }
                    }
                    // Environment repair is contracted to leave a READY environment behind,
                    // and readiness includes the recorded install type: without it the next
                    // `--check-venv` would ask for the dependency profile again on every
                    // launch. A failed write is therefore a failed repair, not a footnote.
                    // A full install keeps the historical behavior (the app is deployed and
                    // usable; the install type is re-detected at startup).
                    if let (InstallerPurpose::EnvironmentRepair, Some(err)) =
                        (self.purpose, persist_error)
                    {
                        let message = tf!("installer.repair.ai_type_save_failed", err = err);
                        self.state = UiState::Failed;
                        self.current_operation = t!("installer.install.install_error").to_string();
                        self.stage_label = t!("installer.install.stage_failed").to_string();
                        self.overall_label = message.clone();
                        self.set_result(InstallerOutcome::Failed(message));
                        continue;
                    }
                    self.state = UiState::Completed;
                    self.current_operation = t!("installer.common.install_complete").to_string();
                    self.stage_progress = 1.0;
                    self.stage_label = t!("installer.install.finished").to_string();
                    self.overall_progress = 1.0;
                    self.overall_label = t!("installer.common.ready").to_string();
                    self.set_result(InstallerOutcome::Completed);
                }
                InstallEvent::Finished(Err(err)) => {
                    self.state = UiState::Failed;
                    self.current_operation = t!("installer.install.install_error").to_string();
                    self.stage_label = t!("installer.install.stage_failed").to_string();
                    self.overall_label = err.clone();
                    self.set_result(InstallerOutcome::Failed(err));
                }
            }
        }

        let mut selected_torch_install: Option<TorchInstallSelection> = None;
        let mut selected_dependency_profile: Option<InstallDependencyProfile> = None;
        let mut requested_start_install = false;
        let mut finish_outcome: Option<InstallerOutcome> = None;
        egui::CentralPanel::default().show(ui, |ui| {
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
                        ui.label(t!("installer.install.choose_install_type_label"));
                        ui.radio_value(
                            &mut self.install_location_choice,
                            InstallLocationChoice::Local,
                            t!("installer.install.install_locally_option"),
                        );
                        ui.radio_value(
                            &mut self.install_location_choice,
                            InstallLocationChoice::AllUsers,
                            t!("installer.install.install_all_users_option"),
                        );
                        ui.radio_value(
                            &mut self.install_location_choice,
                            InstallLocationChoice::Custom,
                            t!("installer.install.install_other_location_option"),
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
                                if ui.button(t!("installer.install.choose_button")).clicked() {
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
                                ui.small(tf!("installer.install.will_install_to_status", target = target.display()));
                                if self.install_location_choice == InstallLocationChoice::AllUsers {
                                    ui.small(t!("installer.install.elevation_needed_hint"));
                                }
                            }
                            Err(err) => {
                                ui.colored_label(
                                    ms_theme::status::ERROR,
                                    tf!("installer.install.path_error", err = err),
                                );
                            }
                        }

                        ui.add_space((center_height * 0.15).max(12.0));
                        ui.horizontal_centered(|ui| {
                            if ui
                                .add_sized([180.0, 40.0], egui::Button::new(t!("installer.install.install_button")))
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
                            ui.label(t!("installer.install.checking_gpu_cuda_rocm_status"));
                        });
                    }
                    UiState::DependencyProfileChoice => {
                        ui.add_space((center_height * 0.16).max(8.0));
                        ui.label(match self.purpose {
                            InstallerPurpose::FullInstall { .. } => {
                                t!("installer.install.choose_deps_set_label")
                            }
                            InstallerPurpose::EnvironmentRepair => {
                                t!("installer.repair.choose_deps_set_label")
                            }
                        });
                        ui.add_space(8.0);
                        if ui
                            .add_sized([300.0, 38.0], egui::Button::new(t!("installer.install.fast_install_option")))
                            .clicked()
                        {
                            selected_dependency_profile = Some(InstallDependencyProfile::Fast);
                        }
                        ui.small(t!("installer.install.fast_install_desc"));
                        ui.add_space(12.0);
                        if ui
                            .add_sized([300.0, 38.0], egui::Button::new(t!("installer.install.full_install_option")))
                            .clicked()
                        {
                            selected_dependency_profile = Some(InstallDependencyProfile::Full);
                        }
                        ui.small(t!("installer.install.full_install_desc"));
                    }
                    UiState::TorchChoice => {
                        if let Some(prompt) = &self.torch_choice_prompt {
                            ui.label(t!("installer.install.available_pytorch_wheels_label"));
                            ui.small(&prompt.summary);
                            if prompt.options.is_empty() {
                                ui.add_space(8.0);
                                ui.small(t!("installer.install.no_gpu_variants_hint"));
                            } else {
                                ui.add_space(8.0);
                                ui.small(tf!("installer.install.recommended_label", prompt = prompt.options[prompt.recommended_index].label));
                                ui.add_space(8.0);

                                for (idx, option) in prompt.options.iter().enumerate() {
                                    let title = if idx == prompt.recommended_index {
                                        tf!("installer.install.option_recommended_marker", option = option.label)
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
                            .add_sized([260.0, 34.0], egui::Button::new(t!("installer.install.keep_cpu_button")))
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
                    // Environment repair finishes with nothing to deploy or launch:
                    // no shortcuts, no invites, no "open the installed copy".
                    UiState::Completed if self.purpose == InstallerPurpose::EnvironmentRepair => {
                        ui.add_space((center_height * 0.25).max(12.0));
                        ui.horizontal_centered(|ui| {
                            ui.heading(t!("installer.repair.env_ready_heading"));
                        });
                        ui.add_space(10.0);
                        ui.horizontal_centered(|ui| {
                            if ui.button(t!("installer.common.close_button")).clicked() {
                                finish_outcome = Some(InstallerOutcome::Completed);
                            }
                        });
                    }
                    UiState::Completed => {
                        ui.add_space((center_height * 0.25).max(12.0));
                        ui.horizontal_centered(|ui| {
                            ui.heading(t!("installer.common.install_complete"));
                        });
                        ui.add_space(10.0);
                        ui.horizontal_centered(|ui| {
                            if ui
                                .add_sized([210.0, 36.0], egui::Button::new(t!("installer.install.open_button")))
                                .clicked()
                            {
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
                                finish_outcome = self.finish_with_shortcuts(InstallerOutcome::LaunchLauncher(install_dir));
                            }
                            ui.add_space(10.0);
                            ui.vertical(|ui| {
                                ui.checkbox(
                                    &mut self.invite_telegram,
                                    t!("installer.install.telegram_button"),
                                );
                                ui.checkbox(&mut self.invite_discord, t!("installer.install.discord_button"));
                                #[cfg(target_os = "windows")]
                                ui.checkbox(
                                    &mut self.create_windows_desktop_shortcut,
                                    t!("installer.common.create_desktop_shortcut_button"),
                                );
                                #[cfg(target_os = "windows")]
                                ui.checkbox(
                                    &mut self.create_windows_start_menu_shortcut,
                                    t!("installer.install.create_start_menu_shortcut_button_caps"),
                                );
                            });
                        });
                        ui.add_space(8.0);
                        ui.horizontal_centered(|ui| {
                            if ui.button(t!("installer.common.close_button")).clicked() {
                                finish_outcome = self.finish_with_shortcuts(InstallerOutcome::Completed);
                            }
                        });
                    }
                    #[cfg(target_os = "windows")]
                    UiState::CreatingShortcuts => {
                        ui.add_space((center_height * 0.25).max(12.0));
                        ui.horizontal_centered(|ui| {
                            ui.spinner();
                            ui.label(t!("installer.install.waiting_shortcuts_status"));
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
                            if ui.button(t!("installer.common.close_button")).clicked() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        });
                    }
                },
            );
            ui.add_space(6.0);
            ui.label(tf!("installer.install.current_operation_status", arg = self.current_operation));
            ui.add_space(4.0);
            ui.label(tf!("installer.install.stage_status", arg = self.stage_label));
            ui.add(egui::ProgressBar::new(self.stage_progress).show_percentage());
            ui.small(tf!(
                "installer.install.stage_progress_percent",
                pct = format!("{:.0}", self.stage_progress * 100.0)
            ));
            ui.add_space(4.0);
            ui.label(t!("installer.install.overall_progress_label"));
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
                self.current_operation = t!("installer.install.install_error").to_string();
                self.stage_label = t!("installer.install.stage_failed").to_string();
                self.overall_label = t!("installer.install.no_target_folder_error").to_string();
                self.set_result(InstallerOutcome::Failed(
                    t!("installer.install.no_target_folder_error").to_string(),
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
                self.current_operation = t!("installer.install.install_error").to_string();
                self.stage_label = t!("installer.install.stage_failed").to_string();
                self.overall_label = t!("installer.install.no_target_folder_error").to_string();
                self.set_result(InstallerOutcome::Failed(
                    t!("installer.install.no_target_folder_error").to_string(),
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
                                    t!("installer.install.request_elevation_failed").to_string();
                                self.overall_label = err;
                            }
                        }
                    } else {
                        self.show_dependency_profile_choice(target_dir);
                    }
                }
                Err(err) => {
                    self.current_operation = t!("installer.install.install_path_error").to_string();
                    self.overall_label = err;
                }
            }
        }
        #[cfg(target_os = "windows")]
        if finish_outcome.is_none() {
            finish_outcome = self.poll_finish_shortcuts();
        }
        #[cfg(target_os = "windows")]
        if self.finish_shortcuts_rx.is_some() && ctx.input(|input| input.viewport().close_requested()) {
            // Closing now would end the process under the worker mid-write; the outcome the user
            // already chose is applied (and the window closed) once the worker reports.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if let Some(outcome) = finish_outcome {
            self.set_result(outcome);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Also polls the finish-screen shortcut worker without user input.
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

/// What the finish-screen shortcut worker did: user-facing lines, applied by the GUI thread.
#[cfg(target_os = "windows")]
#[derive(Debug, Default)]
struct FinishShortcutsReport {
    /// `Desktop: <path>` / `Start Menu: <path>` for every shortcut written.
    created_paths: Vec<String>,
    /// `[Shortcut/…] <user message>` console lines of failed writes.
    console_lines: Vec<String>,
}

/// Writes the finish screen's ticked shortcuts for the install at `target_dir` (Desktop when
/// `desktop`, per-user Start menu when `start_menu`). Blocking COM file I/O: worker thread only.
/// Every failure becomes a console line; none aborts the other shortcut.
#[cfg(target_os = "windows")]
fn write_finish_screen_shortcuts(target_dir: &Path, desktop: bool, start_menu: bool) -> FinishShortcutsReport {
    let mut report = FinishShortcutsReport::default();
    if desktop {
        match create_windows_desktop_shortcut(target_dir).map_err(|e| e.user_message()) {
            Ok(path) => report.created_paths.push(format!("Desktop: {}", path.display())),
            Err(err) => report.console_lines.push(format!("[Shortcut/Desktop] {err}")),
        }
    }
    if start_menu {
        match create_windows_start_menu_shortcut(target_dir).map_err(|e| e.user_message()) {
            Ok(path) => report.created_paths.push(format!("Start Menu: {}", path.display())),
            Err(err) => report.console_lines.push(format!("[Shortcut/StartMenu] {err}")),
        }
    }
    report
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum UiState {
    Idle,
    DependencyProfileChoice,
    PreparingTorchChoice,
    TorchChoice,
    Running,
    Completed,
    /// Finish screen left: the ticked shortcuts are being written on a worker (Windows).
    #[cfg(target_os = "windows")]
    CreatingShortcuts,
    Failed,
}

/// Records `General.ai_install_type` in the install target's `user_config.json`.
///
/// One serialized read-modify-write through `ms_docstore::update` (atomic replace, the
/// historical pretty layout without a trailing newline). Like the former `JsonConfig`
/// path it also backfills missing `user_config_defaults()` keys and treats a non-object
/// root as `{}`. A malformed existing document is left untouched and reported.
///
/// # Errors
/// A localized message: `open_config_error` when the existing document cannot be read or
/// parsed, `save_ai_type_error` when the replacement cannot be written.
fn persist_ai_install_type_for_install_target(
    install_target_dir: &Path,
    install_type: config::AiInstallType,
) -> Result<(), String> {
    let path = install_target_dir.join(config::USER_CONFIG_FILE);
    let doc = ms_docstore::DocRef::new(&path, ms_docstore::DocKind::UserConfig);
    let defaults = config::user_config_defaults();
    let outcome = ms_docstore::update(&doc, ms_docstore::WriteOptions::default(), |root| {
        if !root.is_object() {
            *root = serde_json::Value::Object(serde_json::Map::new());
        }
        config::merge_missing(root, &defaults);
        let general = root
            .as_object_mut()
            .ok_or_else(|| "user config root is not an object".to_string())?
            .entry("General")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if !general.is_object() {
            *general = serde_json::Value::Object(serde_json::Map::new());
        }
        general
            .as_object_mut()
            .ok_or_else(|| "user config `General` is not an object".to_string())?
            .insert(
                config::GENERAL_AI_INSTALL_TYPE_KEY.to_string(),
                serde_json::Value::String(install_type.as_str().to_string()),
            );
        Ok(())
    });
    outcome.map_err(|err| {
        ms_log::runtime_log::log_error(format!(
            "[install] failed to record ai_install_type={} in '{}': {err}",
            install_type.as_str(),
            path.display()
        ));
        match err {
            ms_docstore::DocStoreError::Malformed { .. } | ms_docstore::DocStoreError::Storage(_) => tf!(
                "installer.install.open_config_error",
                path = install_target_dir.display(),
                err = err.to_string()
            ),
            _ => tf!("installer.install.save_ai_type_error", path = path.display(), err = err.to_string()),
        }
    })
}

#[derive(Debug)]
pub enum InstallEvent {
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
            status: t!("installer.install.prepare_uninstall_stage").to_string(),
            detail: t!("installer.install.collecting_cleanup_plan_status").to_string(),
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35: `App::ui` receives the window-root `Ui`; keep a borrowed `Context` handle for
        // viewport commands / repaint scheduling, and build the root `CentralPanel` on `ui` below.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
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
                        self.status = t!("installer.install.uninstall_complete_stage").to_string();
                        self.detail =
                            t!("installer.install.uninstall_final_cleanup_hint")
                                .to_string();
                        self.set_result(Ok(()));
                        self.close_after = Some(Instant::now() + Duration::from_millis(700));
                    }
                    Err(err) => {
                        self.error = Some(err.clone());
                        self.status = t!("installer.install.uninstall_error").to_string();
                        self.detail = err.clone();
                        self.set_result(Err(err));
                    }
                },
            }
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading(t!("installer.install.uninstall_window_title"));
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
                ui.colored_label(ms_theme::status::ERROR, err);
                ui.add_space(8.0);
                if ui.button(t!("installer.common.close_button")).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        });

        if let Some(deadline) = self.close_after
            && Instant::now() >= deadline
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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

    ms_thread::Builder::new()
        .name("mini-launcher-uninstall".to_string())
        .spawn(move || {
            let result = run_windows_uninstall_worker(current_exe, install_dir, &tx);
            let _ = tx.send(UninstallEvent::Finished(result));
        })
        .map_err(|e| tf!("installer.install.start_uninstall_worker_error", e = e))?;

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
        t!("installer.install.uninstall_window_title"),
        native_options,
        Box::new(move |cc| {
            ms_widgets::ui_fonts::install(&cc.egui_ctx, ms_widgets::ui_fonts::Tier::Core);
            Ok(Box::new(UninstallApp::new(rx, result_sink_for_app)))
        }),
    )
    .map_err(|e| e.to_string())?;

    let mut guard = result_sink
        .lock()
        .map_err(|_| t!("installer.install.no_uninstall_result_error").to_string())?;
    guard
        .take()
        .unwrap_or_else(|| Err(t!("installer.install.uninstall_window_closed_early_error").to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_version_display_never_invents_a_version() {
        assert_eq!(
            installed_version_display(&InstalledVersionProbe::Known("3.6.0+1cd9638".to_string())),
            "3.6.0+1cd9638",
            "a known version must be shown verbatim, extended suffix included"
        );
        let probing = installed_version_display(&InstalledVersionProbe::Pending);
        let unknown = installed_version_display(&InstalledVersionProbe::Unknown);
        assert!(!probing.is_empty() && !unknown.is_empty(), "both placeholders must say something");
        assert_ne!(
            probing, unknown,
            "a slow probe and a failed probe must not read the same"
        );
        for placeholder in [&probing, &unknown] {
            assert!(
                !placeholder.chars().any(|ch| ch.is_ascii_digit()),
                "a placeholder must not look like a version number: {placeholder}"
            );
        }
    }

    #[test]
    fn persist_ai_install_type_writes_installed_user_config() {
        let test_dir = std::env::temp_dir().join(format!(
            "manhwastudio_install_type_test_{}_{}",
            std::process::id(),
            web_time::SystemTime::now()
                .duration_since(web_time::UNIX_EPOCH)
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

    /// A malformed install-target user config is reported and never overwritten; a valid
    /// one keeps its keys, gains the missing defaults, and records the install type.
    #[test]
    fn persist_ai_install_type_keeps_malformed_config_and_existing_keys() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(config::USER_CONFIG_FILE);

        std::fs::write(&path, "{ not json").expect("fixture must be writable");
        assert!(persist_ai_install_type_for_install_target(dir.path(), config::AiInstallType::Base).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("fixture readable"), "{ not json");

        std::fs::write(&path, r#"{"General":{"ui_language":"xx-custom"},"Custom":{"k":1}}"#).expect("fixture must be writable");
        persist_ai_install_type_for_install_target(dir.path(), config::AiInstallType::Full).expect("valid config must be updated");
        let raw = std::fs::read_to_string(&path).expect("updated config readable");
        assert!(!raw.ends_with('\n'), "historical layout has no trailing newline");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(config::AiInstallType::from_user_settings(&value), config::AiInstallType::Full);
        assert_eq!(value["General"]["ui_language"], "xx-custom", "existing values are kept");
        assert_eq!(value["Custom"]["k"], 1, "unknown sections are kept");
        let defaults = config::user_config_defaults();
        for key in defaults.as_object().expect("defaults are an object").keys() {
            assert!(value.get(key).is_some(), "default section `{key}` must be backfilled");
        }
    }
}
