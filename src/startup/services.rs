/*
FILE OVERVIEW: src/startup/services.rs
The full process seeding of `--check-venv` and the main run, and the process-global facts it
hands to the main run. `init_full` is the ONE owner of the startup-order contract (its doc
comment states it); the order of its config-driven steps is data (`PRE_SETTINGS_STEPS`,
`SETTINGS_STEPS`) that the function iterates and a unit test pins.

Key items:
- `ProcessServices`: the user-settings snapshot, the process's `LocalAiAvailability` (also
  installed process-wide, `ms_local_ai::local_ai::install_process`) and the
  launcher's `LauncherHost`; `start_ai_backend_supervisor` is the one construction of the AI
  backend supervisor (same availability, same storage-conversion autostart gate) for both the
  test launcher and the main window.
- `init_full`: storage-mode probe -> pre-settings steps -> settings load -> pending storage
  reconciliation -> settings-driven steps -> app-version registration -> eager browser-bridge
  start (main run only, when a machine-translation service uses the browser transport).
- `PreSettingsStep` / `SettingsStep`: one variant per seeding call, with the reason for its
  position next to the call.

Notes: native-only like the whole module. The config seeds in `seed.rs` keep their own
`config::load_user_config` reads (not the snapshot): see "Seeds and the settings snapshot" in
`MODULE_README.md` for why that is not equivalent.
*/

use super::launcher_host;
use super::seed::{auto_detect_missing_ai_install_type_for_startup, seed_advanced_form_search_from_config, seed_hanging_punctuation_from_config, seed_rotation_ctrl_wheel_from_config, seed_text_language_from_config};
use crate::args::Cli;
use crate::{ai_backend_supervisor, config, general_settings_panel, hf_token, launcher, locale_store, runtime_log, storage_mode_job, tabs};
use ms_local_ai::local_ai::LocalAiAvailability;

/// What the main run takes from the full seeding. Built only by [`init_full`], so holding one
/// means the whole startup-order contract has run.
#[derive(Debug)]
pub(super) struct ProcessServices {
    /// The merged user settings (`config::load_user_settings_for_startup`), read after the
    /// on-disk locale reconcile; handed to the main window.
    pub(super) user_settings: serde_json::Value,
    /// Which local AI runtimes this process may use: `--no-ai` switches off the Python backend,
    /// native ONNX AND sd.cpp; the platform decides on its own (`LocalAiAvailability::for_process`).
    /// Cloud AI API and machine translation are never gated by it.
    pub(super) local_ai: LocalAiAvailability,
    /// The launcher's process facts (`--ignore-installed`, version core).
    pub(super) host: launcher::state::LauncherHost,
}

impl ProcessServices {
    /// Starts the single Python AI backend supervisor with this process's local-AI availability.
    /// Its autostart waits (on the supervisor worker) until no storage-mode conversion is pending
    /// or running: the backend writes `user_config` itself (KG-016). With `--project` the
    /// reconciliation stays pending until the project is loaded, so the autostart does too.
    ///
    /// The one construction for every mode that runs the main window. It is deliberately NOT
    /// started inside [`init_full`]: the update windows run between the two, so each mode calls
    /// this where it starts the backend. The returned supervisor stops the backend process and its
    /// probe when dropped.
    pub(super) fn start_ai_backend_supervisor(&self) -> ai_backend_supervisor::AiBackendSupervisor {
        ai_backend_supervisor::AiBackendSupervisor::start_with_autostart_gate(self.local_ai, Some(storage_conversion_autostart_gate()))
    }
}

/// The AI backend's deferred-autostart gate: open once the process-wide storage-mode job
/// slot holds nothing pending or running and no chapter conversion is registered. Polled on
/// the supervisor worker (a brief mutex read, never on the GUI thread).
fn storage_conversion_autostart_gate() -> ai_backend_supervisor::AutostartGate {
    Box::new(|| !storage_mode_job::backend_autostart_blocked())
}

/// One seeding call that runs after the storage-mode probe and before the user settings are
/// loaded, in the order of [`PRE_SETTINGS_STEPS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreSettingsStep {
    FirstRunLanguageMarker,
    AiInstallTypeProbe,
    HangingPunctuation,
    TextLanguage,
    RotationCtrlWheel,
    AdvancedFormSearch,
    HfToken,
    TypingEffectDefaults,
    TypingImportedFonts,
    LocaleReconcile,
}

/// The pre-settings steps in their contract order: the first-run marker first, the locale
/// reconcile last (see [`init_full`]).
const PRE_SETTINGS_STEPS: [PreSettingsStep; 10] = [
    PreSettingsStep::FirstRunLanguageMarker,
    PreSettingsStep::AiInstallTypeProbe,
    PreSettingsStep::HangingPunctuation,
    PreSettingsStep::TextLanguage,
    PreSettingsStep::RotationCtrlWheel,
    PreSettingsStep::AdvancedFormSearch,
    PreSettingsStep::HfToken,
    PreSettingsStep::TypingEffectDefaults,
    PreSettingsStep::TypingImportedFonts,
    PreSettingsStep::LocaleReconcile,
];

impl PreSettingsStep {
    /// Runs this step. Every step is best-effort: it logs its own failures and never fails
    /// startup.
    fn run(self) {
        match self {
            // MUST be the first startup call after the storage probe: every following call that
            // touches `load_user_config()` (AI install-type detection, language/effect seeding)
            // persists the full defaults tree, which materializes `General.ui_language` /
            // `TextTab.text_language` and erases the "language keys absent" signal this
            // fresh-install detection depends on. Run it before any of them so the marker is
            // recorded.
            Self::FirstRunLanguageMarker => config::mark_first_run_languages_if_needed(),
            // Fills an absent `General.ai_install_type`, which `--check-venv` (`venv_check`)
            // reads after `init_full` returns.
            Self::AiInstallTypeProbe => auto_detect_missing_ai_install_type_for_startup(),
            Self::HangingPunctuation => seed_hanging_punctuation_from_config(),
            Self::TextLanguage => seed_text_language_from_config(),
            Self::RotationCtrlWheel => seed_rotation_ctrl_wheel_from_config(),
            Self::AdvancedFormSearch => seed_advanced_form_search_from_config(),
            // Reads the Hugging Face token out of the OS secret store on a worker thread, so the
            // token badge and every gated download start from the value the user already saved.
            // Fire-and-forget: until it lands the cache honestly reports "not known".
            Self::HfToken => hf_token::seed_hf_token_from_secret_store(),
            // Seed the typing tab's per-effect-kind default overrides so newly added effect
            // cards pick up the user's stored defaults from the first frame.
            Self::TypingEffectDefaults => tabs::typing::seed_effect_defaults_from_config(),
            // Seed the typing tab's per-font settings (imported system fonts + display-name
            // overrides) from `fonts/fonts_data.json`. On first run it migrates the legacy
            // `user_config.json` imported-fonts list into that file once.
            Self::TypingImportedFonts => tabs::typing::seed_imported_system_fonts_from_config(),
            // Unpack/reconcile the editable on-disk locale catalog before settings load.
            // Best-effort and pre-window: an unwritable `locale/` folder degrades to the
            // embedded catalog without failing startup (see `locale_store` failure policy).
            Self::LocaleReconcile => locale_store::reconcile_disk_catalog(),
        }
    }
}

/// One process-global seeded from the loaded user settings, in the order of [`SETTINGS_STEPS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsStep {
    UiLocale,
    UiScale,
    AutosavePolicy,
    SecondaryWindows,
    TouchGestures,
    MtTransport,
}

/// The settings-driven steps in their order; all run before [`init_full`] returns, so before
/// any window (`shell::run`) exists.
const SETTINGS_STEPS: [SettingsStep; 6] = [
    SettingsStep::UiLocale,
    SettingsStep::UiScale,
    SettingsStep::AutosavePolicy,
    SettingsStep::SecondaryWindows,
    SettingsStep::TouchGestures,
    SettingsStep::MtTransport,
];

impl SettingsStep {
    /// Seeds this step's process-global from `user_settings` (the snapshot [`init_full`] loaded).
    fn run(self, user_settings: &serde_json::Value) {
        match self {
            // Install the active UI locale from disk (or the embedded catalog on failure),
            // now that `General.ui_language` is available in the loaded settings.
            Self::UiLocale => locale_store::install_ui_locale(user_settings),
            // Seed the process-global interface scale before any window is created. Windows read
            // the global (not this snapshot), so a scale changed in the launcher is honored by the
            // studio opened afterwards and vice versa.
            Self::UiScale => general_settings_panel::seed_ui_scale_from_user_settings(user_settings),
            // Seed the process-global autosave policy (interval + action threshold) the save
            // workers re-read on every wait; the General settings pane changes it live afterwards.
            Self::AutosavePolicy => ms_config::autosave_policy::seed_autosave_policy_from_user_settings(user_settings),
            // Seed the process-global secondary-window setting (separate OS windows vs embedded)
            // before `shell::run` reads it; the General settings pane changes it live afterwards.
            Self::SecondaryWindows => ms_config::secondary_windows::seed_from_user_settings(user_settings),
            // Seed the process-global touchscreen-gestures switch the main window's router reads
            // every frame; the General settings pane changes it live afterwards.
            Self::TouchGestures => ms_config::touch_gestures::seed_from_user_settings(user_settings),
            // Seed the live per-service machine-translation transport (built-in requests or the
            // browser extension) every MT run and the Cleaning prompt translator read; the MT
            // panel changes it live afterwards.
            Self::MtTransport => tabs::translation::mt_transport::seed_from_user_settings(user_settings),
        }
    }
}

/// The full process seeding of `--check-venv` and the main run, and the ONE owner of the
/// startup-order contract. Each rule below is load-bearing; none may be reordered:
///
/// - (a) `init_storage_mode_at_startup` is the process's FIRST document access: it seeds the
///   docstore default format, which resolves the "both files exist" rule of every later read
///   or write (fixed first statement here; `init_minimal`, the log init and the Windows service
///   modes touch no document). The terminal paths that never reach this function and read
///   `user_config` for their localized diagnostics (usage error, registration helper, refused
///   self-update) run the same probe first (`service_flags::install_early_diagnostic_locale`);
///   they exit or return instead of calling `init_full`, so the probe runs once per process.
/// - (b) the first-run language marker runs before any `config::load_user_config` (which
///   persists the defaults tree): `PRE_SETTINGS_STEPS[0]`.
/// - (c) the `General.ai_install_type` probe runs here, so before `--check-venv`, which
///   `run_native` starts only after `init_full` returned.
/// - (d) the on-disk locale reconcile runs before `load_user_settings_for_startup`: the last
///   pre-settings step.
/// - (e) the `secondary_windows` seed (and every other settings-driven global) runs before
///   `shell::run`: all of `SETTINGS_STEPS` run before this function returns.
/// - (f) precondition owned by the caller: on Android the working directory is the runtime
///   root BEFORE the first `ms_config` call of the process (`android_entry.rs::start` ->
///   `enter_runtime_root`, ahead of `native_main`); `run_native`'s log init is that first call.
/// - (g) the eager browser-bridge start runs after the `MtTransport` seed (it reads the live
///   transports) and after `register_app_version` (the bridge tells the extension that
///   version); never for `--check-venv`, which exits right after this function.
///
/// The pure facts of [`ProcessServices`] (`local_ai`, `host`) have no side effect, so where they
/// are computed is not part of the contract; `local_ai` is also installed as the process-global
/// answer (`ms_local_ai::local_ai::install_process`, once per process: only this function does
/// it). The AI backend supervisor is not started here (see
/// [`ProcessServices::start_ai_backend_supervisor`]).
///
/// # Errors
/// The user settings could not be loaded (`config::load_user_settings_for_startup`).
pub(super) fn init_full(cli: &Cli) -> anyhow::Result<ProcessServices> {
    runtime_log::log_info("starting main application flow");
    // (a) Reads the Dev/Prod storage mode out of the user config (whichever format it is stored
    // in) and seeds `ms_docstore`'s default format with it; must precede every other read or
    // write, including the first-run marker.
    let storage_probe = config::storage_mode::init_storage_mode_at_startup();
    for step in PRE_SETTINGS_STEPS {
        step.run();
    }
    let user_settings = config::load_user_settings_for_startup()?;
    // The user config (converted last by the driver) is still in the other format: an
    // upgrade from the JSON-only era or an unfinished switch. Converting may take a while
    // (every title), so it runs on a worker started when the first window is up: when the
    // main window enters the launcher, or after the studio's project load.
    if storage_probe.reconciliation_pending {
        storage_mode_job::set_pending_reconciliation(storage_mode_job::ConversionRequest::for_current_install(storage_probe.mode, &user_settings));
    }
    for step in SETTINGS_STEPS {
        step.run(&user_settings);
    }
    // Not a setting and order-free: the browser bridge tells the extension which app version it
    // talks to, and library crates cannot read `MS_APP_VERSION`.
    ms_browser_bridge::app::register_app_version(env!("MS_APP_VERSION"));
    // (g) A service persisted as "through my browser": start the bridge now (on a worker), so
    // the extension has connected by the first translation instead of failing it.
    if !cli.check_venv {
        tabs::translation::mt_transport::start_browser_bridge_if_used();
    }
    let local_ai = LocalAiAvailability::for_process(cli.no_ai);
    ms_local_ai::local_ai::install_process(local_ai);
    Ok(ProcessServices { user_settings, local_ai, host: launcher_host(cli.ignore_installed) })
}

#[cfg(test)]
mod tests {
    use super::{PRE_SETTINGS_STEPS, PreSettingsStep, SETTINGS_STEPS, SettingsStep};

    /// The contract position of every pre-settings step. The exhaustive match makes a new
    /// variant fail to compile here until its position is decided.
    fn expected_pre_settings_position(step: PreSettingsStep) -> usize {
        match step {
            PreSettingsStep::FirstRunLanguageMarker => 0,
            PreSettingsStep::AiInstallTypeProbe => 1,
            PreSettingsStep::HangingPunctuation => 2,
            PreSettingsStep::TextLanguage => 3,
            PreSettingsStep::RotationCtrlWheel => 4,
            PreSettingsStep::AdvancedFormSearch => 5,
            PreSettingsStep::HfToken => 6,
            PreSettingsStep::TypingEffectDefaults => 7,
            PreSettingsStep::TypingImportedFonts => 8,
            PreSettingsStep::LocaleReconcile => 9,
        }
    }

    /// The position of every settings-driven step (exhaustive like the one above).
    fn expected_settings_position(step: SettingsStep) -> usize {
        match step {
            SettingsStep::UiLocale => 0,
            SettingsStep::UiScale => 1,
            SettingsStep::AutosavePolicy => 2,
            SettingsStep::SecondaryWindows => 3,
            SettingsStep::TouchGestures => 4,
            SettingsStep::MtTransport => 5,
        }
    }

    #[test]
    fn pre_settings_steps_keep_the_contract_order() {
        // (b): the first-run marker precedes every step that may persist the defaults tree.
        assert_eq!(PRE_SETTINGS_STEPS.first(), Some(&PreSettingsStep::FirstRunLanguageMarker));
        // (d): the locale reconcile is the last step before the settings load.
        assert_eq!(PRE_SETTINGS_STEPS.last(), Some(&PreSettingsStep::LocaleReconcile));
        // (c): the AI install-type probe is part of the full seeding (which `--check-venv` runs first).
        assert!(PRE_SETTINGS_STEPS.contains(&PreSettingsStep::AiInstallTypeProbe));
        // Every step exactly once, at its decided position (no silent reorder or duplicate).
        for (index, step) in PRE_SETTINGS_STEPS.into_iter().enumerate() {
            assert_eq!(expected_pre_settings_position(step), index, "{step:?} moved");
        }
    }

    #[test]
    fn settings_steps_keep_the_contract_order() {
        // (e): the secondary-windows seed is a settings step, so it runs before `init_full`
        // returns and therefore before `shell::run`.
        assert!(SETTINGS_STEPS.contains(&SettingsStep::SecondaryWindows));
        for (index, step) in SETTINGS_STEPS.into_iter().enumerate() {
            assert_eq!(expected_settings_position(step), index, "{step:?} moved");
        }
    }
}
