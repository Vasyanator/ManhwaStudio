/*
FILE OVERVIEW: src/startup/routing.rs
Where the main window starts, and the update routing around it.

- `resolve_start`: a valid `--project` chapter, or an image set (`--image <PATH>`, repeatable, or
  positional image paths: OS "Open with", `.desktop` `%F`; or the images another app delivered
  when there is no CLI target), opens the studio directly; an invalid project, or an image set
  none of whose paths is a file, is reported in the console and the startup error box
  (`prompts::show_startup_error_dialog`). Without an open target the pure, unit-tested
  `startup_routing` decides: on a platform without installed-copy management straight to the
  launcher; otherwise installer continuation, or — with no usable Python environment — the
  Windows existing-install check and the "install / update a custom folder / continue" prompt,
  each as its own window, ONCE per process.
- `--ignore-installed` (run from a source checkout): no existing-install discovery or prompt, no
  update check, self-update refused (`refuse_self_update_from_sources`).
- `run_launcher_requested_update`: the update window the launcher asked for, after the main
  window closed.

Key items: `StartPlan`, `StartupRoutingFlags`, `StartupRoute`, `startup_routing`,
`update_check_enabled`, `installed_copy_management_available`.
*/

use super::HOST_VERSION;
use super::prompts::{MissingPythonEnvAction, prompt_missing_python_env_action, run_startup_installer, show_startup_error_dialog};
use crate::args::Cli;
#[cfg(target_os = "windows")]
use crate::installer::install as launcher_install;
use crate::{args, config, installer, python_manager, runtime_log, shell};
use ms_platform::PlatformFeature;
use std::path::{Path, PathBuf};

/// Runs the update window the launcher's "Update" button asked for, after the main window has
/// closed. The button only exists with an update notification, which the startup check never
/// produces under `--ignore-installed` or without installed-copy management, so both checks below
/// are guards, not the UI path.
///
/// # Errors
/// The update window could not run.
pub(super) fn run_launcher_requested_update(cli: &Cli) -> anyhow::Result<()> {
    if cli.ignore_installed {
        refuse_self_update_from_sources();
        return Ok(());
    }
    if let Err(unavailable) = PlatformFeature::InstalledCopyManagement.require() {
        runtime_log::log_error(format!("[startup] self-update requested on a platform without installed-copy management; refused: {unavailable:?}"));
        return Ok(());
    }
    let outcome = installer::update::run_update_window(cli.test_ver_check, HOST_VERSION).map_err(anyhow::Error::msg)?;
    runtime_log::log_info(format!("[startup] update window closed: {outcome:?}"));
    Ok(())
}

/// Where the main window starts and whether it runs the startup update check.
#[derive(Debug)]
pub(super) struct StartPlan {
    pub(super) initial: shell::InitialScreen,
    /// Run the startup update check (once, on the first launcher entry).
    pub(super) update_check: bool,
}

/// Resolves where the main window starts: the CLI open target (`--project`, or an image set from
/// `--image` / the positional paths) when present, else the images another app delivered at
/// startup (`incoming`, Android), else whatever the no-argument startup path (installer
/// prompts) yields — normally the launcher. `None` means "exit the program". The prompts run
/// once per process: a later return to the launcher does not repeat them. A CLI target wins over
/// `incoming` (the two never meet on one platform); the dropped images are released.
///
/// # Errors
/// An invalid CLI open target (after its error dialog) and every error of the no-argument path.
pub(super) fn resolve_start(cli: &Cli, incoming: Option<shell::target::StartupTarget>) -> anyhow::Result<Option<StartPlan>> {
    // A CLI start can still reach the launcher later ("Exit to launcher"); its update notice
    // follows the same rule as a launcher start. A start from delivered images follows it too.
    let cli_update_check = || update_check_enabled(cli.ignore_installed, installed_copy_management_available());
    let has_cli_target = cli.project.is_some() || !cli.image_args().is_empty();
    let incoming = match incoming {
        Some(target) if has_cli_target => {
            shell::incoming::discard_target(target, "a command-line open target was given");
            None
        }
        other => other,
    };
    if let Some(project_dir) = &cli.project {
        return Ok(resolve_cli_project_dir(project_dir)?.map(|target| StartPlan { initial: shell::InitialScreen::Studio(target), update_check: cli_update_check() }));
    }
    let images = cli.image_args();
    if !images.is_empty() {
        let target = resolve_cli_images(images)?;
        return Ok(Some(StartPlan { initial: shell::InitialScreen::Studio(target), update_check: cli_update_check() }));
    }
    if let Some(target) = incoming {
        return Ok(Some(StartPlan { initial: shell::InitialScreen::Studio(target), update_check: cli_update_check() }));
    }
    resolve_start_without_cli_arg(
        StartupRoutingFlags { force_run_installer: cli.continue_install || cli.continue_install_target.is_some(), ignore_installed: cli.ignore_installed },
        cli.continue_install_target.clone(),
    )
}

/// CLI-derived switches that steer the no-`--project` startup path.
///
/// Passed explicitly instead of threading the whole `Cli` (or a global) through the
/// startup helpers, so every branch that changes behavior is visible at the call site.
#[derive(Debug, Clone, Copy)]
struct StartupRoutingFlags {
    /// Installer continuation after elevation: go straight into the installer window.
    force_run_installer: bool,
    /// `--ignore-installed`: never discover, modify, or replace an installed copy.
    ignore_installed: bool,
}

/// Where the no-open-target startup path goes; decided by [`startup_routing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupRoute {
    /// Straight into the installer window (installer continuation after elevation).
    Installer,
    /// No usable Python environment: the existing-install discovery (Windows) and the
    /// "install the Python environment?" prompt run first; choosing the launcher there
    /// continues to it, with the update check as given.
    MissingEnvironmentPrompt { update_check: bool },
    /// The launcher, with or without the startup update check.
    Launcher { update_check: bool },
}

/// The pure routing decision of `resolve_project_dir_without_cli_arg`.
///
/// - Without installed-copy management on this platform (`installed_copy_management` =
///   `PlatformFeature::InstalledCopyManagement`) nothing installer-related runs: no installer
///   continuation, no install prompts, no update check — the launcher opens directly.
/// - Installer continuation (`flags.force_run_installer`, or an `has_auto_install_target`
///   from the elevated relaunch) enters the installer.
/// - Under `--ignore-installed` the environment is the developer's business: no prompt.
///   Otherwise a missing environment (`has_python_env == false`) asks first.
///
/// `has_python_env` is read only where it can matter (installed-copy management available, no
/// `--ignore-installed`, no installer continuation).
const fn startup_routing(flags: StartupRoutingFlags, has_auto_install_target: bool, installed_copy_management: bool, has_python_env: bool) -> StartupRoute {
    let update_check = update_check_enabled(flags.ignore_installed, installed_copy_management);
    if !installed_copy_management {
        return StartupRoute::Launcher { update_check };
    }
    if flags.force_run_installer || has_auto_install_target {
        return StartupRoute::Installer;
    }
    if !flags.ignore_installed && !has_python_env {
        return StartupRoute::MissingEnvironmentPrompt { update_check };
    }
    StartupRoute::Launcher { update_check }
}

fn resolve_cli_project_dir(project_dir: &Path) -> anyhow::Result<Option<shell::target::StartupTarget>> {
    let path = project_dir.to_path_buf();
    match crate::project_scan::validate_project_dir_for_startup(&path) {
        crate::project_scan::ProjectValidationState::Valid { .. } => Ok(Some(shell::target::StartupTarget::Project(path))),
        crate::project_scan::ProjectValidationState::Invalid { message } => {
            let full_message = format!("--project path is invalid: {message}");
            show_startup_error_dialog(&full_message);
            anyhow::bail!("{full_message}")
        }
    }
}

/// Validates the command-line image paths (`--image` values or positional paths, in order)
/// before any window exists. Only existence and kind are checked here (`args::check_image_arg`).
///
/// When at least one path passes, ALL paths go to the studio open worker in the given order: the
/// worker reports every unusable one in the session's skipped list, so a bad file is reported in
/// one place only. When none passes, the startup error dialog shows each path's problem (for one
/// path exactly the message a single-image start always showed) and startup ends, mirroring
/// `resolve_cli_project_dir`.
///
/// # Errors
/// No path is an existing regular file (missing, a directory — the message names `--project` —
/// or another kind of non-file).
fn resolve_cli_images(images: &[PathBuf]) -> anyhow::Result<shell::target::StartupTarget> {
    use crate::project::single_image::SingleImageError;
    let mut problems = Vec::new();
    for path in images {
        let message = match args::check_image_arg(path) {
            Ok(()) => continue,
            Err(args::ImageArgProblem::NotFound) => SingleImageError::NotFound { path: path.clone() }.user_message(),
            Err(args::ImageArgProblem::Directory) => tf!("startup.image.directory_error", path = path.display()),
            Err(args::ImageArgProblem::NotAFile) => SingleImageError::NotAFile { path: path.clone() }.user_message(),
        };
        problems.push(message);
    }
    if problems.len() < images.len() {
        if !problems.is_empty() {
            runtime_log::log_warn(format!(
                "[startup] {} of {} command-line image path(s) are not files; the open worker lists them as skipped",
                problems.len(),
                images.len()
            ));
        }
        let sources = shell::target::ImageSources::new(images.to_vec(), Vec::new(), Vec::new());
        return sources.map(shell::target::StartupTarget::Images).ok_or_else(|| anyhow::Error::msg("no image path was given"));
    }
    let message = problems.join("\n");
    show_startup_error_dialog(&message);
    anyhow::bail!("image path is invalid: {message}")
}

/// Relaunches this executable with `--ignore-installed` and returns once the child is spawned.
///
/// This is the only way to enter standalone mode from the existing-install window: the flag is
/// consumed during startup routing (`StartupRoutingFlags`) and seeded into a process-global
/// `OnceLock` for the backend socket path long before the window opens, so it cannot be turned
/// on in place. The caller must let this process exit right afterwards, otherwise two copies
/// compete for the same runtime root.
///
/// `working_dir` is the program directory the child should start in — the same root this copy
/// was resolved against.
///
/// # Errors
/// Returns an error when the current executable cannot be located or the child cannot be
/// spawned; the message is localized because it is shown to the user.
#[cfg(target_os = "windows")]
fn spawn_self_in_standalone_mode(working_dir: &Path) -> anyhow::Result<()> {
    let current_exe = std::env::current_exe()
        .map_err(|e| anyhow::Error::msg(tf!("startup.standalone.current_exe_error", e = e)))?;
    let original_args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let relaunch_args = args::standalone_relaunch_args(&original_args);
    runtime_log::log_info(format!(
        "[startup] relaunching '{}' in standalone mode from '{}'",
        current_exe.display(),
        working_dir.display()
    ));
    std::process::Command::new(&current_exe)
        .current_dir(working_dir)
        .args(&relaunch_args)
        .spawn()
        .map_err(|e| {
            anyhow::Error::msg(tf!("startup.standalone.relaunch_error", current_exe = current_exe.display(), e = e))
        })?;
    Ok(())
}

/// The no-open-target startup path: installer continuation, or — with no usable Python
/// environment — the Windows existing-install check and the missing-environment prompt, each as
/// its own window; otherwise (or when the user picks it there) the launcher. `None` = exit.
///
/// # Errors
/// An installer or update window failed, or the standalone relaunch failed.
fn resolve_start_without_cli_arg(flags: StartupRoutingFlags, auto_install_target: Option<PathBuf>) -> anyhow::Result<Option<StartPlan>> {
    let installed_copy_management = installed_copy_management_available();
    let program_dir = config::program_dir();
    // The probe is skipped where `startup_routing` ignores its answer (no installed-copy
    // management on this platform, or `--ignore-installed`); it is a few cheap `stat`s.
    let has_python_env = installed_copy_management
        && !flags.ignore_installed
        && python_manager::has_supported_python_env(&program_dir);
    let route = startup_routing(flags, auto_install_target.is_some(), installed_copy_management, has_python_env);
    let update_check = match route {
        StartupRoute::Installer => {
            run_startup_installer(program_dir, auto_install_target)?;
            return Ok(None);
        }
        StartupRoute::MissingEnvironmentPrompt { update_check } => {
            #[cfg(target_os = "windows")]
            if route_existing_windows_install(&program_dir)? {
                return Ok(None);
            }
            match prompt_missing_python_env_action() {
                MissingPythonEnvAction::Install => {
                    run_startup_installer(program_dir, None)?;
                    return Ok(None);
                }
                MissingPythonEnvAction::UpdateCustom(target) => {
                    let outcome = installer::update::run_external_install_update_window(target, HOST_VERSION).map_err(anyhow::Error::msg)?;
                    runtime_log::log_info(format!("[startup] custom-folder update window closed: {outcome:?}"));
                    return Ok(None);
                }
                MissingPythonEnvAction::LaunchLauncher => {}
            }
            update_check
        }
        StartupRoute::Launcher { update_check } => update_check,
    };
    Ok(Some(StartPlan { initial: shell::InitialScreen::Launcher, update_check }))
}

/// Windows only, missing Python environment: looks for an installed copy of the program and
/// acts on the user's choice in the existing-install window. `true` ends the startup (the
/// program exits); `false` continues to the missing-environment prompt (no install found, or the
/// detection failed, which is logged).
///
/// # Errors
/// The update window or the standalone relaunch failed.
#[cfg(target_os = "windows")]
fn route_existing_windows_install(program_dir: &Path) -> anyhow::Result<bool> {
    let existing_install_action = match launcher_install::handle_existing_windows_install(program_dir, HOST_VERSION) {
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
        launcher_install::ExistingInstallAction::NoInstallFound => Ok(false),
        launcher_install::ExistingInstallAction::ExitCurrentCopy => Ok(true),
        launcher_install::ExistingInstallAction::StartInstaller(target_dir) => {
            run_startup_installer(program_dir.to_path_buf(), Some(target_dir))?;
            Ok(true)
        }
        launcher_install::ExistingInstallAction::UpdateInstalled(target) => {
            let outcome = installer::update::run_external_install_update_window(target, HOST_VERSION).map_err(anyhow::Error::msg)?;
            runtime_log::log_info(format!("[startup] installed-copy update window closed: {outcome:?}"));
            Ok(true)
        }
        // Its own arm on purpose: it must NOT fall through to the missing-Python prompt
        // the way `NoInstallFound` does — the user asked to run THIS copy as it is.
        launcher_install::ExistingInstallAction::RunStandalone => {
            spawn_self_in_standalone_mode(program_dir)?;
            Ok(true)
        }
    }
}

/// Reports that self-update is unavailable when the program runs from a source checkout.
///
/// The updater replaces the running executable in place; under `--ignore-installed` that
/// executable is a build product — either the one in `target/` or the copy `run-dev` publishes
/// into the project root — so replacing it would overwrite a local build with a release binary.
/// Both the log and a modal dialog say what to do instead.
fn refuse_self_update_from_sources() {
    runtime_log::log_warn(
        "[startup] self-update refused: running from sources (--ignore-installed)",
    );
    show_startup_error_dialog(t!("startup.ignore_installed.update_refused"));
}

/// `PlatformFeature::InstalledCopyManagement` for the startup routing. When the platform lacks
/// it (no installer, update check, install prompts or update windows), the first call logs that
/// once per process; the routing loop asks again on every return to the launcher.
pub(super) fn installed_copy_management_available() -> bool {
    static LOGGED_UNAVAILABLE: std::sync::Once = std::sync::Once::new();
    let available = PlatformFeature::InstalledCopyManagement.is_available();
    if !available {
        LOGGED_UNAVAILABLE.call_once(|| {
            runtime_log::log_info("[startup] installed-copy management unavailable on this platform: no installer, install prompts or update check");
        });
    }
    available
}

/// Whether the startup update check runs: never from a source checkout (`--ignore-installed`)
/// and never on a platform without installed-copy management.
pub(super) const fn update_check_enabled(ignore_installed: bool, installed_copy_management: bool) -> bool {
    installed_copy_management && !ignore_installed
}

#[cfg(test)]
mod startup_routing_tests {
    use super::{StartupRoute, StartupRoutingFlags, startup_routing};

    fn flags(force_run_installer: bool, ignore_installed: bool) -> StartupRoutingFlags {
        StartupRoutingFlags { force_run_installer, ignore_installed }
    }

    #[test]
    fn installer_continuation_enters_the_installer() {
        assert_eq!(startup_routing(flags(true, false), false, true, true), StartupRoute::Installer);
        assert_eq!(startup_routing(flags(false, false), true, true, false), StartupRoute::Installer);
    }

    #[test]
    fn missing_environment_prompts_before_the_launcher() {
        assert_eq!(startup_routing(flags(false, false), false, true, false), StartupRoute::MissingEnvironmentPrompt { update_check: true });
    }

    #[test]
    fn ready_environment_opens_the_launcher_with_the_update_check() {
        assert_eq!(startup_routing(flags(false, false), false, true, true), StartupRoute::Launcher { update_check: true });
    }

    #[test]
    fn ignore_installed_skips_prompt_and_update_check() {
        assert_eq!(startup_routing(flags(false, true), false, true, false), StartupRoute::Launcher { update_check: false });
    }

    #[test]
    fn no_installed_copy_management_opens_the_launcher_without_any_installer_step() {
        for force_run_installer in [false, true] {
            for has_auto_install_target in [false, true] {
                for has_python_env in [false, true] {
                    assert_eq!(
                        startup_routing(flags(force_run_installer, false), has_auto_install_target, false, has_python_env),
                        StartupRoute::Launcher { update_check: false },
                        "force={force_run_installer} target={has_auto_install_target} env={has_python_env}"
                    );
                }
            }
        }
    }
}
