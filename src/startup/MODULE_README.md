# Module: src/startup

## Purpose
Native process startup of the root package: command-line validation, hidden service flags,
process-global seeding, the pre-window prompt windows (installer, missing environment, update)
and the hand-over to the ONE main window (`shell::run`). The whole module is native-only:
`lib.rs` declares it behind a single `cfg(not(target_arch = "wasm32"))`; the web build starts
in `web_entry.rs` instead.

## Architecture
```text
main.rs / android_entry.rs -> lib.rs::native_main (logs how the process ended)
  -> startup::run_native (mod.rs): parse -> mode::decide (mode.rs) -> match on RunMode
       -> service_flags.rs   usage-error report, registration helper, --check-venv (exit codes)
       -> os_integration.rs  scratch sweep + Linux desktop entry (fire-and-forget workers)
       -> services.rs        init_full: the startup-order contract -> ProcessServices
            -> seed.rs       config-driven process-global seeds
       -> routing.rs         resolve_start (CLI target | delivered images | prompts | launcher)
            -> prompts.rs    missing-environment prompt, installer window, startup error box
       -> shell::run (the main window until it closes) -> routing::run_launcher_requested_update
```
`run_native` is the only entry point; every other item is `pub(super)` or private. The module
holds no state between calls: process-global services it starts (AI backend supervisor) or seeds
(UI scale, autosave policy, …) are owned by their crates.

## Files and submodules
- `mode.rs`: the ONE run-mode decision, `decide(&Cli, PlatformFacts) -> RunMode` (pure,
  table-tested; see "Run modes" below).
- `mod.rs`: `run_native` (parse -> decide -> match), `init_minimal` (the process-wide side
  effects every non-helper mode needs), `run_main` (update preludes, test launcher, main
  window), `HOST_VERSION` (the `ms_installer::HostVersion` pair
  this package hands to the installer and IPC crates), `launcher_host`, the ONE session-log
  init (`init_startup_logging_best_effort`, called once before the mode split; no later window
  re-opens the log).
- `services.rs`: `init_full`, the ONE owner of the startup-order contract (its doc comment lists
  rules (a)-(f)), returning `ProcessServices` (user-settings snapshot, `LocalAiAvailability`,
  `LauncherHost`); `ProcessServices::start_ai_backend_supervisor`, the one construction of the
  AI backend supervisor with the storage-conversion autostart gate. The config-driven steps are
  data (`PRE_SETTINGS_STEPS`, `SETTINGS_STEPS`) that `init_full` iterates.
- `routing.rs`: where the main window starts (`StartPlan`, `resolve_start`, CLI project / image
  validation), the pure unit-tested no-target route (`startup_routing` over
  `StartupRoutingFlags` -> `StartupRoute`), the Windows existing-install route and standalone
  relaunch, the update-check / installed-copy-management gates, the launcher-requested update.
  A CLI image set (`--image`, positional paths) skips the launcher like `--project`; only when NO
  path is a file (missing, a directory — the message names `--project` — or a non-file) does the
  startup error box list each problem and end startup; otherwise every path goes to the open
  worker, which lists the unusable ones as skipped.
- `service_flags.rs`: `exit_with_usage_error`, the elevated system-registration
  helper, the refused self-update, `--check-venv` and its repair window, the exit-code
  constants, and `install_early_diagnostic_locale` (storage probe, then the embedded catalog)
  shared by the three early paths.
- `prompts.rs`: the missing-environment prompt (the window lives in
  `ms_installer::prompt_window`; this file only maps a window that could not run to
  `LaunchLauncher`), the startup installer window, the startup error box
  (`ms_platform::message::show_error_blocking`, log-only where absent).
- `seed.rs`: seeds of `ms-text-util` / typing-tab globals from `user_config`, and the
  `General.ai_install_type` auto-detection.
- `os_integration.rs`: the stale single-image scratch sweep (once per start, lock-guarded, so a
  live instance's session is never touched) and the Linux desktop entry worker (runs
  `ms_os_integration`'s `ensure_at_startup` for `CopyIdentity::current` and logs the outcome; the
  entry's text, ownership rule and writer live in `crates/ms-os-integration/src/linux/`).

## Run modes
`mode::decide` picks exactly one mode right after parsing; first match wins. Columns: the
process-wide side effects that run before the mode's own action (`log` = session-log init,
`min` = `init_minimal`, `full` = `init_full`, `probe` = only the storage-mode probe of
`install_early_diagnostic_locale`; `full` starts with the same probe).

| Precedence | `RunMode` | Trigger | Runs before it | Ends in |
|---|---|---|---|---|
| 1 | `Service(UsageError)` | `args::conflicting_installed_copy_flags` / `conflicting_image_flags` / `conflicting_project_flags` non-empty, or `args::windows_only_flags` off Windows | log (unless `--system-registration-apply`), probe | exit 2 |
| 2 | `Service(RegistrationHelper)` | `--system-registration-apply` (+ result), every OS | probe | exit with the helper's code (off Windows: 2) |
| 3 | `Service(WindowsStartMenuShortcut)` | `--create-start-menu-shortcut-install-dir` (Windows; rejected at 1 elsewhere) | log, min | return |
| 4 | `Service(WindowsUninstall)` | `--uninstall` (Windows; rejected at 1 elsewhere) | log, min | return |
| 5 | `Service(CheckVenv)` | `--check-venv` | log, min, full | exit 0 / 1 |
| 6 | `Service(SelfUpdateRefused)` | `--update` / `--continue-update` without `InstalledCopyManagement` | log, probe | logged refusal + one localized stderr line, return (exit 0) |
| 7 | `Main` preludes | `--continue-update`, then `--update` (both run, in this order) | log, min, full | window `Exit` -> return, else falls through |
| 8 | `Main(TestLauncher)` | `--test-launcher` | log, min, full, preludes | return |
| 9 | `Main(Main)` | default (startup prompts, launcher, `--project`, images) | log, min, full, preludes | return / launcher-requested update |

No flag is dropped silently: a Windows-only service flag on another OS, and `--project` (like an
image) with `--check-venv` / `--test-launcher`, are usage errors; a self-update without
`InstalledCopyManagement` is refused exactly like a launcher-requested one
(`routing::run_launcher_requested_update`: logged, no window; the CLI refusal also prints one
localized stderr line). Each rule has named rows in
`mode.rs`'s table test. On Windows a lone continuation flag (`--continue-uninstall` without
`--uninstall`) still falls through to the main run.

## Contracts and invariants
Startup order in `run_native` (each step's position is load-bearing; the match arms run only
the prefix their mode needs, see "Run modes"):
1. CLI parse; session-log init (skipped for the system-registration helper: the init rotates
   `last.log` while the launcher that started the helper still writes it; the helper's outcomes
   reach the log through the launcher, which logs every result-file entry).
2. Usage error (`decide`'s first rule, reported by `exit_with_usage_error`): before any other
   action, because several service flags act on an installed copy immediately.
   `--ignore-installed` with a flag that manages an installed copy (`args::INSTALLED_COPY_FLAGS` — visible `--update` and the hidden
   install/update/uninstall/shortcut service flags), an image to open with `--check-venv`,
   `--test-launcher` or any installed-copy flag, `--project` with `--check-venv` /
   `--test-launcher`, or a Windows-only service flag on another OS exits with code 2
   (`CLI_USAGE_ERROR_EXIT`). The decisions are the pure, unit-tested
   `args::conflicting_installed_copy_flags` / `conflicting_image_flags` /
   `conflicting_project_flags` / `windows_only_flags`.
3. `--system-registration-apply` + `--system-registration-result` (hidden pair, also
   installed-copy flags): the elevated helper
   (`ms_os_integration::windows::elevation::run_elevated_helper`) runs and exits with its code
   before any other side effect; off Windows the pair exits 2 with an "unsupported" diagnostic.
4. Stale scratch sweep (worker); Linux desktop entry (worker, not under `--ignore-installed`);
   IPC studio version; isolated backend socket name under `--ignore-installed`; trace init.
   Step 4 is `init_minimal`.
5. Windows service flags: Start Menu shortcut, `--uninstall` (and its elevated continuation,
   which keeps the normal log init: its parent is not writing the same `last.log` in the common
   case). A refused self-update (no `InstalledCopyManagement`) only logs, prints one line and
   returns, before step 4.
6. Storage-mode probe (`init_storage_mode_at_startup`): the FIRST document access; seeds the
   docstore default format. The terminal paths of steps 2, 3 and the refused self-update read
   `user_config` for their localized diagnostics, so they run the same probe first
   (`service_flags::install_early_diagnostic_locale`); they never reach `init_full`, so the
   probe runs exactly once per process. Steps 1, 4 and 5 touch no owned document.
7. `mark_first_run_languages_if_needed` (before anything that persists the defaults tree), then
   the config seeds (`seed.rs`, HF token, typing-tab seeds), the on-disk locale reconcile
   (BEFORE `load_user_settings_for_startup`), settings load, pending storage reconciliation, UI
   locale / UI scale / autosave policy / secondary windows / touch gestures seeding. Steps 6-7 are
   `services::init_full`; its doc comment is the authoritative statement of rules (a)-(f).
8. `--check-venv` (terminal) -> `--continue-update` -> `--update` -> `--test-launcher` (the main
   window in test mode).
9. AI backend supervisor (`ProcessServices::start_ai_backend_supervisor`: the process's
   `LocalAiAvailability`; autostart waits for storage conversions) -> cold-start delivered images (`shell::incoming::cold_start_target`) ->
   `resolve_start` -> `shell::run` -> bounded wait for queued background `user_config` writes
   (`ms_settings_ui::pending_config_writes`; no window is open any more) -> the
   launcher-requested update window, if any.

Precondition outside this module: on Android the working directory is the runtime root before
the first `ms_config` call of the process (`android_entry.rs::start` -> `enter_runtime_root`,
before `native_main`); `run_native`'s session-log init is that first call.

**What pins the order.** `services.rs`'s unit tests pin `PRE_SETTINGS_STEPS` /
`SETTINGS_STEPS` (first-run marker first, locale reconcile last before the settings load, the
AI install-type probe and the secondary-windows seed present, every step at its decided
position; a variant left out of a table is dead code and fails clippy). They do NOT pin: the
storage probe being the first statement of `init_full` and the settings load between the two
tables (fixed code, guarded by the doc comment), the probe being the first statement of
`install_early_diagnostic_locale` (same), that a step's body does what its name says,
that no lower crate reads a document earlier, or the match arms' order in `run_native`.

**Seeds and the settings snapshot.** The four `seed.rs` seeds (hanging punctuation, text
language, rotation Ctrl+wheel, advanced form search) each call `config::load_user_config` instead
of reading the `ProcessServices` snapshot. That is not equivalent, so it stays: `load_user_config`
migrates (`secondary_windows::migrate_android_native_windows`, the legacy memory profile),
backfills the defaults tree and WRITES the result, while `load_user_settings_for_startup` only
merges in memory and does not run the Android secondary-windows migration. On an install whose
first-run marker and `General.ai_install_type` already exist, these seeds are the first startup
calls that persist that migration, which the later snapshot (and the `secondary_windows` seed)
then reads. Reading the snapshot would also move them after the locale reconcile.

**One supervisor construction, two call sites.** The test launcher and the main window each
start the supervisor where they did before (`start_ai_backend_supervisor`): inside `init_full`,
or hoisted above the test-launcher / main split, it would run before the update windows.

A pending storage reconciliation (user_config still in the other format than its recorded mode)
is started on a worker whenever the main window enters the launcher, or — on a direct `--project`
or image start — by the studio screen once the project has loaded (`ms-settings-ui`'s
`storage_mode_job`).

Exit codes are exact because callers branch on them: service flags call `std::process::exit`
instead of returning an `anyhow` error.
- `--check-venv` is TERMINAL: it checks the environment (`ms-installer`'s `venv_check`), exits 0
  with a printed message when it is complete, otherwise opens the installer in
  environment-repair mode and exits 0 (repaired AND re-verified) or 1 (cancelled / failed /
  still not ready). It never opens the launcher or studio and never starts the AI backend.
- `--ignore-installed` marks a run from a source checkout: no Linux desktop-entry write, no
  Windows existing-install discovery, no missing-environment prompt (straight to the launcher),
  no startup update check and a refusal at every self-update entry point, plus a per-root
  backend socket name (`backend_ipc::seed_isolated_backend_socket_name`). The flag is threaded
  explicitly through `StartupRoutingFlags`; the socket name is the only process-global, because
  the supervisor and the IPC client must agree on it without a shared call path.

The prompt windows run ONCE per process, before the main window exists; a later return to the
launcher does not repeat them. Platform gates ask `ms_platform::PlatformFeature`
(`InstalledCopyManagement` for installer / prompts / update check / update window).

## Editing map
- A new CLI flag that selects a mode, or a mode's precedence: `mode.rs::decide` plus a table row
  (and `args.rs`); its effect and place in the order: `mod.rs` (`run_native` match arm,
  `init_minimal`, `run_main`) or `services.rs::init_full`.
- Where the window starts, install prompts routing, update routing: `routing.rs`.
- A flag that must exit with its own code, flag conflicts, `--check-venv`: `service_flags.rs`.
- A pre-window prompt or the startup error box: `prompts.rs` (the missing-environment prompt's
  window and texts: `crates/ms-installer/src/prompt_window.rs`).
- A new process-global seeded at startup: a `PreSettingsStep` (reads `user_config` itself, body in
  `seed.rs`) or a `SettingsStep` (from the snapshot) in `services.rs`, placed in its table and in
  the test's expected positions.
- A startup worker touching OS state: `os_integration.rs`; the desktop entry's text and writer
  live in `crates/ms-os-integration/src/linux/`.
