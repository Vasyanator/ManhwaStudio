/*
FILE OVERVIEW: src/startup/mod.rs
Native process startup of the `manhwastudio_rs` package (the whole module is compiled out on
wasm32; `lib.rs` declares it behind one `cfg(not(target_arch = "wasm32"))`). `lib.rs::native_main`
(desktop binary and the Android `android_main`) calls `run_native`, which runs the startup
sequence below in this exact order. Every `eframe::NativeOptions` built under this module sets
`android_app` from `window_geometry::android_app::current()` on Android (compiled out elsewhere).

Main flow (`run_native`): parse -> `mode::decide` (the ONE run-mode decision, pure and
table-tested) -> match on the `RunMode`:
- Usage error (`service_flags.rs`) and the elevated system-registration helper exit before any
  other side effect; a self-update refused for lack of `InstalledCopyManagement` only reports and
  returns. Their localized report runs the storage-mode probe first (the process's first document
  access, `services::init_full` rule (a)), then reads `user_config` for the UI language.
- Every other mode first runs `init_minimal` (startup workers from `os_integration.rs`: the stale
  single-image scratch sweep and, unless `--ignore-installed`, the Linux desktop entry; IPC
  version, isolated backend socket, trace). The Windows service flags (Start Menu shortcut,
  uninstall) then run and return.
- `--check-venv` and the main run then run `services::init_full`, the ONE owner of the
  startup-order contract: the process-global settings (storage mode first, locale, UI scale,
  autosave policy, secondary windows; config seeds in `seed.rs`), returning `ProcessServices`.
  `--check-venv` is terminal.
- The main run (`run_main`): `--continue-update` / `--update` windows first, then either
  `--test-launcher` (the main window in test mode, own title / app id; any exit quits) or the
  default: the AI backend supervisor with the process's `LocalAiAvailability` (`--no-ai`
  switches off the Python backend and native ONNX), the images another app delivered at startup
  (Android Share / "Open with", `shell::incoming::cold_start_target`, pre-window),
  `resolve_start` (`routing.rs`), then `shell::run` — the ONE main window (`src/shell/`) on the
  launcher or the studio. Once `shell::run` returns (the window is closed) the background
  settings writes are waited for, bounded (`wait_for_settings_writes_after_window`). A launcher
  "Update" request runs the update window after the main window has closed.

Key items: `run_native`, `init_minimal`, `run_main`, `HOST_VERSION`, `launcher_host` and the
one session-log init (`init_startup_logging_best_effort`, before the mode split).

Submodules: `mode` (the run-mode decision), `routing` (where the main window starts, update
routing), `service_flags` (usage-error report, the registration helper, `--check-venv`),
`prompts` (pre-window prompt windows and the startup error box), `seed` (config-driven
process-global seeds), `services` (`ProcessServices`, `init_full` and the startup-order
contract), `os_integration` (startup workers touching OS state). See `MODULE_README.md` for the startup-order contract.
*/

mod mode;
mod os_integration;
mod prompts;
mod routing;
mod seed;
mod service_flags;
mod services;

#[cfg(target_os = "linux")]
use os_integration::install_linux_desktop_integration_async;
use os_integration::spawn_stale_scratch_sweep;
use routing::{installed_copy_management_available, resolve_start, run_launcher_requested_update, update_check_enabled};
use mode::{MainMode, MainRun, PlatformFacts, RunMode, ServiceMode, UpdatePrelude, decide};
use service_flags::{exit_with_usage_error, report_self_update_refused, run_check_venv_flow, run_system_registration_helper};
use services::{ProcessServices, init_full};

use crate::args::Cli;
#[cfg(target_os = "windows")]
use crate::installer::install as launcher_install;
use crate::installer::update::UpdateWindowOutcome;
use crate::{backend_ipc, config, installer, launcher, runtime_log, shell, trace, trace_log, version_format};
use clap::Parser;
#[cfg(target_os = "windows")]
use std::path::Path;

/// The application's own version pair, handed to `ms-installer` at its entry points.
///
/// It cannot be read inside that crate: `env!("CARGO_PKG_VERSION")` compiled there would
/// yield the LIBRARY's version, and `MS_APP_VERSION` is a `rustc-env` that `build.rs`
/// emits for this crate only. The two halves are not interchangeable — `core` is the
/// machine-facing value every release comparison reduces with `version_format::version_core`,
/// `display` is the git-derived string shown to the user and never parsed.
const HOST_VERSION: ms_installer::HostVersion = ms_installer::HostVersion {
    core: env!("CARGO_PKG_VERSION"),
    display: env!("MS_APP_VERSION"),
};

/// The process facts the launcher needs from startup: `--ignore-installed` (the System
/// registration tab is read-only and its warnings silent) and this build's version core (the
/// installer's reduction, `install.rs`), which a written Uninstall entry carries.
fn launcher_host(ignore_installed: bool) -> launcher::state::LauncherHost {
    launcher::state::LauncherHost { ignore_installed, version_core: version_format::version_core(HOST_VERSION.core) }
}

/// The whole native startup sequence, called only by `lib.rs::native_main`, which logs how it
/// ended: parse the command line, decide the run mode ONCE (`mode::decide`, pure and
/// table-tested), then run that mode. Each match arm performs exactly the process-wide side
/// effects its mode needs, in the contract order (see `MODULE_README.md`): only the storage-mode
/// probe (inside their report, `service_flags::install_early_diagnostic_locale`) for the usage
/// error, the registration helper and the refused self-update, `init_minimal` for the Windows
/// service flags,
/// `init_minimal` + `init_full` for `--check-venv` and the main run. Service flags may end the
/// process directly (`std::process::exit`) with their own exit code instead of returning.
///
/// # Errors
/// A fatal startup error: an invalid CLI open target, a failed installer / update / prompt
/// window, unreadable user settings, or a main-window failure.
pub(crate) fn run_native() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The elevated system-registration helper never opens the session log: initializing it
    // rotates `last.log` -> `previous.log`, and the helper runs while the launcher that
    // started it still writes `last.log` (on Windows it also holds the file open). The helper
    // reports every outcome in its result file and the launcher logs them, so it runs without
    // a file log (`runtime_log` lines are dropped before init). The CLI is therefore parsed
    // before the log init; a clap usage error exits there without touching the logs. The skip
    // is keyed on the flag itself, so a usage error that includes the helper flag also runs
    // without the file log.
    if cli.system_registration_apply.is_none() {
        init_startup_logging_best_effort();
    }
    match decide(&cli, PlatformFacts::current()) {
        // Decided FIRST (top of the precedence): several service flags below (Start Menu
        // shortcut, uninstall, update continuation) act on an installed copy immediately, so an
        // impossible combination must be rejected before any of them can run.
        RunMode::Service(ServiceMode::UsageError(conflicts)) => exit_with_usage_error(&conflicts),
        // The elevated system-registration helper (a short-lived admin process the launcher
        // starts through UAC) does its batch and exits here, BEFORE every other startup side
        // effect (scratch sweep, desktop entry, backend socket): an elevated process must do
        // nothing else.
        RunMode::Service(ServiceMode::RegistrationHelper) => std::process::exit(run_system_registration_helper(&cli)),
        #[cfg(target_os = "windows")]
        RunMode::Service(ServiceMode::WindowsStartMenuShortcut { install_dir, continuation }) => {
            init_minimal(&cli);
            run_windows_start_menu_shortcut(&install_dir, continuation)
        }
        #[cfg(target_os = "windows")]
        RunMode::Service(ServiceMode::WindowsUninstall { continuation, signal_file }) => {
            init_minimal(&cli);
            run_windows_uninstall(continuation, signal_file.as_deref())
        }
        // Off Windows `decide` turns every Windows-only flag into a usage error, so these modes
        // cannot be decided here; the arm only keeps the match exhaustive and reports a broken
        // contract instead of panicking.
        #[cfg(not(target_os = "windows"))]
        RunMode::Service(mode @ (ServiceMode::WindowsStartMenuShortcut { .. } | ServiceMode::WindowsUninstall { .. })) => {
            anyhow::bail!("internal error: the Windows-only startup mode {mode:?} was decided on a non-Windows build")
        }
        // Environment check mode terminates the process: it must never fall through into the
        // launcher, the studio, or the AI backend supervisor. `process::exit` is used instead
        // of returning, because callers (run-dev scripts) branch on an EXACT code and an
        // `anyhow` error return would collapse every outcome into the generic failure exit
        // plus a printed backtrace-style message. It runs after the full seeding on purpose:
        // `venv_check` reads `General.ai_install_type`, which the startup auto-detection fills.
        RunMode::Service(ServiceMode::CheckVenv) => {
            init_minimal(&cli);
            init_full(&cli)?;
            std::process::exit(run_check_venv_flow(&config::program_dir()));
        }
        // Refused like a launcher-requested update on such a build
        // (`routing::run_launcher_requested_update`): logged (plus one stderr line for the CLI
        // user), no window, a normal return. Only the storage-mode probe of its localized report
        // runs; no other process-wide state is set up for it.
        RunMode::Service(ServiceMode::SelfUpdateRefused) => {
            report_self_update_refused(&cli);
            Ok(())
        }
        RunMode::Main(run) => {
            init_minimal(&cli);
            let services = init_full(&cli)?;
            run_main(&cli, run, services)
        }
    }
}

/// The process-wide side effects every mode past the registration helper needs, in order:
/// stale scratch sweep (worker), Linux desktop entry (worker, not under `--ignore-installed`),
/// the IPC studio version, the isolated backend socket name (`--ignore-installed`), trace init.
/// Touches no owned document.
fn init_minimal(cli: &Cli) {
    // Deletes single-image scratch sessions left behind by crashed runs. Lock-guarded, so a
    // session of another running instance is never touched; fire-and-forget on a worker.
    spawn_stale_scratch_sweep();
    // Desktop integration writes into the user's shared XDG data tree. It creates the entry
    // when missing and refreshes only an entry of THIS copy (another copy's entry is never
    // taken over); a run from a source checkout (`--ignore-installed`) does not touch it at
    // all, so this runs only after the CLI is known.
    #[cfg(target_os = "linux")]
    if !cli.ignore_installed {
        install_linux_desktop_integration_async();
    }
    // Hand the IPC crate this package's own version. It cannot read it itself — compiled
    // inside `ms-backend-ipc`, `env!("CARGO_PKG_VERSION")` yields that crate's `0.1.0` — and the
    // handshake log line exists precisely to make a mixed installation visible in
    // `last.log`, so a wrong number there defeats its only purpose. Diagnostic only: it
    // never reaches the wire. Seeded before any backend client exists; read-only afterwards.
    backend_ipc::set_studio_version(HOST_VERSION.core);
    // Give this copy its own backend socket so a dev build and an installed copy can run
    // side by side without adopting each other's Python backend. Seeded before any
    // backend client or supervisor exists; read-only afterwards.
    if cli.ignore_installed {
        let socket_name = backend_ipc::seed_isolated_backend_socket_name(&config::program_dir());
        runtime_log::log_info(format!(
            "[startup] --ignore-installed: isolated backend socket name '{socket_name}'"
        ));
    }
    if let Err(err) = trace::init_trace(&config::data_dir(), cli.trace) {
        eprintln!("failed to initialize tracing: {err}");
    }
    trace_log!(
        trace::cat::STARTUP,
        "tracing enabled, args: project={:?} images={:?} no_ai={} update={} test_launcher={} trace={}",
        cli.project,
        cli.image_args(),
        cli.no_ai,
        cli.update,
        cli.test_launcher,
        cli.trace
    );
}

/// `--create-start-menu-shortcut-install-dir` (Windows): creates the all-users Start Menu
/// shortcut of the copy in `install_dir`, relaunching elevated unless `continuation`.
///
/// # Errors
/// The installer's localized failure text.
#[cfg(target_os = "windows")]
fn run_windows_start_menu_shortcut(install_dir: &Path, continuation: bool) -> anyhow::Result<()> {
    launcher_install::run_windows_create_start_menu_shortcut_for_install(install_dir, continuation).map_err(anyhow::Error::msg)
}

/// `--uninstall` (Windows): uninstalls the running copy, relaunching elevated unless
/// `continuation`; the outcome is also written to `signal_file` when given.
///
/// # Errors
/// The installer's localized failure text.
#[cfg(target_os = "windows")]
fn run_windows_uninstall(continuation: bool, signal_file: Option<&Path>) -> anyhow::Result<()> {
    launcher_install::run_windows_uninstall_from_current_exe(continuation, signal_file).map_err(anyhow::Error::msg)
}

/// The main run after the full seeding: each update-window prelude in order (an `Exit` outcome
/// ends the process, any other falls through), then the test launcher or the main window.
///
/// # Errors
/// A failed update window, test-launcher or main window, an invalid CLI open target, or a
/// failed startup prompt (`resolve_start`).
fn run_main(cli: &Cli, run: MainRun, services: ProcessServices) -> anyhow::Result<()> {
    for prelude in run.preludes {
        // `--update` / `--continue-update` (like every other installed-copy flag) can no longer
        // arrive together with `--ignore-installed`: `decide` made that a usage error.
        let outcome = match prelude {
            UpdatePrelude::ContinueUpdate => installer::update::run_continue_update_window(HOST_VERSION),
            UpdatePrelude::Update => installer::update::run_update_window(cli.test_ver_check, HOST_VERSION),
        }
        .map_err(anyhow::Error::msg)?;
        if outcome == UpdateWindowOutcome::Exit {
            return Ok(());
        }
    }
    match run.main {
        MainMode::TestLauncher => run_test_launcher(cli, services),
        MainMode::Main => run_main_window(cli, services),
    }
}

/// `--test-launcher`: the main window in test mode with its own AI backend supervisor.
///
/// # Errors
/// A main-window failure.
fn run_test_launcher(cli: &Cli, services: ProcessServices) -> anyhow::Result<()> {
    let supervisor = services.start_ai_backend_supervisor();
    // Test mode routes nowhere: the shell quits on any launcher exit.
    let exit = shell::run(shell::ShellConfig {
        initial: shell::InitialScreen::Launcher,
        user_settings: services.user_settings,
        ai_backend: supervisor.handle(),
        host: services.host,
        test_mode: true,
        update_check: update_check_enabled(cli.ignore_installed, installed_copy_management_available()).then_some(cli.test_ver_check),
        incoming_error: None,
    });
    wait_for_settings_writes_after_window();
    let exit = exit?;
    runtime_log::log_info(format!("[startup] --test-launcher window closed ({exit:?})"));
    Ok(())
}

/// The default mode: AI backend supervisor, cold-start delivered images, the startup prompts
/// (`resolve_start`), the ONE main window, then a launcher-requested update window.
///
/// # Errors
/// An invalid CLI open target, a failed startup prompt or installer window, a main-window
/// failure, or a failed launcher-requested update.
fn run_main_window(cli: &Cli, services: ProcessServices) -> anyhow::Result<()> {
    // The single Python AI backend is owned here, above the main window, so it starts with the
    // launcher (per the autostart toggle) and survives every switch between the launcher and the
    // studio. Dropped (which stops the process + probe) on any return.
    let ai_backend_supervisor = services.start_ai_backend_supervisor();
    let ai_backend = ai_backend_supervisor.handle();
    let ProcessServices { user_settings, host, .. } = services;

    // Images another app handed over before (or by) starting the process (Android Share / "Open
    // with"): taken once, before the main window exists — the documented pre-window exception to
    // "no JNI on the GUI thread" (`ms_platform::incoming::init`). It also binds the warm-delivery
    // wake-up the main window polls. Nothing outside Android.
    let (incoming_target, incoming_error) = shell::incoming::cold_start_target();
    // The startup prompts (installer continuation, missing environment, Windows existing
    // install) run ONCE here, as their own windows, before the main window exists.
    let Some(start) = resolve_start(cli, incoming_target)? else {
        return Ok(());
    };
    // The ONE main window: launcher and studio are its screens, switched in place. The launcher
    // shows the storage reconciliation's progress; a direct `--project` or image start defers it
    // to the studio screen, after the load.
    let exit = shell::run(shell::ShellConfig {
        initial: start.initial,
        user_settings,
        ai_backend,
        host,
        test_mode: false,
        update_check: start.update_check.then_some(cli.test_ver_check),
        incoming_error,
    });
    wait_for_settings_writes_after_window();
    let exit = exit?;
    match exit {
        shell::ShellExit::Quit => Ok(()),
        shell::ShellExit::StartUpdate => run_launcher_requested_update(cli),
    }
}

/// Upper bound of the exit wait for queued background `user_config` writes. A write is one small
/// locked read-modify-write; the bound only stops a stuck disk from hanging the exit.
const SETTINGS_WRITES_EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Waits (bounded, [`SETTINGS_WRITES_EXIT_WAIT`]) for the settings writes the screens handed to
/// background threads (`ms_settings_ui::pending_config_writes`: General pane, launcher last
/// selection) once the main window has CLOSED, on every return of `shell::run`. Blocking here is
/// the documented exception to the GUI-thread rule: the window and its event loop are gone, no
/// frame is left to keep responsive, and returning first would let the process exit with those
/// writes unwritten. A timeout is logged by the wait itself.
fn wait_for_settings_writes_after_window() {
    // The result only says whether all landed; a timeout is already logged and the exit goes on.
    ms_settings_ui::pending_config_writes::wait_for_pending_config_writes(SETTINGS_WRITES_EXIT_WAIT);
}

/// Opens the session log (rotating `last.log` -> `previous.log`) and logs where its files are.
/// Called ONCE per process, by `run_native` before the mode split (every mode except the
/// elevated registration helper), so no later window re-opens or re-announces it. A failure is
/// printed to stderr and startup continues without a file log.
fn init_startup_logging_best_effort() {
    let log_dir = config::data_dir();
    if let Err(err) = runtime_log::init_session_logs(&log_dir) {
        eprintln!("[runtime-log] failed to initialize logging: {err}");
        return;
    }
    runtime_log::log_info(format!(
        "session log files: last='{}', previous='{}'",
        log_dir.join("last.log").display(),
        log_dir.join("previous.log").display()
    ));
}
