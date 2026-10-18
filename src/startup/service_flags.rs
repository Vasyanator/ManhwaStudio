/*
FILE OVERVIEW: src/startup/service_flags.rs
Command-line validation and the service flags that end the process with an exact exit code
(`std::process::exit` in `run_native`, never an `anyhow` return, because scripts and the
launcher branch on the code).

- `exit_with_usage_error`: reports the contradictory flag combinations `mode::decide` found
  (`mode::UsageConflicts`: installed-copy flags with `--ignore-installed`, image / `--project`
  conflicts, Windows-only flags off Windows) and exits with `CLI_USAGE_ERROR_EXIT`.
- `run_system_registration_helper`: the elevated system-registration helper (Windows; elsewhere
  an "unsupported" diagnostic and `CLI_USAGE_ERROR_EXIT`).
- `report_self_update_refused`: the logged and printed refusal of `--update` /
  `--continue-update` on a build without `InstalledCopyManagement` (exit 0).
- `run_check_venv_flow` / `run_environment_repair_and_report`: `--check-venv`, exit code
  `CHECK_VENV_EXIT_READY` / `CHECK_VENV_EXIT_NOT_READY`.

The early paths (usage error, helper, self-update refusal) run before `services::init_full` and
share `install_early_diagnostic_locale`: the storage-mode probe first (the process's first
document access, rule (a)), then the EMBEDDED UI catalog read-only, so their diagnostics are
localized before the on-disk locale is reconciled.
*/

#[cfg(target_os = "windows")]
use super::HOST_VERSION;
use super::mode::UsageConflicts;
use crate::args::Cli;
use crate::installer::install as launcher_install;
use crate::{config, locale_store, runtime_log, venv_check};
use ms_platform::PlatformFeature;
use std::path::Path;

/// Process exit code for an impossible command line (a usage error, mirroring clap's
/// own exit code for one). Distinct from [`CHECK_VENV_EXIT_NOT_READY`], which reports a
/// well-formed run whose environment is unusable.
const CLI_USAGE_ERROR_EXIT: i32 = 2;

/// The early locale bring-up of the terminal paths that run before `services::init_full` (usage
/// error, registration helper, refused self-update), so their diagnostics are localized.
///
/// The UI catalog is normally installed much later in startup (it needs the user config), and
/// `t!` would return raw keys here. Order, as `init_full`'s rule (a) requires of every process:
/// the storage-mode probe (`init_storage_mode_at_startup`) runs FIRST, because it seeds the
/// docstore default format that resolves the "both files exist" rule of the user-config read that
/// follows. These paths end the process (or `run_native`) without reaching `init_full`, so the
/// probe still runs exactly once per process. Everything else only READS: the first-run language
/// marker and the user config stay untouched. The EMBEDDED catalog is used on purpose — the
/// on-disk `locale/` folder is only reconciled later in startup, so a freshly shipped key may not
/// be in it yet.
fn install_early_diagnostic_locale() {
    // The probe's `reconciliation_pending` is deliberately not acted on: only the main run starts
    // the storage conversion (`init_full`), and these paths never open a window. The probe logs
    // its own outcome. The probe itself is infallible.
    let _probe = config::storage_mode::init_storage_mode_at_startup();
    // An unreadable `user_config` only costs the diagnostic its UI language (English fallback);
    // the read error is logged instead of being dropped.
    let raw_settings = config::load_raw_user_settings_for_startup().unwrap_or_else(|err| {
        runtime_log::log_warn(format!("[startup] could not read user_config for the diagnostic locale; using English: {err:#}"));
        serde_json::Value::Null
    });
    locale_store::install_embedded_ui_locale(&raw_settings);
}

/// Reports a command line that combines contradictory flags and ends the process with
/// [`CLI_USAGE_ERROR_EXIT`]: `--ignore-installed` with a flag that manages an installed copy, an
/// image to open with a terminal mode or an installed-copy flag, `--project` with a terminal
/// mode, or a Windows-only service flag on another OS. The decision is
/// `mode::decide` (`RunMode::Service(ServiceMode::UsageError)`); this only reports it.
///
/// Logs and prints one user-facing diagnostic per non-empty conflict list to stderr. It must
/// terminate rather than ignore a flag, because silently dropping, say, `--uninstall` would be
/// as surprising as honoring it.
pub(super) fn exit_with_usage_error(conflicts: &UsageConflicts) -> ! {
    install_early_diagnostic_locale();
    let mut messages = Vec::with_capacity(4);
    if !conflicts.installed_copy.is_empty() {
        messages.push(tf!(
            "startup.ignore_installed.conflicting_flags",
            flags = conflicts.installed_copy.join(", ")
        ));
    }
    if !conflicts.image.is_empty() {
        messages.push(tf!("startup.image.conflicting_flags", flags = conflicts.image.join(", ")));
    }
    if !conflicts.project.is_empty() {
        messages.push(tf!("startup.project.conflicting_flags", flags = conflicts.project.join(", ")));
    }
    if !conflicts.windows_only.is_empty() {
        messages.push(tf!("startup.windows_only.unsupported_flags_error", flags = conflicts.windows_only.join(", ")));
    }
    for message in &messages {
        runtime_log::log_error(format!("[startup] {message}"));
        eprintln!("{message}");
    }
    std::process::exit(CLI_USAGE_ERROR_EXIT);
}

/// Reports `ServiceMode::SelfUpdateRefused` (`--update` / `--continue-update` on a build without
/// `PlatformFeature::InstalledCopyManagement`): logs the refusal with the same text as a refused
/// launcher-requested update (`routing::run_launcher_requested_update`) and prints one localized
/// line to stderr. The caller returns normally (exit 0): a refusal, not a usage error.
pub(super) fn report_self_update_refused(cli: &Cli) {
    install_early_diagnostic_locale();
    let unavailable = PlatformFeature::InstalledCopyManagement.require().err();
    runtime_log::log_error(format!("[startup] self-update requested on a platform without installed-copy management; refused: {unavailable:?} (--update={}, --continue-update={})", cli.update, cli.continue_update));
    eprintln!("{}", t!("startup.self_update.unsupported_platform_error"));
}

/// Runs this process as the elevated system-registration helper
/// (`--system-registration-apply` + `--system-registration-result`, clap requires both) and
/// returns its exit code (`ms_os_integration::actions::HELPER_EXIT_*`). Windows only; elsewhere
/// the flags are rejected with a localized "unsupported" diagnostic and [`CLI_USAGE_ERROR_EXIT`].
pub(super) fn run_system_registration_helper(cli: &Cli) -> i32 {
    // Localized, so the helper's failure texts (written into its result file) are too.
    install_early_diagnostic_locale();
    #[cfg(target_os = "windows")]
    {
        let (Some(actions), Some(result_file)) =
            (cli.system_registration_apply.as_deref(), cli.system_registration_result.as_deref())
        else {
            runtime_log::log_error("[startup] --system-registration-apply without --system-registration-result");
            return CLI_USAGE_ERROR_EXIT;
        };
        ms_os_integration::windows::elevation::run_elevated_helper(actions, result_file, HOST_VERSION.version)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let message = t!("startup.system_registration.unsupported_error");
        runtime_log::log_error(format!(
            "[startup] {message} (--system-registration-apply {:?})",
            cli.system_registration_apply
        ));
        eprintln!("{message}");
        CLI_USAGE_ERROR_EXIT
    }
}

/// Process exit code of `--check-venv` when the environment is usable.
const CHECK_VENV_EXIT_READY: i32 = 0;
/// Process exit code of `--check-venv` when the environment is NOT usable: it was
/// incomplete and the user cancelled the repair, a repair stage failed, or the check
/// itself could not verify readiness. Scripts branch on exactly this value.
const CHECK_VENV_EXIT_NOT_READY: i32 = 1;

/// Runs the `--check-venv` flow for `root_dir` and returns the process exit code.
///
/// Silent success path: when the environment already provides everything the recorded
/// install type requires, this only logs and prints one line, opening no window. Otherwise
/// the installer is opened in environment-repair mode, starting on the dependency-profile
/// screen; the repair provisions the environment of `root_dir` and nothing else.
///
/// Returns [`CHECK_VENV_EXIT_READY`] when the environment is (or has just been made)
/// usable and [`CHECK_VENV_EXIT_NOT_READY`] otherwise. Never opens the launcher or the
/// studio, and never starts the AI backend.
pub(super) fn run_check_venv_flow(root_dir: &Path) -> i32 {
    match venv_check::check_environment(root_dir) {
        venv_check::EnvironmentReadiness::Ready => {
            runtime_log::log_info(format!(
                "[check-venv] environment at '{}' is complete",
                root_dir.display()
            ));
            println!("{}", t!("venv_check.ready_message"));
            CHECK_VENV_EXIT_READY
        }
        venv_check::EnvironmentReadiness::NotReady(reason) => {
            let message = reason.user_message();
            runtime_log::log_warn(format!(
                "[check-venv] environment at '{}' is not ready: {reason:?}",
                root_dir.display()
            ));
            println!("{}", tf!("venv_check.not_ready_message", reason = message));
            run_environment_repair_and_report(root_dir)
        }
    }
}

/// Opens the environment-repair installer window and maps its outcome to an exit code.
///
/// A reported success is VERIFIED before returning [`CHECK_VENV_EXIT_READY`]: the same
/// readiness check runs again, so a repair that finished without actually satisfying the
/// contract (a stage that under-installed, a config the worker could not record) exits
/// non-zero instead of handing a broken environment to the caller.
fn run_environment_repair_and_report(root_dir: &Path) -> i32 {
    let outcome = match launcher_install::run_environment_repair_window(root_dir) {
        Ok(outcome) => outcome,
        Err(err) => {
            runtime_log::log_error(format!("[check-venv] repair window failed: {err}"));
            eprintln!("{}", tf!("venv_check.repair_failed_message", err = err));
            return CHECK_VENV_EXIT_NOT_READY;
        }
    };
    match outcome {
        launcher_install::InstallerOutcome::Completed => {
            match venv_check::check_environment(root_dir) {
                venv_check::EnvironmentReadiness::Ready => {
                    runtime_log::log_info("[check-venv] environment repaired");
                    println!("{}", t!("venv_check.repair_completed_message"));
                    CHECK_VENV_EXIT_READY
                }
                venv_check::EnvironmentReadiness::NotReady(reason) => {
                    runtime_log::log_error(format!(
                        "[check-venv] repair reported success but the environment is still not ready: {reason:?}"
                    ));
                    eprintln!(
                        "{}",
                        tf!(
                            "venv_check.repair_incomplete_message",
                            reason = reason.user_message()
                        )
                    );
                    CHECK_VENV_EXIT_NOT_READY
                }
            }
        }
        launcher_install::InstallerOutcome::Cancelled => {
            runtime_log::log_warn("[check-venv] environment repair cancelled by the user");
            eprintln!("{}", t!("venv_check.repair_cancelled_message"));
            CHECK_VENV_EXIT_NOT_READY
        }
        launcher_install::InstallerOutcome::Failed(err) => {
            runtime_log::log_error(format!("[check-venv] environment repair failed: {err}"));
            eprintln!("{}", tf!("venv_check.repair_failed_message", err = err));
            CHECK_VENV_EXIT_NOT_READY
        }
        // Neither outcome is reachable in repair mode (it never elevates and never
        // deploys a copy to launch), but the enum is matched exhaustively on purpose:
        // a future variant must be considered here rather than silently pass as ready.
        launcher_install::InstallerOutcome::ElevatedRelaunchStarted
        | launcher_install::InstallerOutcome::LaunchLauncher(_) => {
            runtime_log::log_error(
                "[check-venv] unexpected installer outcome in environment-repair mode",
            );
            eprintln!("{}", t!("venv_check.repair_unexpected_outcome_message"));
            CHECK_VENV_EXIT_NOT_READY
        }
    }
}
