/*
FILE OVERVIEW: src/lib.rs
Library root of the `manhwastudio_rs` package: the module tree (`app`, `args`, `tabs`, …), the
`crate::x` re-export shims onto the `ms-*` crates, and the whole native startup flow. The
`manhwastudio_rs` binary (`src/main.rs`) is a thin shell that calls `native_main()` (desktop) or
`web_main()` (wasm32); these two functions are the library's only public entry points. The split
exists so a platform build that needs a library (the Android `cdylib`, built with
`cargo rustc --lib --crate-type cdylib`) can reach the same startup code: on Android the
unmangled `android_main` of `android_entry.rs` prepares the process and calls `native_main()`.
Every `eframe::NativeOptions` built here sets `android_app` from
`window_geometry::android_app::current()` on Android (compiled out elsewhere).

Main flow (`native_main` -> `run_main`):
- Parses CLI args and rejects contradictory flag combinations first.
- Handles hidden service flags (installer continuation, Windows uninstall, elevated Start Menu
  shortcut creation, the elevated system-registration helper) before normal startup.
- Seeds the process-global settings (storage mode first, locale, UI scale, autosave policy,
  secondary windows) and starts the Python AI backend supervisor where
  `PlatformFeature::PythonBackend` exists.
- `--check-venv` (terminal), `--continue-update` and `--update` run their own windows.
- `resolve_start`: a valid `--project` chapter, or `--image <PATH>` / a positional image path
  (OS "Open with", `.desktop` `%f`), opens the studio directly; an invalid one is reported in
  the console and a modal error box (`ms_platform::message::show_error_blocking`; log-only where
  no native box exists). Without an open target the pure `startup_routing` decides: on a platform
  without installed-copy management straight to the launcher; otherwise installer continuation,
  or — with no usable Python environment — the Windows existing-install check and the
  "install / update a custom folder / continue" prompt, each as its own window, ONCE per process.
- `shell::run` opens the ONE main window (`src/shell/`) on the launcher or the studio; the
  launcher and the studio are screens of it, switched in place. The startup GitHub release check
  runs once, on the first launcher entry (`update_check.rs`); a launcher "Update" request runs the
  update window after the main window has closed.
- `--test-launcher` is the main window in test mode (own title / app id; any exit quits).
- `--ignore-installed` (run from a source checkout): no Linux desktop entry, no existing-install
  discovery or prompt, no update check, per-root backend socket, self-update refused.
- Otherwise keeps the Linux desktop entry and icon current on a startup worker
  (`install_linux_desktop_integration_async`; text, ownership rule and writer live in
  `ms_os_integration::linux`); sweeps stale single-image scratch sessions on a worker.
- Auto-detects and persists `General.ai_install_type` when that key is absent.
*/

// Bring the `ms-i18n` UI-string macros (`t!` / `tf!` / `tp!`) into crate-wide
// scope so migrated call sites can use the bare macro names the extraction tool
// (`tools/i18n_extract.py`) emits and the `ms-i18n` key-validation test scans for.
// A single crate-root `#[macro_use]` avoids a per-file `use ms_i18n::{t, tf, tp};`
// across the hundreds of UI files the localization migration touches.
#[macro_use]
extern crate ms_i18n;

// Android process entry (`android_main`, exported from the `cdylib` the APK loads): logcat,
// TLS verifier, runtime root, then the same `native_main` startup as the desktop binary.
#[cfg(target_os = "android")]
mod android_entry;
mod app;
mod args;
mod i18n_resolve;
// App-shell half of the single-image mode: save controller, its dialogs and the tab / title /
// hotkey rules `MangaApp` consults (plan `dev-docs/single_image_mode_plan.md`, WP-2.4).
mod single_image;
// The main window: ONE `run_native` hosting the launcher and the studio as screens (launcher
// entry, studio load and studio teardown on workers). Its `style` part is shared with the web
// entry; everything else is native-only (gated inside).
mod shell;
mod tabs;
// The startup check for a newer release (GitHub), started once by the main window.
#[cfg(not(target_arch = "wasm32"))]
mod update_check;
#[cfg(target_arch = "wasm32")]
mod web_entry;

// The crate-shim map below: each `pub(crate) use` mounts an extracted `ms-*` crate (or one of
// its modules) under the binary-era `crate::…` name its call sites still use. The shims are
// crate-internal on purpose — the library's public API is only `native_main` / `web_main` —
// so an alias that no call site uses any more is reported as an unused import and removed;
// code needing such a crate names it directly (`ms_tools::…`).

// The settings surface both shells share — the menu-level `settings_shared` layer, the
// double-interface panes (`general_settings_panel`, plus the `ai_backend_supervisor` the
// panes drive) and the onboarding `tutorial` subsystem — now lives in the standalone
// `ms-settings-ui` crate. The launcher's settings page and the studio's Settings tab render
// the SAME panes, so neither could own them. The re-exports keep every
// `crate::settings_shared::…` / `crate::general_settings_panel::…` /
// `crate::ai_backend_supervisor::…` / `crate::tutorial::…` path valid without touching a
// call site.
pub(crate) use ms_settings_ui::{ai_backend_supervisor, general_settings_panel, settings_shared, storage_mode_job};

// The launcher shell — project catalogue, the detached "New Project" window with every
// download/stitch/reline/batch flow, PSD import, the import/export pages, the first-run
// language modal and the launcher settings page — now lives in the standalone
// `ms-launcher` crate. It is a screen of the main window: the studio never names it, only the
// main window (`shell/`) and `web_entry.rs` host it. The re-export keeps every `crate::launcher::…`
// path valid without touching a call site.
pub(crate) use ms_launcher as launcher;
// Gated exactly as the module was. The root `tutorial` feature FORWARDS to
// `ms-settings-ui/tutorial` (see `Cargo.toml`): features are not inherited, so without the
// forward this re-export would not exist while the binary's own `mark` sites compiled in.
// The demo bin `src/bin/tutorial_test` mounts `engine.rs` directly via `#[path]`, so it
// stays buildable regardless of this feature.
#[cfg(feature = "tutorial")]
pub(crate) use ms_settings_ui::tutorial;
// Startup monitor + window geometry contract (`Window` section of `user_config.json`) now
// lives in the standalone `ms-window-geometry` crate — it is also the only direct user of
// `winit`, which left the binary's manifest with it. The re-export keeps every
// `crate::window_geometry::…` path valid without touching call sites. Native-only: it talks
// to winit monitors and OS windows, neither of which exists on web.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use ms_window_geometry as window_geometry;

// `runtime_log` and `trace` now live in the standalone `ms-log` crate. These
// re-exports keep the existing `crate::runtime_log::…` / `crate::trace::…` module
// paths and the `crate::trace_log!` macro path valid across the whole crate without
// touching call sites.
pub(crate) use ms_log::{runtime_log, trace, trace_log};

// `backend_ipc` now lives in the standalone `ms-backend-ipc` crate. The re-export keeps
// every existing `crate::backend_ipc::…` path valid across the binary without touching
// call sites, exactly like the `ms-log` shim above.
pub(crate) use ms_backend_ipc as backend_ipc;

// `ai_install_probe` and `python_manager` now live in the standalone `ms-sysprobe` crate.
// The re-exports keep `crate::ai_install_probe::…` and `crate::python_manager::…` valid
// without touching call sites.
pub(crate) use ms_sysprobe::{ai_install_probe, python_manager};

// `hf_token` (the process-wide Hugging Face access token backed by the OS secret store,
// seeded at startup by `seed_hf_token_from_secret_store`) moved into the same crate. The
// re-export keeps `crate::hf_token::…` valid unchanged.
pub(crate) use ms_sysprobe::hf_token;

// `memory_manager` now lives in the standalone `ms-memory` crate. It sits BELOW `config`
// (which reads `MemoryProfile` from it), so it had to leave the binary before `config` can.
// The re-export keeps every `crate::memory_manager::…` path valid without touching call sites.
pub(crate) use ms_memory as memory_manager;

// The process-wide storage backend selection now lives in the `ms-storage` crate
// (`ms_storage::global`). The re-export keeps every `crate::storage::…` path valid
// across the binary without touching call sites. It sits BELOW `config` (which reads
// and writes `user_config.json` through it), so it had to leave the binary first.
pub(crate) use ms_storage::global as storage;

// `config` — the project's global config / runtime-path hub — now lives in the
// standalone `ms-config` crate, together with the crate-root module its default trees
// name for `General.enabled_tabs`: `app_tab`. The two re-exports keep every existing
// `crate::config::…` and `crate::app_tab::…` path valid without touching call sites, and
// with them the `tabs::AppTab` re-export layered on top.
pub(crate) use ms_config as config;
pub(crate) use ms_config::app_tab;

// `locale_store` (the editable on-disk `locale/` catalog, whose active tag comes out of
// `General.ui_language`) moved down into the same crate: it sits directly on the
// `user_config.json` border that crate owns. The re-export keeps `crate::locale_store::…`
// valid without touching call sites. It keeps its native-only gate: on wasm there is no
// folder next to an executable, and `web_entry.rs` installs the embedded catalog directly.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use ms_config::locale_store;

// The desktop installer/updater subsystem now lives in the standalone `ms-installer`
// crate. The re-export keeps every `crate::installer::…` path valid across the binary and
// the launcher without touching call sites. Native-only, exactly as the module was: it
// installs the managed Python environment and replaces the shipped executable, and its
// modules are compiled out on wasm inside the crate itself.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use ms_installer as installer;
// The `--check-venv` readiness check moved into the same crate: it reads the installer's
// own package requirements and shares two predicates with its repair worker, so a copy
// outside would be free to drift. The re-export keeps `crate::venv_check::…` valid.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use ms_installer::venv_check;

/// The application's own version pair, handed to `ms-installer` at its entry points.
///
/// It cannot be read inside that crate: `env!("CARGO_PKG_VERSION")` compiled there would
/// yield the LIBRARY's version, and `MS_APP_VERSION` is a `rustc-env` that `build.rs`
/// emits for this crate only. The two halves are not interchangeable — `core` is the
/// machine-facing value every release comparison reduces with `version_format::version_core`,
/// `display` is the git-derived string shown to the user and never parsed.
#[cfg(not(target_arch = "wasm32"))]
const HOST_VERSION: ms_installer::HostVersion = ms_installer::HostVersion {
    core: env!("CARGO_PKG_VERSION"),
    display: env!("MS_APP_VERSION"),
};

/// The process facts the launcher needs from startup: `--ignore-installed` (the System
/// registration tab is read-only and its warnings silent) and this build's version core (the
/// installer's reduction, `install.rs`), which a written Uninstall entry carries.
#[cfg(not(target_arch = "wasm32"))]
fn launcher_host(ignore_installed: bool) -> launcher::state::LauncherHost {
    launcher::state::LauncherHost { ignore_installed, version_core: version_format::version_core(HOST_VERSION.core) }
}

// Pure version-string composition/stripping, shared verbatim with `build.rs` through
// `include!("crates/ms-config/src/version_format.rs")`. It moved into `ms-config` so the
// installer and launcher crates can reach it without going through the binary. Defines
// the human/machine split: `MS_APP_VERSION` (composed at build time) is for display,
// `CARGO_PKG_VERSION` is for every comparison and cross-process parse.
pub(crate) use ms_config::version_format;

// The reusable UI-primitive layer — `crates/ms-widgets/src/` plus two neighbouring leaves of the
// same layer: `ui_fonts` (the single owner of the bundled `fonts/ui` stack) and
// `bubble_status` (the egui half of the bubble-status feature: it paints a rule's border and
// re-exports the GUI-free model from `ms-config`) — now lives in the standalone `ms-widgets`
// crate. The re-exports keep every `crate::widgets::…`, `crate::ui_fonts::…` and
// `crate::bubble_status::…` path valid without touching call sites. The crate knows nothing
// about the project domain: `canvas` and the tabs depend on IT, never the other way round.
pub(crate) use ms_widgets as widgets;
pub(crate) use ms_widgets::{bubble_status, ui_fonts};
// The configurable-hotkey registry moved into `ms-widgets` (it is an egui-input
// primitive over `AppTab`, and the `translation` tab crate registers specs with it).
// This re-export keeps every `crate::input_manager_v2::…` path valid in `app.rs` and the
// settings hotkeys pane.
pub(crate) use ms_widgets::input_manager_v2;

// The shared runtime models — the bubble model, the layer document, the clean-overlay
// model and the text-mask model — now live in the standalone `ms-models` crate, together
// with `page_view` (the source-page view model, a crate-root module of the same layer).
// The two re-exports keep every `crate::models::…` and `crate::page_view::…` path valid
// without touching call sites. The crate sits ABOVE `ms-project` (it loads and saves the
// chapter domain model) and BELOW the canvas and the tabs.
pub(crate) use ms_models as models;
pub(crate) use ms_models::page_view;

// The canvas facade — page rendering, bubble editing and clean-overlay painting — now lives
// in the standalone `ms-canvas` crate. The re-export keeps every `crate::canvas::…` path
// valid without touching call sites. It DECLARES the `CanvasHooks` trait that the tabs
// IMPLEMENT, so the dependency points down only and no cycle appears.
pub(crate) use ms_canvas as canvas;

// `page_ops` — the journalled structural page transaction — now lives in the standalone
// `ms-page-ops` crate, and `project` — the chapter domain model — in `ms-project`. The two
// re-exports keep every `crate::page_ops::…` and `crate::project::…` path valid across the
// binary without touching call sites.
//
// `Page` and `ProjectPaths` were DECLARED in `project.rs` and are now declared in
// `ms-page-ops` and re-exported by `ms-project`, so `crate::project::{Page, ProjectPaths}`
// still resolves. They had to move: `ProjectData::load` resolves a pending page-op journal
// before any reconcile pass runs (a contract that cannot be hoisted into callers without
// turning a crash-safety invariant into a convention), so `ms-project` depends on
// `ms-page-ops` and not the other way round.
pub(crate) use ms_page_ops as page_ops;
pub(crate) use ms_project as project;
// The projects-root catalogue scan (`list_titles`/`list_chapters`/
// `validate_project_dir_for_startup`) moved into `ms-project` as well: it is the same
// domain one level up from the chapter load, and `ms_config`'s path constants were its
// only dependency. The re-export keeps every `crate::project_scan::…` path valid.
pub(crate) use ms_project::project_scan;

// `text_punctuation` now lives in the config-free `ms-text-util` crate. Re-export
// keeps `crate::text_punctuation::…` valid across the binary. The crate no longer
// reads user config itself; `seed_hanging_punctuation_from_config` seeds it at startup.
pub(crate) use ms_text_util::text_punctuation;

// Typesetting-language model + process-global selection live in `ms-text-util`.
// The crate is config-free; `seed_text_language_from_config` seeds the user's
// choice at startup, mirroring `seed_hanging_punctuation_from_config`.
pub(crate) use ms_text_util::language;

// Native-only startup imports. All of these feed the native launcher/installer/
// update-check flow (`eframe::run_native`, `ureq`, `clap` CLI, native
// windows) which does not exist on `wasm32`. They are gated so the wasm build
// does not fail on missing deps or unused imports. `std::path::{Path, PathBuf}`
// stays unconditional because path types appear in items that both targets compile;
// the shared filesystem helpers that used to keep `std::ffi::OsStr` unconditional
// here now live in `project_scan.rs`.
#[cfg(not(target_arch = "wasm32"))]
use args::Cli;
#[cfg(not(target_arch = "wasm32"))]
use clap::Parser;
#[cfg(not(target_arch = "wasm32"))]
use installer::install as launcher_install;
#[cfg(not(target_arch = "wasm32"))]
use installer::update::UpdateWindowOutcome;
#[cfg(not(target_arch = "wasm32"))]
use installer::utils::ExternalUpdateTarget;
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use ms_platform::PlatformFeature;
#[cfg(not(target_arch = "wasm32"))]
use ms_platform::file_dialog::{self, DialogKind, FileDialogRequest, PendingDialog};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Mutex};
#[cfg(not(target_arch = "wasm32"))]
use ms_thread as thread;

/// Web entry, called by the wasm32 `fn main` of the `manhwastudio_rs` binary: boots the eframe
/// `WebRunner` on the page canvas. Real startup lives in `web_entry.rs`; the native
/// launcher/installer flow below is compiled out.
#[cfg(target_arch = "wasm32")]
pub fn web_main() {
    web_entry::start();
}

/// Native process entry, called by the desktop `fn main` of the `manhwastudio_rs` binary: runs
/// the whole startup flow (CLI parsing, service flags, launcher / studio windows) and logs how
/// the process ended. Service flags may terminate the process directly with their own exit code
/// (`std::process::exit`) instead of returning.
///
/// # Errors
/// Returns the fatal startup/runtime error of `run_main` (already logged here), which the
/// binary's `main` turns into a non-zero exit code.
#[cfg(not(target_arch = "wasm32"))]
pub fn native_main() -> anyhow::Result<()> {
    let result = run_main();
    match &result {
        Ok(()) => runtime_log::log_info("manhwastudio_rs exited normally"),
        Err(err) => runtime_log::log_error(format!("fatal startup/runtime error: {err:#}")),
    }
    result
}

/// The AI backend's deferred-autostart gate: open once the process-wide storage-mode job
/// slot holds nothing pending or running and no chapter conversion is registered. Polled on
/// the supervisor worker (a brief mutex read, never on the GUI thread).
#[cfg(not(target_arch = "wasm32"))]
fn storage_conversion_autostart_gate() -> ai_backend_supervisor::AutostartGate {
    Box::new(|| !storage_mode_job::backend_autostart_blocked())
}

#[cfg(not(target_arch = "wasm32"))]
fn run_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The elevated system-registration helper never opens the session log: initializing it
    // rotates `last.log` -> `previous.log`, and the helper runs while the launcher that
    // started it still writes `last.log` (on Windows it also holds the file open). The helper
    // reports every outcome in its result file and the launcher logs them, so it runs without
    // a file log (`runtime_log` lines are dropped before init). The CLI is therefore parsed
    // before the log init; a clap usage error exits there without touching the logs.
    if cli.system_registration_apply.is_none() {
        init_startup_logging_best_effort();
    }
    // FIRST action after parsing: several service flags below (Start Menu shortcut,
    // uninstall, update continuation) act on an installed copy immediately, so an
    // impossible combination must be rejected before any of them can run.
    reject_conflicting_startup_flags(&cli);
    // The elevated system-registration helper (a short-lived admin process the launcher starts
    // through UAC) does its batch and exits here, BEFORE every other startup side effect (scratch
    // sweep, desktop entry, backend socket): an elevated process must do nothing else.
    if cli.system_registration_apply.is_some() {
        std::process::exit(run_system_registration_helper(&cli));
    }
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
        "tracing enabled, args: project={:?} image={:?} no_ai={} update={} test_launcher={} trace={}",
        cli.project,
        cli.image_arg(),
        cli.no_ai,
        cli.update,
        cli.test_launcher,
        cli.trace
    );
    #[cfg(target_os = "windows")]
    if let Some(install_dir) = cli.create_start_menu_shortcut_install_dir.as_deref() {
        launcher_install::run_windows_create_start_menu_shortcut_for_install(
            install_dir,
            cli.continue_create_start_menu_shortcut,
        )
        .map_err(anyhow::Error::msg)?;
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    if cli.uninstall {
        launcher_install::run_windows_uninstall_from_current_exe(
            cli.continue_uninstall,
            cli.uninstall_signal_file.as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        return Ok(());
    }

    runtime_log::log_info("starting main application flow");
    // The FIRST document access of the process: reads the Dev/Prod storage mode out of the
    // user config (whichever format it is stored in) and seeds `ms_docstore`'s default
    // format with it. The store's "both files exist" rule resolves by that default, so it
    // must be set before any other read or write, including the first-run marker below.
    let storage_probe = config::storage_mode::init_storage_mode_at_startup();
    // MUST be the first startup call: every following call that touches
    // `load_user_config()` (AI install-type detection, language/effect seeding) persists
    // the full defaults tree, which materializes `General.ui_language` /
    // `TextTab.text_language` and erases the "language keys absent" signal this fresh-
    // install detection depends on. Run it before any of them so the marker is recorded.
    config::mark_first_run_languages_if_needed();
    auto_detect_missing_ai_install_type_for_startup();
    seed_hanging_punctuation_from_config();
    seed_text_language_from_config();
    seed_rotation_ctrl_wheel_from_config();
    seed_advanced_form_search_from_config();
    // Reads the Hugging Face token out of the OS secret store on a worker thread, so the
    // token badge and every gated download start from the value the user already saved.
    // Fire-and-forget: until it lands the cache honestly reports "not known".
    hf_token::seed_hf_token_from_secret_store();
    // Seed the typing tab's per-effect-kind default overrides so newly added effect
    // cards pick up the user's stored defaults from the first frame.
    tabs::typing::seed_effect_defaults_from_config();
    // Seed the typing tab's per-font settings (imported system fonts + display-name
    // overrides) from `fonts/fonts_data.json`. On first run it migrates the legacy
    // `user_config.json` imported-fonts list into that file once.
    tabs::typing::seed_imported_system_fonts_from_config();
    // Unpack/reconcile the editable on-disk locale catalog before settings load.
    // Best-effort and pre-window: an unwritable `locale/` folder degrades to the
    // embedded catalog without failing startup (see `locale_store` failure policy).
    locale_store::reconcile_disk_catalog();
    let user_settings = config::load_user_settings_for_startup()?;
    // The user config (converted last by the driver) is still in the other format: an
    // upgrade from the JSON-only era or an unfinished switch. Converting may take a while
    // (every title), so it runs on a worker started when the first window is up: before
    // the launcher (see the loop below) or after the studio's project load.
    if storage_probe.reconciliation_pending {
        storage_mode_job::set_pending_reconciliation(storage_mode_job::ConversionRequest::for_current_install(storage_probe.mode, &user_settings));
    }
    // Install the active UI locale from disk (or the embedded catalog on failure),
    // now that `General.ui_language` is available in the loaded settings.
    locale_store::install_ui_locale(&user_settings);
    // Seed the process-global interface scale before any window is created. Windows read
    // the global (not this snapshot), so a scale changed in the launcher is honored by the
    // studio opened afterwards and vice versa.
    general_settings_panel::seed_ui_scale_from_user_settings(&user_settings);
    // Seed the process-global autosave policy (interval + action threshold) the save workers
    // re-read on every wait; the General settings pane changes it live afterwards.
    ms_config::autosave_policy::seed_autosave_policy_from_user_settings(&user_settings);
    // Seed the process-global secondary-window setting (separate OS windows vs embedded);
    // the General settings pane changes it live afterwards.
    ms_config::secondary_windows::seed_from_user_settings(&user_settings);

    // Environment check mode terminates the process: it must never fall through into the
    // launcher, the studio, or the AI backend supervisor. `process::exit` is used instead
    // of returning, because callers (run-dev scripts) branch on an EXACT code and an
    // `anyhow` error return would collapse every outcome into the generic failure exit
    // plus a printed backtrace-style message.
    if cli.check_venv {
        init_runtime_logging();
        std::process::exit(run_check_venv_flow(&config::program_dir()));
    }

    if cli.continue_update {
        init_runtime_logging();
        if installer::update::run_continue_update_window(HOST_VERSION).map_err(anyhow::Error::msg)?
            == UpdateWindowOutcome::Exit
        {
            return Ok(());
        }
    }

    // `--update` (like every other installed-copy flag) can no longer arrive together with
    // `--ignore-installed`: `reject_conflicting_startup_flags` ended the process already.
    if cli.update {
        init_runtime_logging();
        if installer::update::run_update_window(cli.test_ver_check, HOST_VERSION).map_err(anyhow::Error::msg)?
            == UpdateWindowOutcome::Exit
        {
            return Ok(());
        }
    }

    if cli.test_launcher {
        init_runtime_logging();
        let supervisor = ai_backend_supervisor::AiBackendSupervisor::start_with_autostart_gate(ai_backend_enabled(&cli), Some(storage_conversion_autostart_gate()));
        // Test mode routes nowhere: the shell quits on any launcher exit.
        let exit = shell::run(shell::ShellConfig {
            initial: shell::InitialScreen::Launcher,
            user_settings,
            ai_backend: supervisor.handle(),
            host: launcher_host(cli.ignore_installed),
            test_mode: true,
            update_check: update_check_enabled(cli.ignore_installed, installed_copy_management_available()).then_some(cli.test_ver_check),
        })?;
        runtime_log::log_info(format!("[startup] --test-launcher window closed ({exit:?})"));
        return Ok(());
    }

    let ai_enabled = ai_backend_enabled(&cli);
    // The single Python AI backend is owned here, above the main window, so it starts with the
    // launcher (per the autostart toggle) and survives every switch between the launcher and the
    // studio. Dropped (which stops the process + probe) on any return.
    // The autostart waits (on the supervisor worker) until no storage-mode conversion is
    // pending or running: the backend writes `user_config` itself (KG-016). With `--project`
    // the reconciliation stays pending until the project is loaded, so the autostart does too.
    let ai_backend_supervisor = ai_backend_supervisor::AiBackendSupervisor::start_with_autostart_gate(ai_enabled, Some(storage_conversion_autostart_gate()));
    let ai_backend = ai_backend_supervisor.handle();

    // The startup prompts (installer continuation, missing environment, Windows existing
    // install) run ONCE here, as their own windows, before the main window exists.
    let Some(start) = resolve_start(&cli)? else {
        return Ok(());
    };
    // The ONE main window: launcher and studio are its screens, switched in place. The launcher
    // shows the storage reconciliation's progress; a direct `--project` or image start defers it
    // to the studio screen, after the load.
    let exit = shell::run(shell::ShellConfig {
        initial: start.initial,
        user_settings,
        ai_backend,
        host: launcher_host(cli.ignore_installed),
        test_mode: false,
        update_check: start.update_check.then_some(cli.test_ver_check),
    })?;
    match exit {
        shell::ShellExit::Quit => Ok(()),
        shell::ShellExit::StartUpdate => run_launcher_requested_update(&cli),
    }
}

/// Runs the update window the launcher's "Update" button asked for, after the main window has
/// closed. The button only exists with an update notification, which the startup check never
/// produces under `--ignore-installed` or without installed-copy management, so both checks below
/// are guards, not the UI path.
///
/// # Errors
/// The update window could not run.
#[cfg(not(target_arch = "wasm32"))]
fn run_launcher_requested_update(cli: &Cli) -> anyhow::Result<()> {
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

/// Starts the one-shot startup sweep of stale single-image scratch sessions
/// (`ms_project::single_image::sweep_stale_sessions` over `scratch_base()`) on a named worker
/// and logs its report. Never blocks startup; a spawn failure is logged and the next start
/// retries.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_stale_scratch_sweep() {
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

#[cfg(not(target_arch = "wasm32"))]
fn init_startup_logging_best_effort() {
    let log_dir = config::data_dir();
    if let Err(err) = runtime_log::init_session_logs(&log_dir) {
        eprintln!("[runtime-log] failed to initialize logging: {err}");
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn init_runtime_logging() {
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

/// Seeds the config-free `ms_text_util::text_punctuation` set from user config at
/// startup. The util crate defaults to `DEFAULT_HANGING_PUNCTUATION`; here the app
/// (which owns config) overrides it with `TextTab.hanging_punctuation` when present
/// and non-blank. Best-effort: on any config error the default set is kept.
#[cfg(not(target_arch = "wasm32"))]
fn seed_hanging_punctuation_from_config() {
    let Ok(cfg) = config::load_user_config() else {
        return;
    };
    if let Some(text) = cfg
        .get_path(&["TextTab", "hanging_punctuation"])
        .and_then(serde_json::Value::as_str)
        .filter(|text| text.chars().any(|ch| !ch.is_whitespace()))
    {
        text_punctuation::set_hanging_punctuation(text);
    }
}

/// Seeds the config-free `ms_text_util::language` typesetting language from user
/// config at startup. The util crate defaults to `TextLanguage::Ru` (preserving
/// the historical Russian-only behavior); here the app (which owns config)
/// overrides it with the `TextTab.text_language` tag when present and recognized.
/// An absent or unrecognized tag keeps `Ru` and logs a warning — never a panic.
#[cfg(not(target_arch = "wasm32"))]
fn seed_text_language_from_config() {
    let Ok(cfg) = config::load_user_config() else {
        return;
    };
    let Some(raw) = cfg
        .get_path(&["TextTab", config::TEXT_TAB_TEXT_LANGUAGE_KEY])
        .and_then(serde_json::Value::as_str)
    else {
        // Key absent (older config): keep the default, no warning needed.
        return;
    };
    match language::TextLanguage::from_tag(raw.trim()) {
        Some(text_language) => language::set_text_language(text_language),
        None => runtime_log::log_warn(format!(
            "[startup] unrecognized TextTab.text_language {raw:?}; keeping default 'ru'"
        )),
    }
}

/// Seeds the runtime `rotation_ctrl_wheel` global from user config at startup.
/// The module defaults to `DEFAULT_ROTATION_CTRL_WHEEL_MODE` (`Vector`); here the app
/// overrides it with `TextTab.rotation_ctrl_wheel_mode` when present and recognized.
/// Best-effort: on any config error or unknown value the default mode is kept.
#[cfg(not(target_arch = "wasm32"))]
fn seed_rotation_ctrl_wheel_from_config() {
    let Ok(cfg) = config::load_user_config() else {
        return;
    };
    use tabs::typing::rotation_ctrl_wheel;
    if let Some(mode) = cfg
        .get_path(&["TextTab", "rotation_ctrl_wheel_mode"])
        .and_then(serde_json::Value::as_str)
        .and_then(rotation_ctrl_wheel::RotationCtrlWheelMode::from_config_str)
    {
        rotation_ctrl_wheel::set_rotation_ctrl_wheel_mode(mode);
    }
}

/// Seeds the runtime-global advanced text-form search knobs from user config at
/// startup. The module defaults every knob; here the app overrides the fields
/// `TextTab.advanced_form_search` actually names.
///
/// Best-effort and PARTIAL-TOLERANT: a config error, a non-object value or a
/// missing/invalid field keeps that field's compiled-in default (the decoding rule
/// belongs to `AdvancedFormParams::from_config_value`, which also clamps the result
/// into the supported ranges).
#[cfg(not(target_arch = "wasm32"))]
fn seed_advanced_form_search_from_config() {
    let Ok(cfg) = config::load_user_config() else {
        return;
    };
    use tabs::typing::advanced_form_params;
    if let Some(stored) = cfg.get_path(&["TextTab", config::TEXT_TAB_ADVANCED_FORM_SEARCH_KEY]) {
        advanced_form_params::set_advanced_form_params(
            advanced_form_params::AdvancedFormParams::from_config_value(stored),
        );
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn auto_detect_missing_ai_install_type_for_startup() {
    let raw_settings = match config::load_raw_user_settings_for_startup() {
        Ok(settings) => settings,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[startup] failed to read user config before AI install-type detection: {err:#}"
            ));
            return;
        }
    };
    if config::user_settings_has_ai_install_type(&raw_settings) {
        return;
    }

    runtime_log::log_info("[startup] General.ai_install_type is missing; probing AI environment");
    let app_dir = config::program_dir();
    let report = match ai_install_probe::collect_ai_computations_report(&app_dir) {
        Ok(report) => report,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[startup] failed to auto-detect AI install type: {err}"
            ));
            return;
        }
    };
    let install_type = ai_install_probe::detect_ai_install_type_from_report(&report);
    let mut cfg = match config::load_user_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[startup] failed to load user config for AI install-type persistence: {err:#}"
            ));
            return;
        }
    };
    if let Err(err) = cfg.set_path(
        &["General", config::GENERAL_AI_INSTALL_TYPE_KEY],
        serde_json::Value::String(install_type.as_str().to_string()),
    ) {
        runtime_log::log_warn(format!(
            "[startup] failed to persist auto-detected AI install type: {err:#}"
        ));
        return;
    }
    runtime_log::log_info(format!(
        "[startup] auto-detected General.ai_install_type={}",
        install_type.as_str()
    ));
}

/// Where the main window starts and whether it runs the startup update check.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct StartPlan {
    initial: shell::InitialScreen,
    /// Run the startup update check (once, on the first launcher entry).
    update_check: bool,
}

/// Resolves where the main window starts: the CLI open target (`--project`, `--image` or the
/// positional image) when present, else whatever the no-argument startup path (installer
/// prompts) yields — normally the launcher. `None` means "exit the program". The prompts run
/// once per process: a later return to the launcher does not repeat them.
///
/// # Errors
/// An invalid CLI open target (after its error dialog) and every error of the no-argument path.
#[cfg(not(target_arch = "wasm32"))]
fn resolve_start(cli: &Cli) -> anyhow::Result<Option<StartPlan>> {
    // A CLI start can still reach the launcher later ("Exit to launcher"); its update notice
    // follows the same rule as a launcher start.
    let cli_update_check = || update_check_enabled(cli.ignore_installed, installed_copy_management_available());
    if let Some(project_dir) = &cli.project {
        return Ok(resolve_cli_project_dir(project_dir)?.map(|target| StartPlan { initial: shell::InitialScreen::Studio(target), update_check: cli_update_check() }));
    }
    if let Some(image) = cli.image_arg() {
        let target = resolve_cli_image(image)?;
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
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy)]
struct StartupRoutingFlags {
    /// Installer continuation after elevation: go straight into the installer window.
    force_run_installer: bool,
    /// `--ignore-installed`: never discover, modify, or replace an installed copy.
    ignore_installed: bool,
}

/// Where the no-open-target startup path goes; decided by [`startup_routing`].
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
fn resolve_cli_project_dir(project_dir: &Path) -> anyhow::Result<Option<shell::studio::StartupTarget>> {
    let path = project_dir.to_path_buf();
    match crate::project_scan::validate_project_dir_for_startup(&path) {
        crate::project_scan::ProjectValidationState::Valid { .. } => Ok(Some(shell::studio::StartupTarget::Project(path))),
        crate::project_scan::ProjectValidationState::Invalid { message } => {
            let full_message = format!("--project path is invalid: {message}");
            show_startup_error_dialog(&full_message);
            anyhow::bail!("{full_message}")
        }
    }
}

/// Validates a command-line image path (`--image` or positional) before any window exists.
/// Only existence and kind are checked here; decodability is reported by the studio loading
/// screen. Mirrors `resolve_cli_project_dir`: an unusable path shows the startup error dialog
/// and ends startup.
///
/// # Errors
/// The path does not exist, is a directory (the message names `--project`), or is not a
/// regular file.
#[cfg(not(target_arch = "wasm32"))]
fn resolve_cli_image(image: &Path) -> anyhow::Result<shell::studio::StartupTarget> {
    use crate::project::single_image::SingleImageError;
    let path = image.to_path_buf();
    let message = match args::check_image_arg(&path) {
        Ok(()) => return Ok(shell::studio::StartupTarget::Image(path)),
        Err(args::ImageArgProblem::NotFound) => SingleImageError::NotFound { path: path.clone() }.user_message(),
        Err(args::ImageArgProblem::Directory) => tf!("startup.image.directory_error", path = path.display()),
        Err(args::ImageArgProblem::NotAFile) => SingleImageError::NotAFile { path: path.clone() }.user_message(),
    };
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
#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
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
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
fn refuse_self_update_from_sources() {
    runtime_log::log_warn(
        "[startup] self-update refused: running from sources (--ignore-installed)",
    );
    show_startup_error_dialog(t!("startup.ignore_installed.update_refused"));
}

/// Whether the Python AI backend may start: not under `--no-ai`, and only where the platform
/// can run it (`PlatformFeature::PythonBackend`). Both supervisor starts use it.
#[cfg(not(target_arch = "wasm32"))]
fn ai_backend_enabled(cli: &Cli) -> bool {
    !cli.no_ai && PlatformFeature::PythonBackend.is_available()
}

/// `PlatformFeature::InstalledCopyManagement` for the startup routing. When the platform lacks
/// it (no installer, update check, install prompts or update windows), the first call logs that
/// once per process; the routing loop asks again on every return to the launcher.
#[cfg(not(target_arch = "wasm32"))]
fn installed_copy_management_available() -> bool {
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
#[cfg(not(target_arch = "wasm32"))]
const fn update_check_enabled(ignore_installed: bool, installed_copy_management: bool) -> bool {
    installed_copy_management && !ignore_installed
}

/// Process exit code for an impossible command line (a usage error, mirroring clap's
/// own exit code for one). Distinct from [`CHECK_VENV_EXIT_NOT_READY`], which reports a
/// well-formed run whose environment is unusable.
#[cfg(not(target_arch = "wasm32"))]
const CLI_USAGE_ERROR_EXIT: i32 = 2;

/// Aborts startup when the command line combines flags that contradict each other:
/// `--ignore-installed` with a flag that manages an installed copy
/// (`args::conflicting_installed_copy_flags`), or an image to open with a terminal mode or an
/// installed-copy flag (`args::conflicting_image_flags`).
///
/// Returns normally when the combination is valid. Otherwise it logs, prints a
/// user-facing diagnostic per violated rule to stderr and exits with [`CLI_USAGE_ERROR_EXIT`]
/// — it must terminate rather than ignore a flag, because silently dropping, say,
/// `--uninstall` would be as surprising as honoring it.
#[cfg(not(target_arch = "wasm32"))]
fn reject_conflicting_startup_flags(cli: &Cli) {
    let installed_conflicts = args::conflicting_installed_copy_flags(cli);
    let image_conflicts = args::conflicting_image_flags(cli);
    if installed_conflicts.is_empty() && image_conflicts.is_empty() {
        return;
    }
    // The UI catalog is normally installed much later in startup (it needs the user
    // config), and `t!` would return raw keys here. This path exits immediately, so
    // bring the catalog up just for the diagnostic: both calls only READ, so the
    // first-run language marker and the user config stay untouched. The EMBEDDED
    // catalog is used on purpose — the on-disk `locale/` folder is only reconciled
    // later in startup, so a freshly shipped key may not be in it yet.
    let raw_settings =
        config::load_raw_user_settings_for_startup().unwrap_or(serde_json::Value::Null);
    locale_store::install_embedded_ui_locale(&raw_settings);
    let mut messages = Vec::with_capacity(2);
    if !installed_conflicts.is_empty() {
        messages.push(tf!(
            "startup.ignore_installed.conflicting_flags",
            flags = installed_conflicts.join(", ")
        ));
    }
    if !image_conflicts.is_empty() {
        messages.push(tf!("startup.image.conflicting_flags", flags = image_conflicts.join(", ")));
    }
    for message in &messages {
        runtime_log::log_error(format!("[startup] {message}"));
        eprintln!("{message}");
    }
    std::process::exit(CLI_USAGE_ERROR_EXIT);
}

/// Runs this process as the elevated system-registration helper
/// (`--system-registration-apply` + `--system-registration-result`, clap requires both) and
/// returns its exit code (`ms_os_integration::actions::HELPER_EXIT_*`). Windows only; elsewhere
/// the flags are rejected with a localized "unsupported" diagnostic and [`CLI_USAGE_ERROR_EXIT`].
#[cfg(not(target_arch = "wasm32"))]
fn run_system_registration_helper(cli: &Cli) -> i32 {
    // Same early locale bring-up as `reject_conflicting_startup_flags`: read-only, embedded
    // catalog, so the helper's failure texts (written into its result file) are localized.
    let raw_settings =
        config::load_raw_user_settings_for_startup().unwrap_or(serde_json::Value::Null);
    locale_store::install_embedded_ui_locale(&raw_settings);
    #[cfg(target_os = "windows")]
    {
        let (Some(actions), Some(result_file)) =
            (cli.system_registration_apply.as_deref(), cli.system_registration_result.as_deref())
        else {
            runtime_log::log_error("[startup] --system-registration-apply without --system-registration-result");
            return CLI_USAGE_ERROR_EXIT;
        };
        ms_os_integration::windows::elevation::run_elevated_helper(actions, result_file, version_format::version_core(HOST_VERSION.core))
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
#[cfg(not(target_arch = "wasm32"))]
const CHECK_VENV_EXIT_READY: i32 = 0;
/// Process exit code of `--check-venv` when the environment is NOT usable: it was
/// incomplete and the user cancelled the repair, a repair stage failed, or the check
/// itself could not verify readiness. Scripts branch on exactly this value.
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
fn run_check_venv_flow(root_dir: &Path) -> i32 {
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
#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
enum MissingPythonEnvAction {
    Install,
    UpdateCustom(ExternalUpdateTarget),
    LaunchLauncher,
}

#[cfg(not(target_arch = "wasm32"))]
fn prompt_missing_python_env_action() -> MissingPythonEnvAction {
    let output = Arc::new(Mutex::new(MissingPythonEnvAction::LaunchLauncher));
    let output_for_app = Arc::clone(&output);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([520.0, 250.0])
        .with_min_inner_size([480.0, 230.0])
        .with_resizable(false);
    if let Some(icon) = window_geometry::identity::app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        // winit cannot build an event loop on Android without the Activity handle.
        #[cfg(target_os = "android")]
        android_app: window_geometry::android_app::current(),
        ..Default::default()
    };

    if let Err(err) = eframe::run_native(
        "ManhwaStudio",
        options,
        Box::new(move |cc| {
            ms_theme::apply(&cc.egui_ctx);
            ui_fonts::install(&cc.egui_ctx, ui_fonts::Tier::Core);
            Ok(Box::new(MissingPythonEnvPromptApp {
                output: output_for_app,
                error_text: None,
                folder_pick: None,
            }))
        }),
    ) {
        runtime_log::log_warn(format!("missing-env prompt failed: {err}"));
        return MissingPythonEnvAction::LaunchLauncher;
    }

    output
        .lock()
        .map(|mut guard| std::mem::replace(&mut *guard, MissingPythonEnvAction::LaunchLauncher))
        .unwrap_or(MissingPythonEnvAction::LaunchLauncher)
}

#[cfg(not(target_arch = "wasm32"))]
struct MissingPythonEnvPromptApp {
    output: Arc<Mutex<MissingPythonEnvAction>>,
    error_text: Option<String>,
    /// The custom-update folder picker while it is open; every button is disabled meanwhile.
    folder_pick: Option<PendingDialog>,
}

#[cfg(not(target_arch = "wasm32"))]
impl MissingPythonEnvPromptApp {
    fn set_output_and_close(&self, action: MissingPythonEnvAction, ctx: &egui::Context) {
        if let Ok(mut output) = self.output.lock() {
            *output = action;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Opens the folder picker for «update a custom install folder» without blocking the window.
    fn start_custom_update_pick(&mut self, ctx: &egui::Context) {
        let request = FileDialogRequest::new(DialogKind::PickDirectoryPath).title(t!("startup.custom_update.folder_picker_title"));
        let repaint_ctx = ctx.clone();
        self.error_text = None;
        self.folder_pick = Some(file_dialog::request(request, Some(Arc::new(move || repaint_ctx.request_repaint()))));
    }

    /// Takes the folder picker's answer once it arrived: a cancel does nothing, an error is
    /// shown in the prompt (and logged), a folder is checked for an installed program copy.
    fn poll_custom_update_pick(&mut self, ctx: &egui::Context) {
        let Some(result) = self.folder_pick.as_mut().and_then(PendingDialog::poll) else {
            return;
        };
        self.folder_pick = None;
        let folder = match result {
            // `PickDirectoryPath` always answers `DirectoryPath` (the ms-platform kind contract).
            Ok(picked) => match picked.and_then(file_dialog::Picked::into_directory_path) {
                Some(folder) => folder,
                None => return,
            },
            Err(err) => {
                runtime_log::log_warn(format!("[startup] custom update folder picker failed: {err:?}"));
                self.error_text = Some(err.user_message());
                return;
            }
        };
        match launcher_install::resolve_installed_program_copy_path(&folder) {
            Ok(executable_path) => {
                self.set_output_and_close(
                    MissingPythonEnvAction::UpdateCustom(ExternalUpdateTarget {
                        root_dir: folder,
                        executable_path,
                    }),
                    ctx,
                );
            }
            Err(err) => {
                self.error_text = Some(err);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl eframe::App for MissingPythonEnvPromptApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35 hands `App::ui` the window-root `Ui`; keep a borrowed `Context` handle for
        // the calls (viewport commands, helper methods) that still operate on the context.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.poll_custom_update_pick(ctx);
        let picking = self.folder_pick.is_some();
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading(t!("startup.install_prompt.title"));
                ui.add_space(8.0);
                ui.label(
                    t!("startup.install_prompt.message"),
                );
                if let Some(error_text) = &self.error_text {
                    ui.add_space(8.0);
                    ui.colored_label(ms_theme::status::ERROR, error_text);
                }
                ui.add_space(14.0);
                // While the folder picker is open the window keeps repainting but takes no
                // other choice: its answer may still close the window.
                ui.add_enabled_ui(!picking, |ui| {
                    if ui
                        .add_sized([360.0, 34.0], egui::Button::new(t!("startup.install_prompt.install_button")))
                        .clicked()
                    {
                        self.set_output_and_close(MissingPythonEnvAction::Install, ctx);
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_sized(
                            [360.0, 34.0],
                            egui::Button::new(t!("startup.install_prompt.update_custom_button")),
                        )
                        .clicked()
                    {
                        self.start_custom_update_pick(ctx);
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_sized([360.0, 34.0], egui::Button::new(t!("startup.install_prompt.open_basic_launcher_button")))
                        .clicked()
                    {
                        self.set_output_and_close(MissingPythonEnvAction::LaunchLauncher, ctx);
                    }
                });
            });
        });
    }
}

/// Runs the installer window for `app_dir` (installer continuation, or the user's "install"
/// choice); the program exits afterwards in every case. A finished install that offers to launch
/// the installed copy starts it as a separate process.
///
/// # Errors
/// The installer window failed, the installer reported a failure, or the installed copy could not
/// be started.
#[cfg(not(target_arch = "wasm32"))]
fn run_startup_installer(app_dir: PathBuf, auto_install_target: Option<PathBuf>) -> anyhow::Result<()> {
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
#[cfg(not(target_arch = "wasm32"))]
fn show_startup_error_dialog(message: &str) {
    runtime_log::log_error(format!("startup error dialog shown: {message}"));
    if let Err(unavailable) = ms_platform::message::show_error_blocking(t!("startup.error_dialog.title"), message) {
        runtime_log::log_error(format!(
            "[startup] the startup error could not be shown in a dialog; it is reported only in this log.\nReason: {unavailable:?}\nMessage: {message}"
        ));
    }
}

/// Keeps this copy's Linux desktop entry and icon current on a named background thread
/// (`ms_os_integration::linux::xdg::ensure_at_startup`: create if missing, refresh only if
/// ours and different, never take over another copy's entry). Best effort: every outcome and
/// failure is logged and nothing else happens.
#[cfg(target_os = "linux")]
fn install_linux_desktop_integration_async() {
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

#[cfg(all(test, not(target_arch = "wasm32")))]
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
