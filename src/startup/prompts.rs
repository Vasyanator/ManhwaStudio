/*
FILE OVERVIEW: src/startup/prompts.rs
The pre-window startup windows and the startup error box. Each runs before the main window
exists, ONCE per process.

- `prompt_missing_python_env_action`: the "install / update a custom folder / open the
  launcher" prompt shown when no usable Python environment exists. The window itself lives in
  `ms_installer::prompt_window` next to the installer windows; this wrapper only logs a window
  that could not run and answers `LaunchLauncher` for it.
- `run_startup_installer`: the installer window (installer continuation or the user's
  "install" choice); a finished install may start the installed copy as a separate process.
- `show_startup_error_dialog`: `ms_platform::message::show_error_blocking`, log-only where no
  native box exists (Android).
*/

use super::HOST_VERSION;
use crate::installer::install as launcher_install;
use crate::runtime_log;
use std::path::PathBuf;

/// The user's answer in the missing-environment prompt (declared next to the window in
/// `ms_installer::prompt_window`).
pub(super) use crate::installer::prompt_window::MissingPythonEnvAction;

/// Runs the missing-environment prompt as its own blocking window and returns the user's choice.
/// A window that could not run is logged and answers `LaunchLauncher`.
pub(super) fn prompt_missing_python_env_action() -> MissingPythonEnvAction {
    crate::installer::prompt_window::run_missing_python_env_prompt().unwrap_or_else(|err| {
        runtime_log::log_warn(format!("missing-env prompt failed: {err}"));
        MissingPythonEnvAction::LaunchLauncher
    })
}

/// Runs the installer window for `app_dir` (installer continuation, or the user's "install"
/// choice); the program exits afterwards in every case. A finished install that offers to launch
/// the installed copy starts it as a separate process.
///
/// # Errors
/// The installer window failed, the installer reported a failure, or the installed copy could not
/// be started.
pub(super) fn run_startup_installer(app_dir: PathBuf, auto_install_target: Option<PathBuf>) -> anyhow::Result<()> {
    match launcher_install::run_python_installer_window(&app_dir, auto_install_target, HOST_VERSION).map_err(anyhow::Error::msg)? {
        launcher_install::InstallerOutcome::Completed
        | launcher_install::InstallerOutcome::ElevatedRelaunchStarted
        | launcher_install::InstallerOutcome::Cancelled => Ok(()),
        launcher_install::InstallerOutcome::LaunchLauncher(install_dir) => {
            let exe = launcher_install::spawn_installed_program_copy(&install_dir).map_err(anyhow::Error::msg)?;
            runtime_log::log_info(format!("[startup] started the installed copy '{}'", exe.display()));
            Ok(())
        }
        launcher_install::InstallerOutcome::Failed(err) => Err(anyhow::Error::msg(err)),
    }
}

/// Shows a startup failure (before any window exists) in the native error box and logs it.
/// Where the platform has no native message box (Android), the error-level log line IS the
/// outcome (it reaches logcat) until an in-app dialog exists there.
pub(super) fn show_startup_error_dialog(message: &str) {
    runtime_log::log_error(format!("startup error dialog shown: {message}"));
    if let Err(unavailable) = ms_platform::message::show_error_blocking(t!("startup.error_dialog.title"), message) {
        runtime_log::log_error(format!(
            "[startup] the startup error could not be shown in a dialog; it is reported only in this log.\nReason: {unavailable:?}\nMessage: {message}"
        ));
    }
}
