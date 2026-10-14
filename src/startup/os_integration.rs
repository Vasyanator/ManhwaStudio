/*
FILE OVERVIEW: src/startup/os_integration.rs
Fire-and-forget startup workers that touch OS state outside the program directory. Neither
blocks startup; every outcome and failure is logged.

- `spawn_stale_scratch_sweep`: deletes single-image scratch sessions of crashed runs
  (lock-guarded, so a live instance's session is never touched).
- `install_linux_desktop_integration_async` (Linux): keeps this copy's desktop entry and icon
  current; text, ownership rule and writer live in `ms_os_integration::linux`.
*/

use crate::{config, project, runtime_log};
use ms_thread as thread;

/// Starts the one-shot startup sweep of stale single-image scratch sessions
/// (`ms_project::single_image::sweep_stale_sessions` over `scratch_base()`) on a named worker
/// and logs its report. Never blocks startup; a spawn failure is logged and the next start
/// retries.
pub(super) fn spawn_stale_scratch_sweep() {
    let spawn_result = thread::Builder::new()
        .name("single-image-scratch-sweep".to_string())
        .spawn(|| {
            let base = config::single_image::scratch_base();
            let report = project::single_image::sweep_stale_sessions(&base);
            // Each removal and each error is already logged by the sweep itself; this line
            // records that the sweep ran and what it saw.
            runtime_log::log_info(format!(
                "[startup] single-image scratch sweep of '{}': removed={} live={} errors={}",
                base.display(),
                report.removed,
                report.live,
                report.errors.len()
            ));
        });
    if let Err(err) = spawn_result {
        runtime_log::log_warn(format!("[startup] could not start the single-image scratch sweep: {err}"));
    }
}

/// Keeps this copy's Linux desktop entry and icon current on a named background thread
/// (`ms_os_integration::linux::xdg::ensure_at_startup`: create if missing, refresh only if
/// ours and different, never take over another copy's entry). Best effort: every outcome and
/// failure is logged and nothing else happens.
#[cfg(target_os = "linux")]
pub(super) fn install_linux_desktop_integration_async() {
    use ms_os_integration::linux::desktop_entry::StartupOutcome;
    use ms_os_integration::linux::xdg::{DesktopDirs, ensure_at_startup};

    let spawn_result = thread::Builder::new()
        .name("desktop-integration-installer".to_string())
        .spawn(|| {
            let identity = match ms_os_integration::CopyIdentity::current(None) {
                Ok(identity) => identity,
                Err(err) => {
                    runtime_log::log_warn(format!("[desktop-integration] {err}; desktop entry not checked"));
                    return;
                }
            };
            let Some(dirs) = DesktopDirs::from_process_env() else {
                runtime_log::log_warn(
                    "[desktop-integration] neither XDG_DATA_HOME nor HOME is an absolute path; desktop entry not checked",
                );
                return;
            };
            let entry = dirs.entry_path();
            match ensure_at_startup(&identity, &dirs) {
                Ok(written @ (StartupOutcome::Created | StartupOutcome::Refreshed)) => runtime_log::log_info(format!(
                    "[desktop-integration] {} '{}' for '{}' (Path='{}', dev copy: {})",
                    if written == StartupOutcome::Created { "created" } else { "refreshed" },
                    entry.display(),
                    identity.exe.display(),
                    identity.program_root.display(),
                    identity.is_dev_copy()
                )),
                Ok(StartupOutcome::AlreadyCurrent) => runtime_log::log_info(format!(
                    "[desktop-integration] '{}' is current for '{}'",
                    entry.display(),
                    identity.exe.display()
                )),
                Ok(StartupOutcome::LeftForeign { exe }) => runtime_log::log_info(format!(
                    "[desktop-integration] '{}' belongs to another copy '{}' (exists: {}); left untouched",
                    entry.display(),
                    exe.display(),
                    exe.exists()
                )),
                // The cause was already logged by `ensure_at_startup`.
                Ok(StartupOutcome::Unreadable) => {}
                Err(err) => runtime_log::log_warn(format!(
                    "[desktop-integration] could not write the Linux desktop entry: {err}"
                )),
            }
        });
    if let Err(err) = spawn_result {
        runtime_log::log_warn(format!(
            "[desktop-integration] could not start the desktop-integration thread: {err}"
        ));
    }
}
