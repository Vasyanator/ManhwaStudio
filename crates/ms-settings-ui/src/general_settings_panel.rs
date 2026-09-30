/*
FILE OVERVIEW: src/general_settings_panel.rs
Shared "General settings" widget used by BOTH the studio settings tab and the
launcher settings page.

Why this is shared:
The projects-directory editor and the global memory-profile selector are needed on
both surfaces. Previously the projects-dir editor + its persistence were duplicated
between the studio general pane and the launcher settings page, and the memory-profile
combo lived only in the studio. This module renders that one panel against a per-UI
[`GeneralSettingsPanelState`] (input scratch + the persisted values it mirrors) and
returns a [`GeneralSettingsOutcome`] describing the per-call-site runtime effects the
caller must apply (there is no app-global channel here, unlike `ai_backend_panel`).

Persistence is SYNCHRONOUS and goes through `ms_config::update_user_config_file`, the
serialized user-config read-modify-write, so a write never clobbers the ONNX Runtime
SIGILL load-guard marker or another writer's keys (see the user_config write-lock
invariant in `ARCHITECTURE.md`, "Config", and `crates/ms-config/src/MODULE_README.md`).

The UI-language selector lists the locales found in the on-disk `locale/` folder
(scanned ONCE at construction — never per frame; see CLAUDE.md §5), each shown by
its `_meta.name`. Changing it persists `General.ui_language` and live-installs that
locale's catalog (falling back to the embedded catalog), with no restart.

The typesetting-language selector below it is a DUPLICATE surface for the same
setting the "Тайп" settings pane owns (`TextTab.text_language`): two independent
languages — interface vs. typeset text — are chosen next to each other here. Both
surfaces read and write the process-global `ms_text_util::language`, which is the
single source of truth, so they cannot drift out of sync.

The interface-scale slider is a THIRD kind of setting here: like the UI language it is
applied live rather than returned in the outcome, but it applies only to the surface
that renders it, because `Context::set_zoom_factor` is per-`Context` and launcher and
studio are separate `run_native` windows. The other surface picks the value up at
startup through `apply_ui_scale_from_user_settings`.

Key items:
- `GeneralSettingsPanelState`: per-UI scratch + mirrored persisted values.
- `GeneralSettingsOutcome`: per-call-site runtime effects to apply after drawing.
- `LocaleOption`: one selectable interface language (tag + display name).
- `build_locale_options`: pure, filesystem-free option builder (deterministic).
- `draw_general_settings_panel`: renders the projects-dir editor + Dev/Prod storage row
  (`storage_mode_setting`) + memory-profile combo + interface-scale slider + autosave policy + UI-language
  selector + typesetting-language selector.
- `draw_autosave_settings`: the global autosave policy (interval minutes + action
  threshold); applied live to `ms_config::autosave_policy` first, then persisted.
- `apply_ui_scale` / `apply_ui_scale_from_user_settings`: apply `General.ui_scale_percent`
  to an egui context (called by each `run_native` constructor closure).
- `draw_text_language_setting`: the shared typesetting-language selector (script group +
  language combos), also called by the studio "Тайп" pane; takes a per-call-site
  `id_salt` prefix so the two rendered instances keep distinct egui ids.

Reuse by the launcher first-run modal:
The launcher's first-run language modal (`src/launcher/first_run_language.rs`) reuses
this module's `pub(crate)` helpers so the two surfaces never drift: `scan_locale_options`
(identical option set/order), `install_selected_ui_locale` (live locale install) and
`persist_config_keys` (one atomic `General.ui_language` + `TextTab.text_language` write on
confirm). It renders its own radio-based UI instead of the combo-based `draw_*` functions,
but persists through the same paths.
*/

use ms_i18n::resolve_key;
use ms_memory::MemoryProfile;
use ms_log::runtime_log;
use ms_widgets::{WheelComboBox, WheelSlider};
use ms_text_util::language::{ScriptGroup, TextLanguage, set_text_language, text_language};
use ms_thread as thread;
use crate::storage_mode_setting::{StorageModeSettingState, draw_storage_mode_setting};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// Status line shown under the projects-directory editor.
///
/// `Idle` shows a neutral hint; `Info`/`Success`/`Error` carry a user-facing
/// (Cyrillic) message. String payloads mean this cannot be `Copy`.
#[derive(Debug, Clone, Default)]
pub enum GeneralSettingsStatus {
    #[default]
    Idle,
    Info(String),
    Success(String),
    Error(String),
}

/// Per-UI state for the shared general-settings widget.
///
/// Owns the editable projects-dir input, the last successfully saved (normalized)
/// projects root it is compared against, the current global memory profile, and the
/// status line. Each call site (studio / launcher) owns one instance.
#[derive(Debug)]
pub struct GeneralSettingsPanelState {
    /// Editable projects-directory text field contents.
    pub projects_dir_input: String,
    /// Last successfully persisted, normalized projects root; drives the dirty check.
    pub saved_projects_dir: String,
    /// Current global image-cache memory profile.
    pub memory_profile: MemoryProfile,
    /// Currently selected interface-language tag (an `ms_i18n::LocaleTag` such as
    /// `"ru"`, or a custom on-disk locale tag). Persisted to `General.ui_language`.
    pub ui_language_tag: String,
    /// Interface-language options, scanned ONCE at construction from the `locale/`
    /// folder (the GUI thread never rescans the filesystem per frame).
    pub locale_options: Vec<LocaleOption>,
    /// Interface scale in percent as shown by the slider (`100` = native size).
    /// Persisted to `General.ui_scale_percent`.
    pub ui_scale_percent: u32,
    /// The scale currently applied to this surface's egui context. Differs from
    /// [`Self::ui_scale_percent`] only while the user is still dragging the slider;
    /// see the apply rule in [`draw_general_settings_panel`].
    pub applied_ui_scale_percent: u32,
    /// Autosave write interval in minutes as shown by the slider. Persisted to
    /// `General.autosave_interval_minutes`; the live value is `ms_config::autosave_policy`.
    pub autosave_interval_minutes: u32,
    /// Autosave action-count threshold as shown by the slider. Persisted to
    /// `General.autosave_action_threshold`; the live value is `ms_config::autosave_policy`.
    pub autosave_action_threshold: u32,
    /// The user's explicit primary-monitor choice (`Window.monitor`), or `None` for "auto",
    /// which means the largest connected monitor. Native-only: there are no OS monitors in a
    /// web build.
    #[cfg(not(target_arch = "wasm32"))]
    pub preferred_monitor: Option<ms_window_geometry::MonitorKey>,
    /// Status line under the projects-dir editor.
    pub status: GeneralSettingsStatus,
    /// The Dev/Prod storage row (conversion job bookkeeping of this surface).
    pub storage_mode: StorageModeSettingState,
}

/// One selectable interface language for the UI-language selector.
///
/// `tag` is the locale tag persisted to `General.ui_language`; `display` is the
/// name shown in the combo, taken from the locale file's `_meta.name` (falling
/// back to the tag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleOption {
    /// Locale tag (`"en"`, `"ru"`, a custom `"de"`, …).
    pub tag: String,
    /// Human-readable display name shown in the combo.
    pub display: String,
}

/// Per-call-site runtime effects produced by [`draw_general_settings_panel`].
///
/// Each field is `Some` only when the corresponding change happened this frame; the
/// caller applies the runtime effect (the widget already persisted the value). The
/// launcher acts on `projects_dir_saved`; the studio acts on `memory_profile_changed`.
#[derive(Debug, Default)]
pub struct GeneralSettingsOutcome {
    /// Set to the normalized saved root when the user saved a NEW projects dir.
    pub projects_dir_saved: Option<PathBuf>,
    /// Set to the new profile when the memory-profile selection changed.
    pub memory_profile_changed: Option<MemoryProfile>,
    /// Set to the target mode on the frame a storage-mode switch started by this pane
    /// finished converting (successfully or with per-document failures).
    pub storage_mode_changed: Option<ms_config::StorageMode>,
}

impl Default for GeneralSettingsPanelState {
    fn default() -> Self {
        Self::new()
    }
}

impl GeneralSettingsPanelState {
    /// Seeds the state from the persisted `user_config.json`: the projects root and
    /// the global memory profile.
    ///
    /// Reads the startup-safe raw settings once (no default backfilling / file
    /// creation). On a read error it logs and falls back to the default projects root
    /// and default memory profile so the UI still opens. The legacy
    /// `Canvas.cache_pages`→memory-profile migration is applied and written back to
    /// disk by `config::load_user_config()`, which runs during startup seeding in
    /// `main.rs` (before any settings panel is constructed), so the `memory_profile`
    /// read here is already migrated.
    #[must_use]
    pub fn new() -> Self {
        let (projects_dir, memory_profile, ui_language_tag) =
            match ms_config::load_raw_user_settings_for_startup() {
                Ok(settings) => (
                    ms_config::projects_root_from_user_settings(&settings)
                        .to_string_lossy()
                        .into_owned(),
                    ms_config::memory_profile_from_user_settings(&settings),
                    ui_language_tag_from_settings(&settings),
                ),
                Err(err) => {
                    runtime_log::log_error(format!(
                        "[general-settings] failed to read user settings for seeding; using \
                         defaults; error={err:#}"
                    ));
                    (
                        ms_config::default_projects_root()
                            .to_string_lossy()
                            .into_owned(),
                        MemoryProfile::default(),
                        DEFAULT_UI_LANGUAGE_TAG.to_string(),
                    )
                }
            };
        // The interface scale comes from the process-global value, not from disk: it is
        // what this surface's context actually renders at, and it stays right even if a
        // persist failed earlier in the session.
        let ui_scale_percent = ui_scale_percent();
        // Same reasoning for the autosave policy: the process-global policy is what the
        // save workers obey right now, seeded from disk at startup.
        let autosave_policy = ms_config::autosave_policy::autosave_policy();
        Self {
            projects_dir_input: projects_dir.clone(),
            saved_projects_dir: projects_dir,
            memory_profile,
            ui_language_tag,
            ui_scale_percent,
            // The surface applied this same value to its egui context when the window
            // was created (`apply_ui_scale_to_context`), so nothing is pending.
            applied_ui_scale_percent: ui_scale_percent,
            autosave_interval_minutes: autosave_policy.interval_minutes(),
            autosave_action_threshold: autosave_policy.action_threshold,
            #[cfg(not(target_arch = "wasm32"))]
            preferred_monitor: seed_preferred_monitor(),
            // Filesystem scan happens once here, at construction — never per frame.
            locale_options: scan_locale_options(),
            status: GeneralSettingsStatus::Idle,
            storage_mode: StorageModeSettingState::default(),
        }
    }

    /// Re-syncs both projects-dir fields when the projects root changes externally
    /// (used by the launcher's `set_projects_root` when another page changes it).
    pub fn set_projects_root(&mut self, root: &str) {
        let normalized = normalize_projects_dir_value(root);
        self.projects_dir_input = normalized.clone();
        self.saved_projects_dir = normalized;
    }
}

/// Renders the shared general-settings widget (projects-directory editor + global
/// memory-profile combo) and returns the runtime effects the caller must apply.
///
/// Persists a changed projects dir / memory profile synchronously (one serialized
/// `ms_docstore` update of `user_config.json`, under its document lock); persistence failures set an error status and
/// are logged. The native folder picker button is desktop-only.
#[must_use]
pub fn draw_general_settings_panel(
    ui: &mut egui::Ui,
    state: &mut GeneralSettingsPanelState,
) -> GeneralSettingsOutcome {
    let mut outcome = GeneralSettingsOutcome::default();

    // Projects-directory editor (rich variant: text field + folder picker + save).
    ui.label(t!("settings.general.projects_dir_label"));
    let mut should_save = false;
    ui.horizontal_wrapped(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(&mut state.projects_dir_input)
                .desired_width(420.0)
                .hint_text(t!("settings.general.projects_dir_picker_title")),
        );
        // Editing the field clears a stale "saved" confirmation.
        if response.changed() {
            clear_success_status(state);
        }
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            should_save = true;
        }
        // The native OS folder picker exists only on desktop; on web there is no OS
        // directory to browse, so the button is omitted.
        #[cfg(not(target_arch = "wasm32"))]
        if ui.button(t!("settings.general.browse_button")).clicked() {
            pick_projects_dir(state);
        }
    });

    ui.small(
        t!("settings.general.projects_dir_hint"),
    );

    draw_status(ui, &state.status);

    let dirty = projects_dir_is_dirty(&state.projects_dir_input, &state.saved_projects_dir);
    if ui
        .add_enabled(dirty, egui::Button::new(t!("settings.general.save_projects_dir_button")))
        .clicked()
    {
        should_save = true;
    }

    if should_save {
        let normalized = normalize_projects_dir_value(&state.projects_dir_input);
        match persist_general_key(
            ms_config::GENERAL_PROJECTS_DIR_KEY,
            serde_json::Value::String(normalized.clone()),
        ) {
            Ok(()) => {
                state.saved_projects_dir = normalized.clone();
                state.projects_dir_input = normalized.clone();
                state.status =
                    GeneralSettingsStatus::Success(t!("settings.general.projects_dir_saved").to_string());
                outcome.projects_dir_saved = Some(PathBuf::from(normalized));
            }
            Err(err) => {
                runtime_log::log_error(format!(
                    "[general-settings] failed to persist projects directory '{normalized}'; \
                     error={err}"
                ));
                state.status = GeneralSettingsStatus::Error(tf!("settings.general.projects_dir_save_error", err = err));
            }
        }
    }

    ui.separator();

    // Dev/Prod document storage, next to the projects root whose titles it converts.
    outcome.storage_mode_changed = draw_storage_mode_setting(ui, &mut state.storage_mode, std::path::Path::new(&state.saved_projects_dir));

    ui.separator();

    // Global memory-profile selector (applied to the runtime by the caller).
    ui.label(t!("settings.general.memory_profile_label"));
    ui.small(t!("settings.general.memory_profile_hint"));
    let mut selected_profile = state.memory_profile;
    egui::ComboBox::from_id_salt("settings_memory_profile")
        .selected_text(selected_profile.display_name_ru())
        .show_ui(ui, |ui| {
            for profile in MemoryProfile::ALL {
                ui.selectable_value(&mut selected_profile, profile, profile.display_name_ru());
            }
        });
    if selected_profile != state.memory_profile {
        state.memory_profile = selected_profile;
        // The runtime effect (applying the profile to the MemoryManager) is the
        // caller's job; the widget only persists the choice.
        outcome.memory_profile_changed = Some(selected_profile);
        if let Err(err) = persist_general_key(
            ms_config::GENERAL_MEMORY_PROFILE_KEY,
            serde_json::Value::String(selected_profile.as_config_str().to_string()),
        ) {
            runtime_log::log_error(format!(
                "[general-settings] failed to persist memory profile '{}'; error={err}",
                selected_profile.as_config_str()
            ));
            state.status =
                GeneralSettingsStatus::Error(t!("settings.general.memory_profile_save_error").to_string());
        }
    }

    ui.separator();

    draw_ui_scale_setting(ui, state);

    ui.separator();

    draw_autosave_settings(ui, state);

    ui.separator();

    draw_primary_monitor_setting(ui, state);

    ui.separator();

    // Interface-language selector. Populated once from the on-disk `locale/` folder
    // (see `scan_locale_options`); changing it persists and live-installs the locale.
    ui.label(t!("settings.general.ui_language_label"));
    ui.small(t!("settings.general.ui_language_hint"));
    let previous_tag = state.ui_language_tag.clone();
    let selected_display = state
        .locale_options
        .iter()
        .find(|option| option.tag == state.ui_language_tag)
        .map_or_else(|| state.ui_language_tag.clone(), |option| option.display.clone());
    // Clone the options so the popup closure can borrow `state.ui_language_tag`
    // mutably without also holding an immutable borrow of `state.locale_options`.
    let options = state.locale_options.clone();
    ui.horizontal_wrapped(|ui| {
        WheelComboBox::from_label(t!("settings.general.ui_language_combo_label")).id_salt("settings.general.ui_language_combo_label")
            .selected_text(selected_display)
            .show_ui(ui, |ui| {
                for option in &options {
                    ui.selectable_value(
                        &mut state.ui_language_tag,
                        option.tag.clone(),
                        option.display.as_str(),
                    );
                }
            });
    });
    if state.ui_language_tag != previous_tag {
        apply_ui_language_change(ui, state);
    }

    ui.separator();

    draw_text_language_setting(ui, "settings.general.text_language");

    outcome
}

/// Renders the typesetting-language selector: a `ScriptGroup` combo followed by the
/// concrete `TextLanguage` combo within that group. Shared by BOTH the studio "Тайп"
/// settings pane and this general-settings widget, so the choice is reachable from the
/// studio Тайп pane and from the launcher/studio general widget.
///
/// `id_salt` is a per-call-site egui id prefix (`"<prefix>.script_group"` /
/// `"<prefix>.language"`) so two rendered instances never collide; pass a distinct,
/// stable prefix per call site.
///
/// Holds no state: the process-global `ms_text_util::language::text_language()` is
/// the single source of truth, so every instance shows the same value. Selecting a
/// group switches to that group's first language. A change applies live (the typing
/// tab's `facade.rs` observes `text_language()` each frame and re-runs font-coverage
/// classification off-thread) and persists `TextTab.text_language` on a background
/// thread via [`persist_text_language`].
pub fn draw_text_language_setting(ui: &mut egui::Ui, id_salt: &str) {
    ui.label(t!("settings.typesetting.text_language_label"));
    ui.small(t!("settings.typesetting.text_language_hint"));

    let current = text_language();
    let current_group = current.group();

    // Group combo: selecting a different group switches to that group's first language.
    let mut selected_group = current_group;
    ui.horizontal_wrapped(|ui| {
        WheelComboBox::from_label(t!("settings.typesetting.script_group_label"))
            .id_salt(format!("{id_salt}.script_group"))
            .selected_text(resolve_key(current_group.name_key()))
            .show_ui(ui, |ui| {
                for group in ScriptGroup::all() {
                    ui.selectable_value(&mut selected_group, group, resolve_key(group.name_key()));
                }
            });
    });

    // Language combo lists only the (possibly new) group's languages. When the group
    // changed this frame, offer that group's first language as selected.
    let mut selected_language = if selected_group == current_group {
        current
    } else {
        selected_group.first_language()
    };
    ui.horizontal_wrapped(|ui| {
        WheelComboBox::from_label(t!("settings.typesetting.language_label"))
            .id_salt(format!("{id_salt}.language"))
            .selected_text(resolve_key(selected_language.name_key()))
            .show_ui(ui, |ui| {
                for language in selected_group.languages() {
                    ui.selectable_value(
                        &mut selected_language,
                        *language,
                        resolve_key(language.name_key()),
                    );
                }
            });
    });

    if selected_language != current {
        // Apply live first so the change takes effect even if the disk write fails.
        set_text_language(selected_language);
        persist_text_language(selected_language);
    }
}

/// Reads the persisted primary-monitor choice for a freshly constructed panel state.
///
/// A config read failure is logged and treated as "no choice made": the selector then shows
/// "auto" instead of a stale value, and the next explicit pick repairs the section.
#[cfg(not(target_arch = "wasm32"))]
fn seed_preferred_monitor() -> Option<ms_window_geometry::MonitorKey> {
    let settings = ms_config::load_raw_user_settings_for_startup().unwrap_or_else(|err| {
        runtime_log::log_error(format!(
            "[general-settings] failed to read the primary-monitor choice; showing 'auto'; \
             error={err:#}"
        ));
        serde_json::Value::Null
    });
    ms_window_geometry::window_settings_from_user_settings(&settings).monitor
}

/// Renders the primary-monitor selector: the monitor the program opens its windows on.
///
/// The option list comes from the monitors the live window can see
/// (`window_geometry::monitor_snapshot`), so it is only populated once some window has
/// published one. Every state in which the choice cannot work — no monitor list at all, or a
/// session (Wayland) whose compositor owns window placement — is stated in the UI instead of
/// silently hiding the control.
///
/// A change persists `Window.monitor` synchronously and asks the studio window to move onto
/// the new monitor right away; other windows follow at their next start.
#[cfg(not(target_arch = "wasm32"))]
fn draw_primary_monitor_setting(ui: &mut egui::Ui, state: &mut GeneralSettingsPanelState) {
    use ms_window_geometry::{self as window_geometry, MonitorKey, MonitorResolution};

    ui.label(t!("settings.general.monitor_label"));
    ui.small(t!("settings.general.monitor_hint"));

    let Some(snapshot) = window_geometry::monitor_snapshot() else {
        ui.small(t!("settings.general.monitor_unavailable_hint"));
        return;
    };
    if !snapshot.position_supported {
        ui.small(t!("settings.general.monitor_wayland_hint"));
        return;
    }
    if snapshot.monitors.is_empty() {
        ui.small(t!("settings.general.monitor_unavailable_hint"));
        return;
    }

    let selected_text = match state.preferred_monitor.as_ref() {
        None => t!("settings.general.monitor_auto_option").to_string(),
        Some(chosen) => match window_geometry::resolve_monitor(Some(chosen), &snapshot.monitors) {
            MonitorResolution::Preferred(index) => snapshot
                .monitors
                .get(index)
                .map_or_else(String::new, |monitor| monitor_option_label(monitor, index)),
            // The chosen monitor is not connected right now. The choice is kept (the monitor
            // may come back), so the selector says so instead of silently showing "auto".
            MonitorResolution::NoMonitors | MonitorResolution::Fallback { .. } => tf!(
                "settings.general.monitor_missing_option",
                name = monitor_display_name(chosen, 0)
            ),
        },
    };

    let mut chosen: Option<MonitorKey> = state.preferred_monitor.clone();
    ui.horizontal_wrapped(|ui| {
        WheelComboBox::from_label(t!("settings.general.monitor_combo_label"))
            .id_salt("settings.general.monitor_combo")
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut chosen,
                    None,
                    t!("settings.general.monitor_auto_option"),
                );
                for (index, monitor) in snapshot.monitors.iter().enumerate() {
                    ui.selectable_value(
                        &mut chosen,
                        Some(monitor.clone()),
                        monitor_option_label(monitor, index),
                    );
                }
            });
    });

    if chosen == state.preferred_monitor {
        return;
    }
    state.preferred_monitor = chosen.clone();
    if let Err(err) = window_geometry::persist_preferred_monitor(chosen.clone()) {
        runtime_log::log_error(format!("[general-settings] {err}"));
        state.status =
            GeneralSettingsStatus::Error(t!("settings.general.monitor_save_error").to_string());
        return;
    }
    // Applying live is what makes the setting verifiable; a window without a geometry tracker
    // (the launcher) simply picks the choice up at its next start.
    if let Some(monitor) = chosen {
        window_geometry::request_relocation(monitor);
    }
}

/// Web stub of the primary-monitor selector: a browser tab has no OS monitors to choose from,
/// so the row states that instead of disappearing.
#[cfg(target_arch = "wasm32")]
fn draw_primary_monitor_setting(ui: &mut egui::Ui, _state: &mut GeneralSettingsPanelState) {
    ui.label(t!("settings.general.monitor_label"));
    ui.small(t!("settings.general.monitor_unavailable_hint"));
}

/// Combo entry for one monitor: its name (or an index-based placeholder) plus its resolution.
#[cfg(not(target_arch = "wasm32"))]
fn monitor_option_label(monitor: &ms_window_geometry::MonitorKey, index: usize) -> String {
    tf!(
        "settings.general.monitor_option",
        name = monitor_display_name(monitor, index),
        width = monitor.w,
        height = monitor.h
    )
}

/// The monitor's OS name, or a 1-based positional placeholder when the platform reports none.
#[cfg(not(target_arch = "wasm32"))]
fn monitor_display_name(monitor: &ms_window_geometry::MonitorKey, index: usize) -> String {
    monitor
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map_or_else(
            || tf!("settings.general.monitor_unnamed", index = index + 1),
            ToString::to_string,
        )
}

/// Renders the global interface-scale slider (percent of native size) and applies a
/// settled change to the surface's own egui context.
///
/// The scale is one `Context::set_zoom_factor` away from `pixels_per_point`, so it
/// rescales the WHOLE surface at once — fonts, spacing, widget sizes — without
/// changing the OS window size. Each surface (launcher / studio) owns its own
/// `Context`, so this applies live only here; the other surface picks the value up
/// from `General.ui_scale_percent` when its window is created.
///
/// Apply rule: a change is applied (and persisted) only once the slider is no longer
/// being dragged. Applying mid-drag would rescale the slider itself under the cursor
/// and make the control fight the pointer; wheel, click and keyboard changes are not
/// drags and therefore apply immediately.
fn draw_ui_scale_setting(ui: &mut egui::Ui, state: &mut GeneralSettingsPanelState) {
    ui.label(t!("settings.general.ui_scale_label"));
    ui.small(t!("settings.general.ui_scale_hint"));

    let slider = ui.add(
        WheelSlider::new(
            &mut state.ui_scale_percent,
            ms_config::UI_SCALE_PERCENT_MIN..=ms_config::UI_SCALE_PERCENT_MAX,
        )
        .suffix("%")
        .step_by(f64::from(UI_SCALE_STEP_PERCENT))
        .wheel_step(f64::from(UI_SCALE_STEP_PERCENT))
        // Typing into the value field must not apply a half-typed number: "150" would
        // otherwise pass through 1 -> 15 (both clamped to the 50 % floor), rescaling the
        // window and writing the config on every keystroke. Applied on Enter/focus loss.
        .update_while_editing(false),
    );

    if state.ui_scale_percent != state.applied_ui_scale_percent && !slider.dragged() {
        let percent = state.ui_scale_percent;
        apply_ui_scale(ui.ctx(), percent);
        state.applied_ui_scale_percent = percent;
        if let Err(err) = persist_general_key(
            ms_config::GENERAL_UI_SCALE_PERCENT_KEY,
            serde_json::Value::from(percent),
        ) {
            runtime_log::log_error(format!(
                "[general-settings] failed to persist ui scale '{percent}%'; error={err}"
            ));
            state.status =
                GeneralSettingsStatus::Error(t!("settings.general.ui_scale_save_error").to_string());
        }
    }
}

/// Renders the global autosave policy: the write interval (minutes) and the action-count
/// threshold after which pending edits are written to the chapter's unsaved session.
///
/// Both values live in the process-global `ms_config::autosave_policy`, which the save
/// workers re-read on every wait, so a settled change is applied live FIRST (it takes
/// effect even if the disk write fails) and then persisted to `General.*`. Like the
/// interface scale, a change settles only once the slider is no longer dragged, so a drag
/// does not write the config on every frame. A persist failure is logged and shown in the
/// panel status line. Shared by the studio settings tab and the launcher settings page.
fn draw_autosave_settings(ui: &mut egui::Ui, state: &mut GeneralSettingsPanelState) {
    // Stable id scope: the sliders' ids must not depend on the localized labels above them.
    ui.push_id("settings_general_autosave", |ui| {
        ui.strong(t!("settings.general.autosave_heading"));

        let policy = ms_config::autosave_policy::autosave_policy();

        ui.label(t!("settings.general.autosave_interval_label"));
        let interval_slider = ui.add(
            WheelSlider::new(
                &mut state.autosave_interval_minutes,
                ms_config::AUTOSAVE_INTERVAL_MINUTES_MIN..=ms_config::AUTOSAVE_INTERVAL_MINUTES_MAX,
            )
            .suffix(t!("settings.general.autosave_interval_suffix"))
            // A half-typed number ("15" passing through "1") must not be applied and
            // persisted per keystroke; typed input applies on Enter/focus loss.
            .update_while_editing(false),
        );
        if state.autosave_interval_minutes != policy.interval_minutes() && !interval_slider.dragged() {
            ms_config::autosave_policy::set_autosave_interval_minutes(state.autosave_interval_minutes);
            // Mirror the stored (clamped) value so a clamp can never make this branch fire
            // again on every frame.
            state.autosave_interval_minutes = ms_config::autosave_policy::autosave_policy().interval_minutes();
            persist_autosave_key(state, ms_config::GENERAL_AUTOSAVE_INTERVAL_MINUTES_KEY, state.autosave_interval_minutes);
        }

        ui.label(t!("settings.general.autosave_threshold_label"));
        let threshold_slider = ui.add(
            WheelSlider::new(
                &mut state.autosave_action_threshold,
                ms_config::AUTOSAVE_ACTION_THRESHOLD_MIN..=ms_config::AUTOSAVE_ACTION_THRESHOLD_MAX,
            )
            .update_while_editing(false),
        );
        if state.autosave_action_threshold != policy.action_threshold && !threshold_slider.dragged() {
            ms_config::autosave_policy::set_autosave_action_threshold(state.autosave_action_threshold);
            state.autosave_action_threshold = ms_config::autosave_policy::autosave_policy().action_threshold;
            persist_autosave_key(state, ms_config::GENERAL_AUTOSAVE_ACTION_THRESHOLD_KEY, state.autosave_action_threshold);
        }

        ui.small(t!("settings.general.autosave_hint"));
    });
}

/// Persists one `General.<key>` autosave value; on failure logs it and sets the panel's
/// error status (the live value stays applied for the rest of the session).
fn persist_autosave_key(state: &mut GeneralSettingsPanelState, key: &str, value: u32) {
    if let Err(err) = persist_general_key(key, serde_json::Value::from(value)) {
        runtime_log::log_error(format!(
            "[general-settings] failed to persist autosave setting '{key}'={value}; error={err}"
        ));
        state.status = GeneralSettingsStatus::Error(tf!("settings.general.autosave_save_error", err = err));
    }
}

/// Slider/wheel step of the interface-scale control, in percent.
const UI_SCALE_STEP_PERCENT: u16 = 5;

/// Process-global interface scale, in percent. The single source of truth while the
/// process runs, mirroring how the UI language lives in the `ms-i18n` runtime rather
/// than in a settings snapshot.
///
/// It must NOT be re-derived from the startup `user_settings` snapshot per window:
/// `run_main` reads that snapshot ONCE and reuses it for every launcher/studio window
/// of the session, so a scale changed in one surface would be invisible to the next
/// window opened after it.
static UI_SCALE_PERCENT: AtomicU32 = AtomicU32::new(ms_config::UI_SCALE_PERCENT_DEFAULT);

/// Seeds the process-global interface scale from the startup user settings.
///
/// Called ONCE during startup, before any window is created (next to the other
/// `seed_*_from_config` calls in `run_main`). Out-of-range stored values are clamped
/// by [`ms_config::ui_scale_percent_from_user_settings`].
pub fn seed_ui_scale_from_user_settings(user_settings: &serde_json::Value) {
    UI_SCALE_PERCENT.store(
        ms_config::ui_scale_percent_from_user_settings(user_settings),
        Ordering::Relaxed,
    );
}

/// The current process-global interface scale, in percent.
#[must_use]
pub fn ui_scale_percent() -> u32 {
    UI_SCALE_PERCENT.load(Ordering::Relaxed)
}

/// Applies an interface scale (in percent) to `ctx` via `Context::set_zoom_factor` and
/// records it as the new process-global value.
///
/// Out-of-range input is clamped by [`ms_config::ui_scale_factor_from_percent`].
/// The zoom change becomes active at the start of the next pass (egui defers it to
/// avoid jitter) and egui requests the repaint itself. Only the passed context is
/// rescaled — other open windows own separate `Context`s and pick the value up when
/// they are (re)created.
pub fn apply_ui_scale(ctx: &egui::Context, percent: u32) {
    UI_SCALE_PERCENT.store(percent, Ordering::Relaxed);
    ctx.set_zoom_factor(ms_config::ui_scale_factor_from_percent(percent));
}

/// Applies the process-global interface scale to a freshly created egui context.
///
/// Every `eframe::run_native` constructor closure that should honor the global
/// interface scale calls this ONCE, next to `ui_fonts::install*`. A surface that does
/// not call it simply renders at native size.
pub fn apply_ui_scale_to_context(ctx: &egui::Context) {
    ctx.set_zoom_factor(ms_config::ui_scale_factor_from_percent(ui_scale_percent()));
}

/// Persists the chosen typesetting language to `TextTab.text_language` on a
/// background thread (the GUI thread must never do disk I/O; see CLAUDE.md §5).
///
/// A failed spawn or a failed write is logged and nothing else: the live value has
/// already been applied, so the UI stays consistent for this session and only the
/// persistence is lost.
fn persist_text_language(language: TextLanguage) {
    let path = ms_config::user_config_path();
    let tag = language.tag().to_string();
    if let Err(err) = thread::Builder::new()
        .name("general-settings-text-language-save".to_string())
        .spawn(move || {
            if let Err(err) = ms_config::save_text_language(&path, &tag) {
                runtime_log::log_error(format!(
                    "[general-settings] failed to persist text language to {}; error={err}",
                    path.display()
                ));
            }
        })
    {
        runtime_log::log_error(format!(
            "[general-settings] failed to start text language save thread; error={err}"
        ));
    }
}

/// Persists the newly selected UI-language tag and live-installs its catalog.
///
/// Persistence is synchronous, matching this widget's projects-dir / memory-profile
/// writes: one tiny key write on an explicit user action, serialized on the
/// `user_config.json` document lock so it never clobbers the ORT load-guard marker. The install is live (no restart) and the frame is repainted so the new
/// strings show immediately.
fn apply_ui_language_change(ui: &egui::Ui, state: &mut GeneralSettingsPanelState) {
    let tag = state.ui_language_tag.clone();
    if let Err(err) = persist_general_key(
        ms_config::GENERAL_UI_LANGUAGE_KEY,
        serde_json::Value::String(tag.clone()),
    ) {
        runtime_log::log_error(format!(
            "[general-settings] failed to persist ui language '{tag}'; error={err}"
        ));
        state.status =
            GeneralSettingsStatus::Error(t!("settings.general.ui_language_save_error").to_string());
    }
    install_selected_ui_locale(&tag);
    ui.ctx().request_repaint();
}

/// Live-installs the selected locale on desktop: loads it from the on-disk
/// `locale/` folder (with embedded / English fallback) and installs it into the
/// `ms-i18n` runtime by reusing the startup install path.
///
/// `pub(crate)` so the launcher's first-run language modal live-installs the
/// preselected/clicked locale through the same path (no duplication).
#[cfg(not(target_arch = "wasm32"))]
pub fn install_selected_ui_locale(tag: &str) {
    // Hand the startup installer a minimal settings object carrying only the chosen
    // tag; it performs the disk-load + embedded/English fallback and the install.
    let settings =
        serde_json::json!({ "General": { ms_config::GENERAL_UI_LANGUAGE_KEY: tag } });
    ms_config::locale_store::install_ui_locale(&settings);
}

/// Web twin: no on-disk `locale/` folder on wasm, so install the embedded catalog
/// for the tag directly. An invalid tag / missing embedded catalog is logged and
/// the UI language is left unchanged (never a panic).
#[cfg(target_arch = "wasm32")]
pub(crate) fn install_selected_ui_locale(tag: &str) {
    match ms_i18n::LocaleTag::parse(tag) {
        Ok(locale_tag) => {
            if let Err(err) = ms_i18n::set_locale(&locale_tag) {
                runtime_log::log_warn(format!(
                    "[general-settings] no embedded catalog for '{tag}' ({err}); \
                     UI language unchanged"
                ));
            }
        }
        Err(err) => runtime_log::log_warn(format!(
            "[general-settings] invalid ui language tag '{tag}' ({err}); UI language unchanged"
        )),
    }
}

/// Reads `General.ui_language` as a raw tag string, defaulting to
/// [`DEFAULT_UI_LANGUAGE_TAG`]. A blank value falls back to the default.
fn ui_language_tag_from_settings(settings: &serde_json::Value) -> String {
    settings
        .get("General")
        .and_then(serde_json::Value::as_object)
        .and_then(|general| general.get(ms_config::GENERAL_UI_LANGUAGE_KEY))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .unwrap_or(DEFAULT_UI_LANGUAGE_TAG)
        .to_string()
}

/// Default interface-language tag when config is missing/blank (matches
/// `config::user_config_defaults()` and `locale_store`'s startup default).
const DEFAULT_UI_LANGUAGE_TAG: &str = "ru";

/// Builds the interface-language option list by scanning the on-disk `locale/`
/// folder once and folding in the embedded catalogs as a fallback.
///
/// Disk files are listed first, so a user-authored `locale/<tag>.json` (a custom
/// language or an override of an embedded one) wins its `_meta.name`; the embedded
/// `en`/`ru` fill any gaps, guaranteeing the list is never empty. Called ONCE at
/// construction — never on the per-frame draw path.
///
/// `pub(crate)` so the launcher's first-run language modal reuses the exact same
/// scan (identical option set and ordering as the settings pane) instead of
/// duplicating the disk/embedded merge.
pub fn scan_locale_options() -> Vec<LocaleOption> {
    let mut pairs = disk_locale_pairs();
    pairs.extend(embedded_locale_pairs());
    build_locale_options(pairs)
}

/// Builds the deterministic, de-duplicated option list from raw `(tag, meta_name)`
/// pairs. A pure function over parsed data — no filesystem access — so it is fully
/// unit-testable.
///
/// First occurrence of a tag wins (disk entries precede embedded ones), an empty
/// tag is skipped, and a missing / blank `_meta.name` falls back to the tag. The
/// result is sorted by tag so ordering is stable regardless of scan order.
#[must_use]
fn build_locale_options(pairs: Vec<(String, Option<String>)>) -> Vec<LocaleOption> {
    let mut seen = std::collections::HashSet::new();
    let mut options = Vec::new();
    for (tag, meta_name) in pairs {
        let tag = tag.trim().to_string();
        if tag.is_empty() {
            continue;
        }
        if !seen.insert(tag.clone()) {
            continue;
        }
        let display = meta_name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| tag.clone());
        options.push(LocaleOption { tag, display });
    }
    options.sort_by(|a, b| a.tag.cmp(&b.tag));
    options
}

/// Embedded `(tag, meta_name)` locale pairs, parsed from the compiled-in catalogs.
/// Available on every target so the option list is never empty.
fn embedded_locale_pairs() -> Vec<(String, Option<String>)> {
    ms_i18n::embedded_locales()
        .iter()
        .map(|(tag, source)| ((*tag).to_string(), meta_name_from_json(source)))
        .collect()
}

/// Extracts `_meta.name` from a locale JSON source, or `None` if it is absent or
/// the source is unparseable (best-effort; the caller falls back to the tag).
fn meta_name_from_json(source: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(source).ok()?;
    value
        .get("_meta")
        .and_then(serde_json::Value::as_object)
        .and_then(|meta| meta.get("name"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Reads `(tag, meta_name)` pairs from the on-disk `locale/` directory (desktop).
///
/// Best-effort and off the per-frame path: a missing/unreadable directory or file
/// is logged and skipped, and the embedded fallback still yields `en`/`ru`.
#[cfg(not(target_arch = "wasm32"))]
fn disk_locale_pairs() -> Vec<(String, Option<String>)> {
    let dir = ms_config::data_dir().join("locale");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[general-settings] locale directory {} unavailable, using embedded list: {err}",
                dir.display()
            ));
            return Vec::new();
        }
    };
    let mut pairs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(tag) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let meta_name = match std::fs::read_to_string(&path) {
            Ok(raw) => meta_name_from_json(&raw),
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[general-settings] could not read locale file {}: {err}",
                    path.display()
                ));
                None
            }
        };
        pairs.push((tag.to_string(), meta_name));
    }
    pairs
}

/// Web twin of [`disk_locale_pairs`]: no on-disk `locale/` directory on wasm, so
/// the embedded list is used alone.
#[cfg(target_arch = "wasm32")]
fn disk_locale_pairs() -> Vec<(String, Option<String>)> {
    Vec::new()
}

/// Renders the status line beneath the projects-directory editor.
fn draw_status(ui: &mut egui::Ui, status: &GeneralSettingsStatus) {
    match status {
        GeneralSettingsStatus::Idle => {
            ui.small(t!("settings.general.projects_dir_empty_hint"));
        }
        GeneralSettingsStatus::Info(message) => {
            ui.small(message);
        }
        GeneralSettingsStatus::Success(message) => {
            ui.colored_label(ms_theme::status::SUCCESS, message);
        }
        GeneralSettingsStatus::Error(message) => {
            ui.colored_label(ms_theme::status::ERROR, message);
        }
    }
}

/// Clears a `Success` status back to `Idle` (called when the user edits the field so
/// a stale "saved" confirmation does not linger).
fn clear_success_status(state: &mut GeneralSettingsPanelState) {
    if matches!(state.status, GeneralSettingsStatus::Success(_)) {
        state.status = GeneralSettingsStatus::Idle;
    }
}

/// Opens the native OS folder picker and stores the chosen (normalized) projects root
/// in the input field. Desktop-only (no OS directory dialog on web).
#[cfg(not(target_arch = "wasm32"))]
fn pick_projects_dir(state: &mut GeneralSettingsPanelState) {
    let current = normalize_projects_dir_value(&state.projects_dir_input);
    let start_dir = if std::path::Path::new(&current).is_dir() {
        PathBuf::from(current)
    } else {
        ms_config::default_projects_root()
    };
    let Some(selected_dir) = rfd::FileDialog::new()
        .set_directory(start_dir)
        .pick_folder()
    else {
        return;
    };
    state.projects_dir_input = normalize_projects_dir_value(&selected_dir.to_string_lossy());
    state.status =
        GeneralSettingsStatus::Info(t!("settings.general.projects_dir_picked_hint").to_string());
}

/// Whether the normalized input differs from the last saved projects root (drives the
/// save button's enabled state).
fn projects_dir_is_dirty(input: &str, saved: &str) -> bool {
    normalize_projects_dir_value(input) != saved
}

/// Normalizes a raw projects-dir field value: trims whitespace; an empty value
/// resolves to the default projects root (lossy string), otherwise the trimmed path
/// is passed through a `PathBuf` (lossy string).
fn normalize_projects_dir_value(raw_value: &str) -> String {
    let trimmed = raw_value.trim();
    if trimmed.is_empty() {
        return ms_config::default_projects_root()
            .to_string_lossy()
            .into_owned();
    }
    PathBuf::from(trimmed).to_string_lossy().into_owned()
}

/// Synchronously persists several `<section>.<key>` values in `user_config.json` in one
/// serialized read-modify-write through [`ms_config::update_user_config_file`] (the
/// single user-config transaction owner: process-wide lock + atomic write), so it never
/// clobbers the ORT load-guard marker or a concurrent writer's keys.
///
/// `entries` is a list of `(section, key, value)`: each `value` is inserted at
/// `root[section][key]`, creating the section object if absent (a non-object section is
/// replaced by an object) and preserving every unrelated key. A missing file starts from
/// an empty object; all entries land in exactly one write. A malformed file is surfaced
/// as the localized parse error and is NEVER overwritten. Returns a user-facing error
/// string on failure.
///
/// Runs on the GUI thread (explicit user action, one tiny write). That is a known
/// CLAUDE.md §5 exception kept as-is in this step; the write may briefly wait behind
/// another user-config writer holding the transaction lock.
///
/// `pub` so the launcher's first-run language modal persists both language keys
/// (`General.ui_language` + `TextTab.text_language`) atomically through this one path.
pub fn persist_config_keys(entries: &[(&str, &str, serde_json::Value)]) -> Result<(), String> {
    persist_config_keys_at(&ms_config::user_config_path(), entries)
}

/// [`persist_config_keys`] against an explicit user-config `path` (tests use a temp file).
fn persist_config_keys_at(path: &std::path::Path, entries: &[(&str, &str, serde_json::Value)]) -> Result<(), String> {
    use serde_json::{Map, Value};

    let result = ms_config::update_user_config_file(path, |root| {
        // Same repair as before the docstore migration: a parseable but non-object root
        // (e.g. `[]`) is replaced by an empty object instead of failing the save.
        if !root.is_object() {
            *root = Value::Object(Map::new());
        }
        let Some(root_obj) = root.as_object_mut() else {
            return Err(anyhow::anyhow!(t!("settings.general.config_root_error").to_string()));
        };
        for (section, key, value) in entries {
            let mut section_obj = root_obj
                .get(*section)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            section_obj.insert((*key).to_string(), value.clone());
            root_obj.insert((*section).to_string(), Value::Object(section_obj));
        }
        Ok(())
    });
    result.map_err(|err| {
        let message = config_write_error_message(path, &err);
        runtime_log::log_error(format!(
            "[general-settings] failed to persist user config keys; path={}; keys={:?}; error={err:#}",
            path.display(),
            entries.iter().map(|(section, key, _)| format!("{section}.{key}")).collect::<Vec<_>>()
        ));
        message
    })
}

/// Maps a failed user-config transaction to the user-facing message.
///
/// A parse failure (the file exists but is not valid JSON — reported either as a
/// `serde_json::Error` or as `ms_docstore::DocStoreError::Malformed` somewhere in the
/// error chain) keeps the localized `config_parse_error` text; every other failure
/// (read, directory creation, write) is shown as its full context chain.
fn config_write_error_message(path: &std::path::Path, err: &anyhow::Error) -> String {
    let parse_cause = err.chain().find_map(|cause| {
        if let Some(json_err) = cause.downcast_ref::<serde_json::Error>() {
            return Some(json_err.to_string());
        }
        if let Some(ms_docstore::DocStoreError::Malformed { cause, .. }) = cause.downcast_ref::<ms_docstore::DocStoreError>() {
            return Some(cause.clone());
        }
        None
    });
    match parse_cause {
        Some(cause) => tf!("settings.general.config_parse_error", path = path.display(), err = cause),
        None => format!("{err:#}"),
    }
}

/// Thin wrapper over [`persist_config_keys`] for a single `General.<key>` write.
fn persist_general_key(key: &str, value: serde_json::Value) -> Result<(), String> {
    persist_config_keys(&[("General", key, value)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp path for one test's user config; the caller removes its directory.
    fn temp_config_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("ms-settings-ui-p2a-{tag}-{}", std::process::id()))
            .join("user_config.json")
    }

    #[test]
    fn persist_config_keys_preserves_unrelated_keys_and_creates_sections() {
        let path = temp_config_path("merge");
        let dir = path.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(&path, r#"{"General":{"keep":1,"ui_language":"ru"},"Other":{"x":true}}"#).expect("seed config");

        persist_config_keys_at(&path, &[
            ("General", "ui_language", serde_json::json!("en")),
            ("TextTab", "text_language", serde_json::json!("en-US")),
        ])
        .expect("persist");

        let root: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("parse back");
        assert_eq!(root, serde_json::json!({
            "General": {"keep": 1, "ui_language": "en"},
            "Other": {"x": true},
            "TextTab": {"text_language": "en-US"},
        }));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn persist_config_keys_never_overwrites_malformed_config() {
        let path = temp_config_path("malformed");
        let dir = path.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let broken = "{\"General\": ";
        std::fs::write(&path, broken).expect("seed broken config");

        let result = persist_config_keys_at(&path, &[("General", "ui_language", serde_json::json!("en"))]);

        assert!(result.is_err(), "a malformed config must fail the save");
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), broken);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn normalize_empty_and_whitespace_use_default_root() {
        let default = ms_config::default_projects_root()
            .to_string_lossy()
            .into_owned();
        assert_eq!(normalize_projects_dir_value(""), default);
        assert_eq!(normalize_projects_dir_value("   "), default);
        assert_eq!(normalize_projects_dir_value("\t \n"), default);
    }

    #[test]
    fn normalize_trims_and_passes_through_a_real_path() {
        // A concrete path is trimmed and preserved (round-trips through PathBuf lossy,
        // so this stays valid on both Linux and Windows string forms).
        let expected = PathBuf::from("/tmp/my_projects")
            .to_string_lossy()
            .into_owned();
        assert_eq!(normalize_projects_dir_value("  /tmp/my_projects  "), expected);
    }

    #[test]
    fn ui_scale_seed_sets_process_global_and_clamps() {
        seed_ui_scale_from_user_settings(&serde_json::json!({"General": {"ui_scale_percent": 90}}));
        assert_eq!(ui_scale_percent(), 90);
        // A hand-edited absurd value is clamped, never stored as-is.
        seed_ui_scale_from_user_settings(
            &serde_json::json!({"General": {"ui_scale_percent": 10_000}}),
        );
        assert_eq!(ui_scale_percent(), ms_config::UI_SCALE_PERCENT_MAX);
        // Restore the default so the shared global does not leak into other tests.
        seed_ui_scale_from_user_settings(&serde_json::json!({}));
        assert_eq!(ui_scale_percent(), ms_config::UI_SCALE_PERCENT_DEFAULT);
    }

    #[test]
    fn outcome_default_is_empty() {
        let outcome = GeneralSettingsOutcome::default();
        assert!(outcome.projects_dir_saved.is_none());
        assert!(outcome.memory_profile_changed.is_none());
    }

    #[test]
    fn build_locale_options_uses_meta_name_and_falls_back_to_tag() {
        let options = build_locale_options(vec![
            ("de".to_string(), Some("Deutsch".to_string())),
            ("en".to_string(), None),
        ]);
        // A custom tag appears with its `_meta.name`.
        let de = options.iter().find(|o| o.tag == "de").expect("de present");
        assert_eq!(de.display, "Deutsch");
        // A missing `_meta.name` falls back to the tag itself.
        let en = options.iter().find(|o| o.tag == "en").expect("en present");
        assert_eq!(en.display, "en");
    }

    #[test]
    fn build_locale_options_blank_meta_falls_back_and_empty_tag_skipped() {
        let options = build_locale_options(vec![
            ("fr".to_string(), Some("   ".to_string())),
            ("".to_string(), Some("Nameless".to_string())),
        ]);
        // The empty-tag entry is dropped; the blank name falls back to the tag.
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].tag, "fr");
        assert_eq!(options[0].display, "fr");
    }

    #[test]
    fn build_locale_options_is_deterministic_and_first_wins() {
        let a = build_locale_options(vec![
            ("ru".to_string(), Some("Русский".to_string())),
            ("en".to_string(), Some("English".to_string())),
            ("en".to_string(), Some("SHOULD-BE-IGNORED".to_string())),
        ]);
        // Sorted by tag regardless of input order.
        let tags: Vec<&str> = a.iter().map(|o| o.tag.as_str()).collect();
        assert_eq!(tags, vec!["en", "ru"]);
        // First occurrence of a duplicate tag wins.
        let en = a.iter().find(|o| o.tag == "en").expect("en present");
        assert_eq!(en.display, "English");
        // A different input order yields an identical result.
        let b = build_locale_options(vec![
            ("en".to_string(), Some("English".to_string())),
            ("ru".to_string(), Some("Русский".to_string())),
        ]);
        assert_eq!(a, b);
    }

    #[test]
    fn dirty_check_compares_normalized_input_to_saved() {
        let saved = PathBuf::from("/tmp/projects").to_string_lossy().into_owned();
        // Same path with surrounding whitespace is NOT dirty after normalization.
        assert!(!projects_dir_is_dirty("  /tmp/projects  ", &saved));
        // A different path is dirty.
        assert!(projects_dir_is_dirty("/tmp/other", &saved));
    }
}
