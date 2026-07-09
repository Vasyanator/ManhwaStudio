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

Persistence is SYNCHRONOUS and serialized through the process-wide
`config::lock_user_config_write()` so a write never clobbers the ONNX Runtime SIGILL
load-guard marker (see `README_AGENT`'s user_config write-lock invariant).

The UI-language selector lists the locales found in the on-disk `locale/` folder
(scanned ONCE at construction — never per frame; see CLAUDE.md §5), each shown by
its `_meta.name`. Changing it persists `General.ui_language` and live-installs that
locale's catalog (falling back to the embedded catalog), with no restart.

Key items:
- `GeneralSettingsPanelState`: per-UI scratch + mirrored persisted values.
- `GeneralSettingsOutcome`: per-call-site runtime effects to apply after drawing.
- `LocaleOption`: one selectable interface language (tag + display name).
- `build_locale_options`: pure, filesystem-free option builder (deterministic).
- `draw_general_settings_panel`: renders the projects-dir editor + memory-profile
  combo + UI-language selector.
*/

use crate::memory_manager::MemoryProfile;
use crate::runtime_log;
use crate::widgets::WheelComboBox;
use std::path::PathBuf;

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
    /// Status line under the projects-dir editor.
    pub status: GeneralSettingsStatus,
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
            match crate::config::load_raw_user_settings_for_startup() {
                Ok(settings) => (
                    crate::config::projects_root_from_user_settings(&settings)
                        .to_string_lossy()
                        .into_owned(),
                    crate::config::memory_profile_from_user_settings(&settings),
                    ui_language_tag_from_settings(&settings),
                ),
                Err(err) => {
                    runtime_log::log_error(format!(
                        "[general-settings] failed to read user settings for seeding; using \
                         defaults; error={err:#}"
                    ));
                    (
                        crate::config::default_projects_root()
                            .to_string_lossy()
                            .into_owned(),
                        MemoryProfile::default(),
                        DEFAULT_UI_LANGUAGE_TAG.to_string(),
                    )
                }
            };
        Self {
            projects_dir_input: projects_dir.clone(),
            saved_projects_dir: projects_dir,
            memory_profile,
            ui_language_tag,
            // Filesystem scan happens once here, at construction — never per frame.
            locale_options: scan_locale_options(),
            status: GeneralSettingsStatus::Idle,
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
/// Persists a changed projects dir / memory profile synchronously (serialized on
/// `config::lock_user_config_write()`); persistence failures set an error status and
/// are logged. The native folder picker button is desktop-only.
#[must_use]
pub fn draw_general_settings_panel(
    ui: &mut egui::Ui,
    state: &mut GeneralSettingsPanelState,
) -> GeneralSettingsOutcome {
    let mut outcome = GeneralSettingsOutcome::default();

    // Projects-directory editor (rich variant: text field + folder picker + save).
    ui.label("Папка с проектами:");
    let mut should_save = false;
    ui.horizontal_wrapped(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(&mut state.projects_dir_input)
                .desired_width(420.0)
                .hint_text("Выберите папку с проектами"),
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
        if ui.button("Обзор").clicked() {
            pick_projects_dir(state);
        }
    });

    ui.small(
        "Используется страницами «Открыть главу», «Импорт главы», «Экспорт главы», а также окнами \
         «Новый проект» и PSD-импорт.",
    );

    draw_status(ui, &state.status);

    let dirty = projects_dir_is_dirty(&state.projects_dir_input, &state.saved_projects_dir);
    if ui
        .add_enabled(dirty, egui::Button::new("Сохранить папку проектов"))
        .clicked()
    {
        should_save = true;
    }

    if should_save {
        let normalized = normalize_projects_dir_value(&state.projects_dir_input);
        match persist_general_key(
            crate::config::GENERAL_PROJECTS_DIR_KEY,
            serde_json::Value::String(normalized.clone()),
        ) {
            Ok(()) => {
                state.saved_projects_dir = normalized.clone();
                state.projects_dir_input = normalized.clone();
                state.status =
                    GeneralSettingsStatus::Success("Папка проектов сохранена.".to_string());
                outcome.projects_dir_saved = Some(PathBuf::from(normalized));
            }
            Err(err) => {
                runtime_log::log_error(format!(
                    "[general-settings] failed to persist projects directory '{normalized}'; \
                     error={err}"
                ));
                state.status = GeneralSettingsStatus::Error(format!(
                    "Не удалось сохранить папку проектов: {err}"
                ));
            }
        }
    }

    ui.separator();

    // Global memory-profile selector (applied to the runtime by the caller).
    ui.label("Использование памяти:");
    ui.small("Применяется сразу к общей политике кэшей изображений.");
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
            crate::config::GENERAL_MEMORY_PROFILE_KEY,
            serde_json::Value::String(selected_profile.as_config_str().to_string()),
        ) {
            runtime_log::log_error(format!(
                "[general-settings] failed to persist memory profile '{}'; error={err}",
                selected_profile.as_config_str()
            ));
            state.status =
                GeneralSettingsStatus::Error("Не удалось сохранить профиль памяти.".to_string());
        }
    }

    ui.separator();

    // Interface-language selector. Populated once from the on-disk `locale/` folder
    // (see `scan_locale_options`); changing it persists and live-installs the locale.
    ui.label("Язык интерфейса:");
    ui.small("Применяется сразу, без перезапуска.");
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
        WheelComboBox::from_label("Язык интерфейса")
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

    outcome
}

/// Persists the newly selected UI-language tag and live-installs its catalog.
///
/// Persistence is synchronous, matching this widget's projects-dir / memory-profile
/// writes: one tiny key write on an explicit user action, serialized on the
/// `config::lock_user_config_write()` lock so it never clobbers the ORT load-guard
/// marker. The install is live (no restart) and the frame is repainted so the new
/// strings show immediately.
fn apply_ui_language_change(ui: &egui::Ui, state: &mut GeneralSettingsPanelState) {
    let tag = state.ui_language_tag.clone();
    if let Err(err) = persist_general_key(
        crate::config::GENERAL_UI_LANGUAGE_KEY,
        serde_json::Value::String(tag.clone()),
    ) {
        runtime_log::log_error(format!(
            "[general-settings] failed to persist ui language '{tag}'; error={err}"
        ));
        state.status =
            GeneralSettingsStatus::Error("Не удалось сохранить язык интерфейса.".to_string());
    }
    install_selected_ui_locale(&tag);
    ui.ctx().request_repaint();
}

/// Live-installs the selected locale on desktop: loads it from the on-disk
/// `locale/` folder (with embedded / English fallback) and installs it into the
/// `ms-i18n` runtime by reusing the startup install path.
#[cfg(not(target_arch = "wasm32"))]
fn install_selected_ui_locale(tag: &str) {
    // Hand the startup installer a minimal settings object carrying only the chosen
    // tag; it performs the disk-load + embedded/English fallback and the install.
    let settings =
        serde_json::json!({ "General": { crate::config::GENERAL_UI_LANGUAGE_KEY: tag } });
    crate::locale_store::install_ui_locale(&settings);
}

/// Web twin: no on-disk `locale/` folder on wasm, so install the embedded catalog
/// for the tag directly. An invalid tag / missing embedded catalog is logged and
/// the UI language is left unchanged (never a panic).
#[cfg(target_arch = "wasm32")]
fn install_selected_ui_locale(tag: &str) {
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
        .and_then(|general| general.get(crate::config::GENERAL_UI_LANGUAGE_KEY))
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
fn scan_locale_options() -> Vec<LocaleOption> {
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
    let dir = crate::config::data_dir().join("locale");
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
            ui.small("Если поле пустое, автоматически используется путь по умолчанию.");
        }
        GeneralSettingsStatus::Info(message) => {
            ui.small(message);
        }
        GeneralSettingsStatus::Success(message) => {
            ui.colored_label(egui::Color32::from_rgb(42, 168, 88), message);
        }
        GeneralSettingsStatus::Error(message) => {
            ui.colored_label(egui::Color32::from_rgb(208, 84, 62), message);
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
        crate::config::default_projects_root()
    };
    let Some(selected_dir) = rfd::FileDialog::new()
        .set_directory(start_dir)
        .pick_folder()
    else {
        return;
    };
    state.projects_dir_input = normalize_projects_dir_value(&selected_dir.to_string_lossy());
    state.status =
        GeneralSettingsStatus::Info("Папка выбрана. Нажмите «Сохранить папку проектов».".to_string());
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
        return crate::config::default_projects_root()
            .to_string_lossy()
            .into_owned();
    }
    PathBuf::from(trimmed).to_string_lossy().into_owned()
}

/// Synchronously persists a single `General.<key>` value in `user_config.json`,
/// serialized on the process-wide write lock so it never clobbers the ORT load-guard
/// marker (see `README_AGENT`'s user_config write-lock invariant).
///
/// Performs one targeted read-modify-write while holding
/// `config::lock_user_config_write()`: reads the current file (a missing file starts
/// from an empty object), sets `General.<key> = value`, and rewrites the file exactly
/// once, preserving every unrelated key. A parse error is surfaced rather than
/// silently resetting the config, so a temporarily unreadable file is never clobbered.
/// Returns a user-facing (Cyrillic) error string on failure. Synchronous disk I/O, but
/// a single tiny write triggered by an explicit user action.
fn persist_general_key(key: &str, value: serde_json::Value) -> Result<(), String> {
    use serde_json::{Map, Value};

    let _guard = crate::config::lock_user_config_write();
    let path = crate::config::user_config_path();

    let mut root = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<Value>(&raw)
            .map_err(|err| format!("не удалось разобрать {}: {err}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(err) => return Err(format!("не удалось прочитать {}: {err}", path.display())),
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let Some(root_obj) = root.as_object_mut() else {
        return Err("не удалось подготовить корень user_config.json".to_string());
    };
    let mut general = root_obj
        .get("General")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    general.insert(key.to_string(), value);
    root_obj.insert("General".to_string(), Value::Object(general));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    std::fs::write(&path, payload).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_empty_and_whitespace_use_default_root() {
        let default = crate::config::default_projects_root()
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
