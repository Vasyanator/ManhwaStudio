/*
FILE OVERVIEW: crates/ms-settings-ui/src/settings_warnings/model.rs
GUI-free, wasm-clean data model of the settings warnings (no egui, no I/O).

Purpose:
Names every checked setting, where it is drawn (section + reserved group level), why it is
flagged, how severe that is, and aggregates the current warnings per item, section and
overall. The worker (`checks.rs` / `runtime.rs`) fills a `WarningSet`; the panes and the
launcher only read it.

Key items:
- `SettingKey` + `SettingLocation` / `SettingGroupId`: the checked items and their place.
  `NATIVE_FAMILY` / `REGISTRATION_FAMILY`: keys evaluated together.
- `WarningLevel`, `WarningReason` (+ `FallbackCause`, `RegistrationRecord`,
  `RegistrationProblem`), `SettingWarning`: one flagged fact.
  The level has ONE owner, `WarningReason::level()`; the text is localized at draw time by
  `WarningReason::message()`, so a UI-language switch re-renders it.
- `SettingChange` + `affected_keys()`: what a pane reports after a write landed, and which
  keys that re-evaluates.
- `WarningSet`: per-key storage and the aggregation (`item_level`, `section_level`,
  `overall_level`, all through one `worst_where`).

Notes:
`FallbackCause` mirrors `ms_native_runtime::NativeFallbackReason`, and `RegistrationRecord` /
`RegistrationProblem` mirror `ms_os_integration::report::{RecordKind, Defect}`; both owners
are native-only. `SettingKey::ALL` and `REGISTRATION_FAMILY` are platform-filtered: the
registration keys exist on Windows (three) and Linux (two, no installed-programs entry), and
never on macOS or the web build, which therefore never request them.
*/

use std::collections::BTreeMap;

use crate::settings_shared::SettingsSectionId;

/// A setting that the warnings worker checks. Each key is drawn as exactly one item in a
/// shared settings pane (see [`SettingKey::location`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingKey {
    /// `General.projects_dir`: the projects folder.
    ProjectsRoot,
    /// `General.ui_language`: the interface language catalog.
    UiLanguage,
    /// `General.ai_runtime`: native ONNX vs. the Python backend.
    AiRuntime,
    /// `ai_backend_autostart`: whether the backend is started with the app.
    BackendAutostart,
    /// `General.ai_onnx_build`: the native ONNX Runtime build.
    OnnxBuild,
    /// `General.ai_onnx_provider`: the native execution provider.
    OnnxProvider,
    /// `General.ai_onnx_device_id`: the native accelerator device.
    OnnxDevice,
    /// `General.ort_load_state`: the ORT crash guard of the effective selection.
    OrtCrashGuard,
    /// The OS record that starts this copy from the menu: the Start-menu `.lnk` (Windows)
    /// or the `.desktop` application entry (Linux).
    RegistrationStartMenu,
    /// The installed-programs (Uninstall) entry and the App Paths entry (Windows only).
    RegistrationProgramEntry,
    /// The "Open with" registration for images: the `Applications\…` key (Windows) or the
    /// `.desktop` entry's `MimeType=` / file-accepting `Exec=` (Linux).
    RegistrationOpenWith,
}

/// The ONNX keys that come from one native selection evaluation and are therefore
/// re-evaluated together.
pub const NATIVE_FAMILY: [SettingKey; 4] =
    [SettingKey::OnnxBuild, SettingKey::OnnxProvider, SettingKey::OnnxDevice, SettingKey::OrtCrashGuard];

/// The system-registration keys of this platform: they come from ONE probe of the OS
/// records and are therefore re-evaluated together. Windows: all three; Linux: no
/// installed-programs entry exists; elsewhere (macOS, web) none.
#[cfg(target_os = "windows")]
pub const REGISTRATION_FAMILY: &[SettingKey] =
    &[SettingKey::RegistrationStartMenu, SettingKey::RegistrationProgramEntry, SettingKey::RegistrationOpenWith];
/// The system-registration keys of this platform (see the Windows declaration).
#[cfg(target_os = "linux")]
pub const REGISTRATION_FAMILY: &[SettingKey] = &[SettingKey::RegistrationStartMenu, SettingKey::RegistrationOpenWith];
/// The system-registration keys of this platform (see the Windows declaration).
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub const REGISTRATION_FAMILY: &[SettingKey] = &[];

/// The keys checked on every platform, in declaration order.
const PORTABLE_KEYS: [SettingKey; 8] = [
    SettingKey::ProjectsRoot,
    SettingKey::UiLanguage,
    SettingKey::AiRuntime,
    SettingKey::BackendAutostart,
    SettingKey::OnnxBuild,
    SettingKey::OnnxProvider,
    SettingKey::OnnxDevice,
    SettingKey::OrtCrashGuard,
];

/// Length of [`SettingKey::ALL`] on this platform.
const ALL_LEN: usize = PORTABLE_KEYS.len() + REGISTRATION_FAMILY.len();

/// [`PORTABLE_KEYS`] followed by [`REGISTRATION_FAMILY`], built at compile time so the
/// platform filter has one owner (the family) instead of one full list per platform. Index
/// loops because iterators are not available in a const initializer; every index is bounded
/// by its source length and `ALL_LEN` is their sum.
const ALL_KEYS: [SettingKey; ALL_LEN] = {
    let mut keys = [SettingKey::ProjectsRoot; ALL_LEN];
    let mut index = 0;
    while index < PORTABLE_KEYS.len() {
        keys[index] = PORTABLE_KEYS[index];
        index += 1;
    }
    let mut family_index = 0;
    while family_index < REGISTRATION_FAMILY.len() {
        keys[PORTABLE_KEYS.len() + family_index] = REGISTRATION_FAMILY[family_index];
        family_index += 1;
    }
    keys
};

impl SettingKey {
    /// Every key checked on this platform, in declaration order (the full run at launcher
    /// entry). The registration keys are included only where [`REGISTRATION_FAMILY`] has
    /// them.
    pub const ALL: &'static [SettingKey] = &ALL_KEYS;

    /// Where the key's item is drawn. Every key lives in a section the launcher always
    /// lists on the platforms that check it (General, AiBackend, and SystemRegistration on
    /// Windows / Linux), so a badge never propagates from a hidden tab; no key belongs to a
    /// group yet.
    #[must_use]
    pub const fn location(self) -> SettingLocation {
        let section = match self {
            SettingKey::ProjectsRoot | SettingKey::UiLanguage => SettingsSectionId::General,
            SettingKey::AiRuntime
            | SettingKey::BackendAutostart
            | SettingKey::OnnxBuild
            | SettingKey::OnnxProvider
            | SettingKey::OnnxDevice
            | SettingKey::OrtCrashGuard => SettingsSectionId::AiBackend,
            SettingKey::RegistrationStartMenu
            | SettingKey::RegistrationProgramEntry
            | SettingKey::RegistrationOpenWith => SettingsSectionId::SystemRegistration,
        };
        SettingLocation { section, group: None }
    }
}

/// Where an item sits: its settings section and, optionally, a collapsible group inside
/// that section. `group` is the reserved level for future collapsible lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SettingLocation {
    /// The tab the item is drawn on.
    pub section: SettingsSectionId,
    /// The collapsible group inside the section; `None` for a top-level item.
    pub group: Option<SettingGroupId>,
}

/// Collapsible groups inside a section. No group exists yet (the enum is uninhabited):
/// adding one means adding a variant and mapping keys to it in [`SettingKey::location`];
/// the aggregation in [`WarningSet`] needs no change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingGroupId {}

/// Severity of a warning. Declaration order makes `Red > Yellow`, so `max` picks the
/// worst one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WarningLevel {
    /// The setting works, but not as the user probably expects.
    Yellow,
    /// The setting cannot work as configured.
    Red,
}

/// Why the native runtime runs the CPU build instead of the configured selection. A
/// wasm-clean mirror of `ms_native_runtime::NativeFallbackReason` (native-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackCause {
    /// The build has no runnable execution provider.
    NoRunnableProvider,
    /// No matching CUDA runtime was found.
    CudaRuntimeMissing,
    /// No Intel device / OpenVINO runtime was found.
    OpenVinoRuntimeMissing,
    /// No WebGPU-capable GPU was found.
    WebGpuUnavailable,
    /// The execution provider cannot run on this OS.
    UnsupportedOnPlatform,
    /// The build ships no archive for this OS/architecture.
    BuildNotShipped,
}

impl FallbackCause {
    /// The localized reason fragment substituted into the fallback tooltip.
    fn label(self) -> &'static str {
        match self {
            FallbackCause::NoRunnableProvider => t!("settings.warnings.fallback_reason.no_provider_label"),
            FallbackCause::CudaRuntimeMissing => t!("settings.warnings.fallback_reason.cuda_missing_label"),
            FallbackCause::OpenVinoRuntimeMissing => t!("settings.warnings.fallback_reason.openvino_missing_label"),
            FallbackCause::WebGpuUnavailable => t!("settings.warnings.fallback_reason.webgpu_missing_label"),
            FallbackCause::UnsupportedOnPlatform => t!("settings.warnings.fallback_reason.unsupported_os_label"),
            FallbackCause::BuildNotShipped => t!("settings.warnings.fallback_reason.not_shipped_label"),
        }
    }
}

/// Which OS record a registration warning is about. A wasm-clean mirror of
/// `ms_os_integration::report::RecordKind` (native-only); App Paths keeps its own name although
/// it is filed under [`SettingKey::RegistrationProgramEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationRecord {
    /// The Start-menu shortcut (Windows) / application-menu entry (Linux).
    StartMenu,
    /// The installed-programs entry (Windows).
    ProgramEntry,
    /// The App Paths entry (Windows).
    AppPaths,
    /// The "Open with" registration for images.
    OpenWith,
}

impl RegistrationRecord {
    /// The localized record name. The keys are shared with the launcher's System
    /// registration tab, which titles its rows with them.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            // The record is a `.lnk` in the Start menu on Windows and a `.desktop`
            // application-menu entry on Linux: the name follows what the user sees.
            #[cfg(target_os = "windows")]
            RegistrationRecord::StartMenu => t!("launcher.sysreg.start_menu_title"),
            #[cfg(not(target_os = "windows"))]
            RegistrationRecord::StartMenu => t!("launcher.sysreg.app_menu_title"),
            RegistrationRecord::ProgramEntry => t!("launcher.sysreg.program_entry_title"),
            RegistrationRecord::AppPaths => t!("launcher.sysreg.app_paths_title"),
            RegistrationRecord::OpenWith => t!("launcher.sysreg.open_with_title"),
        }
    }
}

/// Why a registration record does not work: a wasm-clean mirror of the BROKEN
/// `ms_os_integration::report::Defect`s (stale ones never badge, so they have no mirror).
/// Strings are display-ready paths and values; `name` is the technical value name
/// (`RecordValue::name`, never translated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationProblem {
    /// The program the record launches does not exist (also: a foreign record whose
    /// owning copy is gone).
    TargetMissing { path: String },
    /// The working directory the record starts in does not exist.
    WorkingDirMissing { path: String },
    /// A value the record cannot work without is absent.
    ValueMissing { name: &'static str },
    /// A launch-relevant value differs from what the owning copy writes.
    WrongValue { name: &'static str, expected: String, found: String },
    /// A command line that cannot be run.
    MalformedCommand { command: String },
}

impl RegistrationProblem {
    /// The localized fragment substituted into the registration tooltip. The keys are
    /// shared with the launcher's System registration tab (defect details).
    fn label(&self) -> String {
        match self {
            RegistrationProblem::TargetMissing { path } => tf!("launcher.sysreg.defect.target_missing_label", path = path),
            RegistrationProblem::WorkingDirMissing { path } => {
                tf!("launcher.sysreg.defect.workdir_missing_label", path = path)
            }
            RegistrationProblem::ValueMissing { name } => tf!("launcher.sysreg.defect.value_missing_label", name = name),
            RegistrationProblem::WrongValue { name, expected, found } => {
                tf!("launcher.sysreg.defect.wrong_value_label", name = name, expected = expected, found = found)
            }
            RegistrationProblem::MalformedCommand { command } => {
                tf!("launcher.sysreg.defect.bad_command_label", command = command)
            }
        }
    }
}

/// One flagged fact about a setting, with the data its message needs. Strings carry
/// display-ready values (paths, tags, slugs) captured by the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarningReason {
    /// The configured (non-default) projects folder does not exist.
    ProjectsRootMissing { path: String },
    /// The projects folder path exists but is not a directory.
    ProjectsRootNotDirectory { path: String },
    /// The on-disk catalog of the UI language does not load; the embedded one is used.
    UiCatalogEmbedded { tag: String, error: String },
    /// The UI language has neither a loadable on-disk nor an embedded catalog: English is shown.
    UiCatalogEnglish { tag: String },
    /// `ai_backend.py` is missing from the program directory.
    BackendScriptMissing { app_dir: String },
    /// The Python payload predates the document store (KG-017).
    BackendPayloadOutdated { app_dir: String },
    /// No Python environment resolves; `detail` is the resolver's message.
    BackendPythonMissing { detail: String },
    /// The EFFECTIVE native build has no archive for this OS/arch: native AI cannot load.
    OrtBuildNotShipped { build: String },
    /// The ORT crash guard of the effective selection is `Suspect`.
    OrtLoadCrashed,
    /// The configured selection cannot run here; the CPU build runs instead.
    NativeCpuFallback { reason: FallbackCause },
    /// The persisted device id is not among the devices the EP offers now.
    DeviceMissing { device: String, available: usize },
    /// An OS record of this copy (or one left by a copy that is gone) does not work.
    /// `location` is the registry key or file path; `problems` is never empty.
    RegistrationBroken { record: RegistrationRecord, location: String, problems: Vec<RegistrationProblem> },
}

impl WarningReason {
    /// The severity of this reason; the only place severity is decided.
    #[must_use]
    pub fn level(&self) -> WarningLevel {
        match self {
            WarningReason::BackendScriptMissing { .. }
            | WarningReason::BackendPayloadOutdated { .. }
            | WarningReason::BackendPythonMissing { .. }
            | WarningReason::OrtBuildNotShipped { .. }
            | WarningReason::OrtLoadCrashed => WarningLevel::Red,
            WarningReason::ProjectsRootMissing { .. }
            | WarningReason::ProjectsRootNotDirectory { .. }
            | WarningReason::UiCatalogEmbedded { .. }
            | WarningReason::UiCatalogEnglish { .. }
            | WarningReason::NativeCpuFallback { .. }
            | WarningReason::DeviceMissing { .. }
            | WarningReason::RegistrationBroken { .. } => WarningLevel::Yellow,
        }
    }

    /// The localized, user-facing explanation in the ACTIVE UI language. Call it at draw
    /// time (never cache it), so a language switch re-renders the text.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            WarningReason::ProjectsRootMissing { path } => {
                tf!("settings.warnings.projects_root_missing_tooltip", path = path)
            }
            WarningReason::ProjectsRootNotDirectory { path } => {
                tf!("settings.warnings.projects_root_not_dir_tooltip", path = path)
            }
            WarningReason::UiCatalogEmbedded { tag, error } => {
                tf!("settings.warnings.ui_catalog_embedded_tooltip", tag = tag, error = error)
            }
            WarningReason::UiCatalogEnglish { tag } => tf!("settings.warnings.ui_catalog_english_tooltip", tag = tag),
            WarningReason::BackendScriptMissing { app_dir } => {
                tf!("settings.warnings.backend_script_missing_tooltip", app_dir = app_dir)
            }
            WarningReason::BackendPayloadOutdated { app_dir } => {
                tf!("settings.warnings.backend_payload_outdated_tooltip", app_dir = app_dir)
            }
            WarningReason::BackendPythonMissing { detail } => tf!(
                "settings.warnings.backend_python_missing_tooltip",
                detail = detail,
                option = t!("ai_backend.runtime_native_option")
            ),
            WarningReason::OrtBuildNotShipped { build } => {
                tf!("settings.warnings.ort_build_not_shipped_tooltip", build = build)
            }
            WarningReason::OrtLoadCrashed => {
                tf!("settings.warnings.ort_load_crashed_tooltip", button = t!("ai_backend.ort_retry_button"))
            }
            WarningReason::NativeCpuFallback { reason } => {
                tf!("settings.warnings.native_cpu_fallback_tooltip", reason = reason.label())
            }
            WarningReason::DeviceMissing { device, available } => {
                tf!("settings.warnings.device_missing_tooltip", device = device, available = available)
            }
            WarningReason::RegistrationBroken { record, location, problems } => {
                let detail = problems.iter().map(RegistrationProblem::label).collect::<Vec<_>>().join("; ");
                tf!(
                    "settings.warnings.system_registration_broken_tooltip",
                    record = record.label(),
                    location = location,
                    detail = detail,
                    tab = t!("launcher.settings.tab_system_registration")
                )
            }
        }
    }
}

/// One warning attached to one setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingWarning {
    /// The flagged item.
    pub key: SettingKey,
    /// Why it is flagged; also decides the level.
    pub reason: WarningReason,
}

/// What a pane reports after a settings write LANDED (or, for synchronous writes, after
/// it returned). `BackendAutostart` carries the new value because the supervisor
/// persists it asynchronously, so a re-read of config could still see the old one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingChange {
    /// The projects folder was saved.
    ProjectsRoot,
    /// The UI language was saved (pane or first-run modal).
    UiLanguage,
    /// The AI runtime selection was saved.
    AiRuntime,
    /// The autostart toggle changed to the carried value.
    BackendAutostart(bool),
    /// The native build was saved.
    OnnxBuild,
    /// The native EP / device pair was saved.
    OnnxProviderDevice,
    /// The ORT crash guard was reset.
    OrtGuardReset,
    /// The AI install type changed (install / uninstall flow).
    AiInstallType,
    /// An action of the System registration tab finished (success, failure or a declined
    /// elevation): the OS records are probed again.
    SystemRegistration,
}

impl SettingChange {
    /// The keys whose checks must re-run after this change. A runtime change also
    /// re-evaluates the native family, because those items exist only under `Native`.
    #[must_use]
    pub fn affected_keys(self) -> &'static [SettingKey] {
        const AI_RUNTIME: [SettingKey; 5] = [
            SettingKey::AiRuntime,
            SettingKey::OnnxBuild,
            SettingKey::OnnxProvider,
            SettingKey::OnnxDevice,
            SettingKey::OrtCrashGuard,
        ];
        match self {
            SettingChange::ProjectsRoot => &[SettingKey::ProjectsRoot],
            SettingChange::UiLanguage => &[SettingKey::UiLanguage],
            SettingChange::AiRuntime => &AI_RUNTIME,
            SettingChange::BackendAutostart(_) => &[SettingKey::BackendAutostart],
            SettingChange::OnnxBuild | SettingChange::OnnxProviderDevice | SettingChange::OrtGuardReset => {
                &NATIVE_FAMILY
            }
            SettingChange::AiInstallType => &[SettingKey::AiRuntime, SettingKey::BackendAutostart],
            SettingChange::SystemRegistration => REGISTRATION_FAMILY,
        }
    }
}

/// The current warnings, keyed by setting. A key with no warning is absent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WarningSet {
    by_key: BTreeMap<SettingKey, Vec<SettingWarning>>,
}

impl WarningSet {
    /// Replaces every warning of `key` with `warnings` (an empty Vec clears the key).
    /// Every warning must belong to `key`; a mismatching one is a caller bug (debug
    /// assertion) and is filed under its own key in release builds.
    pub fn replace_key(&mut self, key: SettingKey, warnings: Vec<SettingWarning>) {
        debug_assert!(warnings.iter().all(|warning| warning.key == key), "warning filed under a foreign key");
        if warnings.is_empty() {
            self.by_key.remove(&key);
        } else {
            self.by_key.insert(key, warnings);
        }
    }

    /// The warnings of `key`, empty when it is clean.
    #[must_use]
    pub fn for_key(&self, key: SettingKey) -> &[SettingWarning] {
        self.by_key.get(&key).map_or(&[][..], Vec::as_slice)
    }

    /// The worst level of one item, `None` when it is clean.
    #[must_use]
    pub fn item_level(&self, key: SettingKey) -> Option<WarningLevel> {
        self.for_key(key).iter().map(|warning| warning.reason.level()).max()
    }

    /// The worst level over every item of `section` (any group), `None` when all are clean.
    #[must_use]
    pub fn section_level(&self, section: SettingsSectionId) -> Option<WarningLevel> {
        self.worst_where(|location| location.section == section)
    }

    /// The worst level over every item, `None` when everything is clean.
    #[must_use]
    pub fn overall_level(&self) -> Option<WarningLevel> {
        self.worst_where(|_| true)
    }

    /// Every message of `key`, localized now and joined by newlines (the hover text of
    /// an item badge); `None` when the key is clean.
    #[must_use]
    pub fn tooltip_text(&self, key: SettingKey) -> Option<String> {
        let warnings = self.for_key(key);
        if warnings.is_empty() {
            return None;
        }
        Some(warnings.iter().map(|warning| warning.reason.message()).collect::<Vec<_>>().join("\n"))
    }

    /// The one aggregation: the worst level over the keys whose location matches.
    fn worst_where(&self, matches: impl Fn(SettingLocation) -> bool) -> Option<WarningLevel> {
        self.by_key
            .iter()
            .filter(|(key, _)| matches(key.location()))
            .flat_map(|(_, warnings)| warnings.iter().map(|warning| warning.reason.level()))
            .max()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NATIVE_FAMILY, REGISTRATION_FAMILY, RegistrationProblem, RegistrationRecord, SettingChange, SettingKey,
        SettingWarning, WarningLevel, WarningReason, WarningSet,
    };
    use crate::settings_shared::{SettingsSectionId, SettingsSurface, sections_for};

    fn yellow(key: SettingKey) -> SettingWarning {
        SettingWarning { key, reason: WarningReason::UiCatalogEnglish { tag: "de".to_string() } }
    }

    fn red(key: SettingKey) -> SettingWarning {
        SettingWarning { key, reason: WarningReason::OrtLoadCrashed }
    }

    #[test]
    fn empty_set_is_clean_everywhere() {
        let set = WarningSet::default();
        for &key in SettingKey::ALL {
            assert_eq!(set.item_level(key), None);
            assert!(set.for_key(key).is_empty());
            assert_eq!(set.tooltip_text(key), None);
        }
        assert_eq!(set.section_level(SettingsSectionId::General), None);
        assert_eq!(set.section_level(SettingsSectionId::AiBackend), None);
        assert_eq!(set.overall_level(), None);
    }

    #[test]
    fn red_wins_per_item_section_and_overall() {
        let mut set = WarningSet::default();
        set.replace_key(SettingKey::OnnxBuild, vec![yellow(SettingKey::OnnxBuild), red(SettingKey::OnnxBuild)]);
        set.replace_key(SettingKey::OnnxDevice, vec![yellow(SettingKey::OnnxDevice)]);
        set.replace_key(SettingKey::ProjectsRoot, vec![yellow(SettingKey::ProjectsRoot)]);
        assert_eq!(set.item_level(SettingKey::OnnxBuild), Some(WarningLevel::Red));
        assert_eq!(set.item_level(SettingKey::OnnxDevice), Some(WarningLevel::Yellow));
        assert_eq!(set.item_level(SettingKey::UiLanguage), None);
        assert_eq!(set.section_level(SettingsSectionId::AiBackend), Some(WarningLevel::Red));
        assert_eq!(set.section_level(SettingsSectionId::General), Some(WarningLevel::Yellow));
        assert_eq!(set.section_level(SettingsSectionId::SystemInfo), None);
        assert_eq!(set.overall_level(), Some(WarningLevel::Red));
    }

    #[test]
    fn replace_with_empty_clears_the_key() {
        let mut set = WarningSet::default();
        set.replace_key(SettingKey::OrtCrashGuard, vec![red(SettingKey::OrtCrashGuard)]);
        assert_eq!(set.overall_level(), Some(WarningLevel::Red));
        set.replace_key(SettingKey::OrtCrashGuard, Vec::new());
        assert_eq!(set.overall_level(), None);
        assert_eq!(set, WarningSet::default());
    }

    #[test]
    fn tooltip_joins_every_message_of_the_key() {
        let mut set = WarningSet::default();
        set.replace_key(SettingKey::OnnxBuild, vec![yellow(SettingKey::OnnxBuild), red(SettingKey::OnnxBuild)]);
        let text = set.tooltip_text(SettingKey::OnnxBuild).unwrap_or_default();
        assert_eq!(text.lines().count(), 2, "{text}");
    }

    /// Every key checked on this platform must live in a section the launcher lists here
    /// and never hides (General, AiBackend, SystemRegistration — never TorchUpgrade, which
    /// hides with the AI install type), and no key uses the reserved group level.
    /// SystemRegistration joined the set deliberately (system-registration plan, F3): its
    /// keys exist only where its row is listed.
    #[test]
    fn every_key_maps_to_an_always_listed_launcher_section() {
        let launcher: Vec<SettingsSectionId> =
            sections_for(SettingsSurface::Launcher).iter().map(|descriptor| descriptor.id).collect();
        for &key in SettingKey::ALL {
            let location = key.location();
            assert!(launcher.contains(&location.section), "{key:?}");
            assert!(
                matches!(
                    location.section,
                    SettingsSectionId::General | SettingsSectionId::AiBackend | SettingsSectionId::SystemRegistration
                ),
                "{key:?} must stay in General, AiBackend or SystemRegistration"
            );
            assert_eq!(location.group, None, "{key:?}");
        }
    }

    #[test]
    fn affected_keys_follow_the_recheck_table() {
        use SettingKey as K;
        assert_eq!(SettingChange::ProjectsRoot.affected_keys(), &[K::ProjectsRoot]);
        assert_eq!(SettingChange::UiLanguage.affected_keys(), &[K::UiLanguage]);
        let mut runtime = vec![K::AiRuntime];
        runtime.extend(NATIVE_FAMILY);
        assert_eq!(SettingChange::AiRuntime.affected_keys(), runtime.as_slice());
        assert_eq!(SettingChange::BackendAutostart(true).affected_keys(), &[K::BackendAutostart]);
        assert_eq!(SettingChange::BackendAutostart(false).affected_keys(), &[K::BackendAutostart]);
        for change in [SettingChange::OnnxBuild, SettingChange::OnnxProviderDevice, SettingChange::OrtGuardReset] {
            assert_eq!(change.affected_keys(), &NATIVE_FAMILY, "{change:?}");
        }
        assert_eq!(SettingChange::AiInstallType.affected_keys(), &[K::AiRuntime, K::BackendAutostart]);
        assert_eq!(SettingChange::SystemRegistration.affected_keys(), REGISTRATION_FAMILY);
    }

    /// The registration keys are platform-filtered: Windows checks three records, Linux two
    /// (no installed-programs entry), every other target none; `ALL` carries exactly the
    /// family after the portable keys, and every family key draws on SystemRegistration.
    #[test]
    fn registration_family_is_platform_filtered_and_part_of_all() {
        use SettingKey as K;
        #[cfg(target_os = "windows")]
        let expected: &[SettingKey] = &[K::RegistrationStartMenu, K::RegistrationProgramEntry, K::RegistrationOpenWith];
        #[cfg(target_os = "linux")]
        let expected: &[SettingKey] = &[K::RegistrationStartMenu, K::RegistrationOpenWith];
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        let expected: &[SettingKey] = &[];
        assert_eq!(REGISTRATION_FAMILY, expected);
        let portable = SettingKey::ALL.len() - REGISTRATION_FAMILY.len();
        assert_eq!(&SettingKey::ALL[portable..], REGISTRATION_FAMILY);
        assert!(SettingKey::ALL[..portable].iter().all(|key| !REGISTRATION_FAMILY.contains(key)));
        for &key in REGISTRATION_FAMILY {
            assert_eq!(key.location().section, SettingsSectionId::SystemRegistration, "{key:?}");
        }
    }

    /// A registration warning is Yellow (Q1 of the plan) and aggregates on the
    /// SystemRegistration tab only.
    #[test]
    fn registration_broken_is_yellow_on_its_tab() {
        let reason = WarningReason::RegistrationBroken {
            record: RegistrationRecord::OpenWith,
            location: "/home/u/.local/share/applications/manhwastudio_rs.desktop".to_string(),
            problems: vec![
                RegistrationProblem::TargetMissing { path: "/home/u/gone/manhwastudio_rs".to_string() },
                RegistrationProblem::ValueMissing { name: "Exec" },
            ],
        };
        assert_eq!(reason.level(), WarningLevel::Yellow);
        let mut set = WarningSet::default();
        set.replace_key(SettingKey::RegistrationOpenWith, vec![SettingWarning { key: SettingKey::RegistrationOpenWith, reason }]);
        assert_eq!(set.section_level(SettingsSectionId::SystemRegistration), Some(WarningLevel::Yellow));
        assert_eq!(set.section_level(SettingsSectionId::General), None);
    }

    #[test]
    fn levels_are_owned_by_the_reason() {
        assert_eq!(WarningReason::OrtLoadCrashed.level(), WarningLevel::Red);
        assert_eq!(WarningReason::OrtBuildNotShipped { build: "cpu".to_string() }.level(), WarningLevel::Red);
        assert_eq!(WarningReason::BackendPythonMissing { detail: String::new() }.level(), WarningLevel::Red);
        assert_eq!(
            WarningReason::DeviceMissing { device: "3".to_string(), available: 1 }.level(),
            WarningLevel::Yellow
        );
        assert_eq!(WarningReason::ProjectsRootMissing { path: String::new() }.level(), WarningLevel::Yellow);
        assert!(WarningLevel::Red > WarningLevel::Yellow);
    }
}
