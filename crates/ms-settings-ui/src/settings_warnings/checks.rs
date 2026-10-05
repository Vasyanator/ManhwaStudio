/*
FILE OVERVIEW: crates/ms-settings-ui/src/settings_warnings/checks.rs
Native-only (declared `cfg(not(target_arch = "wasm32"))` in `mod.rs`): the checks behind
every `SettingKey`, run on the settings-warnings worker thread.

Purpose:
Turns a fresh read of `user_config` plus the existing detectors into `WarningReason`s.
Split in two halves:
- fact gathering (`run_checks` and the `gather_*` / `check_*` helpers): one raw config
  read, `ms_native_runtime::evaluate_native_selection` (uncached, never touches the
  process selection cache), the guard scope of the next load
  (`ms_native_runtime::next_load_scope_key`), `probe_onnx_caps` only when the
  device check needs it, `check_backend_spawnable(program_dir)`,
  `locale_store::probe_disk_catalog` and a `stat` of the projects folder. Blocking:
  worker only. Untested glue (it probes real hardware and the filesystem);
- pure decisions (`projects_root_reasons`, `ui_catalog_reasons`, `backend_reasons`,
  `native_family_reasons`): plain data in, reasons out, unit-tested.

Notes:
Every rule here is a CONSUMER of its owner: build shipping is
`ms_onnx_runtime::build_shipped_here`, the CPU fallback is the native runtime's
`NativeSelectionReport::fallback`, whether a device id is used at all is the report's
effective `device`, whether the persisted id names an offered device is
`onnx_caps::device_id_offered` (over the runtime's own id parse), the guard scope is
`next_load_scope_key` (shared with the panel's Retry worker), spawnability is
`check_backend_spawnable`, the catalog rule is `probe_disk_catalog`. The worker never
reconciles the AI install type; it only reads it.
*/

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::Path;

use ms_config::locale_store;
use ms_config::{AiInstallType, AiRuntime, OrtLoadDecision};
use ms_log::runtime_log;
use ms_native_runtime::NativeFallbackReason;
use ms_onnx::NativeDeviceSelection;
use serde_json::Value;

use super::model::{FallbackCause, NATIVE_FAMILY, SettingKey, SettingWarning, WarningReason};
use super::runtime::{CheckContext, KeyOutcome};
use crate::ai_backend_supervisor::{BackendSpawnBlocker, check_backend_spawnable};
use crate::onnx_caps::{device_id_offered, ep_device_ids, probe_onnx_caps};

/// Runs the checks of `keys` against a fresh config read and returns one outcome per
/// requested key. A config read failure yields `Skipped` for every key (the previous
/// warnings stay) and a warning log. Blocking (disk, hardware probes): worker only.
pub(crate) fn run_checks(keys: &BTreeSet<SettingKey>, ctx: CheckContext) -> Vec<(SettingKey, KeyOutcome)> {
    let cfg = match ms_config::load_raw_user_settings_for_startup() {
        Ok(cfg) => cfg,
        Err(err) => {
            runtime_log::log_warn(format!(
                "settings-warnings: could not read user config; keeping the previous warnings of {keys:?}; error: {err:#}"
            ));
            return keys.iter().map(|key| (*key, KeyOutcome::Skipped)).collect();
        }
    };

    let mut outcomes = Vec::with_capacity(keys.len());
    if keys.contains(&SettingKey::ProjectsRoot) {
        outcomes.push((SettingKey::ProjectsRoot, check_projects_root(&cfg)));
    }
    if keys.contains(&SettingKey::UiLanguage) {
        let reasons = gather_ui_language(&cfg);
        outcomes.push((SettingKey::UiLanguage, checked(SettingKey::UiLanguage, reasons)));
    }

    let wants_runtime = keys.contains(&SettingKey::AiRuntime);
    let wants_autostart = keys.contains(&SettingKey::BackendAutostart);
    if wants_runtime || wants_autostart {
        let (runtime_reasons, autostart_reasons) = gather_backend(&cfg, ctx, wants_runtime, wants_autostart);
        if wants_runtime {
            outcomes.push((SettingKey::AiRuntime, checked(SettingKey::AiRuntime, runtime_reasons)));
        }
        if wants_autostart {
            outcomes.push((SettingKey::BackendAutostart, checked(SettingKey::BackendAutostart, autostart_reasons)));
        }
    }

    if NATIVE_FAMILY.iter().any(|key| keys.contains(key)) {
        for (key, reasons) in gather_native(&cfg, ctx) {
            if keys.contains(&key) {
                outcomes.push((key, checked(key, reasons)));
            }
        }
    }
    outcomes
}

/// Wraps `reasons` of `key` into a `Checked` outcome.
fn checked(key: SettingKey, reasons: Vec<WarningReason>) -> KeyOutcome {
    KeyOutcome::Checked(reasons.into_iter().map(|reason| SettingWarning { key, reason }).collect())
}

/// Stats the configured projects folder. An I/O error other than "not found" (e.g.
/// permission denied on a parent) leaves the answer unknown: `Skipped`, logged.
fn check_projects_root(cfg: &Value) -> KeyOutcome {
    let root = ms_config::projects_root_from_user_settings(cfg);
    let default_root = ms_config::default_projects_root();
    let (exists, is_dir) = match std::fs::metadata(&root) {
        Ok(metadata) => (true, metadata.is_dir()),
        Err(err) if err.kind() == ErrorKind::NotFound => (false, false),
        Err(err) => {
            runtime_log::log_warn(format!(
                "settings-warnings: could not stat the projects folder; path={}; error: {err}; keeping the previous warning",
                root.display()
            ));
            return KeyOutcome::Skipped;
        }
    };
    checked(SettingKey::ProjectsRoot, projects_root_reasons(&root, &default_root, exists, is_dir))
}

/// The projects-folder rule. A missing folder is flagged only when it is NOT the
/// default root: the defaults tree persists the default path and the folder is created
/// lazily on the first project, so a fresh install must stay clean (user decision Q1).
pub(crate) fn projects_root_reasons(root: &Path, default_root: &Path, exists: bool, is_dir: bool) -> Vec<WarningReason> {
    let path = root.display().to_string();
    if !exists {
        if root == default_root {
            return Vec::new();
        }
        return vec![WarningReason::ProjectsRootMissing { path }];
    }
    if is_dir { Vec::new() } else { vec![WarningReason::ProjectsRootNotDirectory { path }] }
}

/// Probes the on-disk catalog of the configured UI language by the rule
/// `install_ui_locale` applies.
fn gather_ui_language(cfg: &Value) -> Vec<WarningReason> {
    let tag = locale_store::ui_locale_tag_from_user_settings(cfg);
    let disk_error = locale_store::probe_disk_catalog(&tag).err().map(|err| err.to_string());
    // Same lookup `ms_i18n::set_locale` uses for the embedded fallback.
    let embedded = ms_i18n::embedded_locales().iter().any(|(embedded_tag, _)| *embedded_tag == tag.as_str());
    ui_catalog_reasons(tag.as_str(), disk_error, embedded)
}

/// The UI-catalog rule: a disk catalog that does not load falls back to the embedded
/// catalog of the same tag, or to English when none is embedded.
pub(crate) fn ui_catalog_reasons(tag: &str, disk_error: Option<String>, embedded: bool) -> Vec<WarningReason> {
    match disk_error {
        None => Vec::new(),
        Some(error) if embedded => vec![WarningReason::UiCatalogEmbedded { tag: tag.to_string(), error }],
        Some(_) => vec![WarningReason::UiCatalogEnglish { tag: tag.to_string() }],
    }
}

/// Reads the runtime and install type and runs the spawn preconditions only when a
/// requested check can use the answer.
fn gather_backend(
    cfg: &Value,
    ctx: CheckContext,
    wants_runtime: bool,
    wants_autostart: bool,
) -> (Vec<WarningReason>, Vec<WarningReason>) {
    let runtime = AiRuntime::from_user_settings(cfg);
    // Read only: the install-type owner (the launcher's reconciliation) is not run here.
    let install = AiInstallType::from_user_settings(cfg);
    let app_dir = ms_config::program_dir();
    let needs_spawn_check = ctx.ai_enabled
        && ((wants_runtime && runtime == AiRuntime::Backend) || (wants_autostart && ctx.backend_autostart));
    let blocker = if needs_spawn_check { check_backend_spawnable(&app_dir).err() } else { None };
    backend_reasons(runtime, ctx, install, blocker.as_ref(), &app_dir)
}

/// The backend rules, as `(AiRuntime reasons, BackendAutostart reasons)`.
///
/// - Nothing under `--no-ai` (`!ctx.ai_enabled`) or without a blocker.
/// - `AiRuntime`: the blocker, when the runtime is `Backend`.
/// - `BackendAutostart`: the blocker, when autostart is on — except a missing Python
///   while no AI is installed (user decision Q2: that state is the main page's "AI not
///   installed" notice, not a broken setting). An explicit `Backend` runtime stays Red.
pub(crate) fn backend_reasons(
    runtime: AiRuntime,
    ctx: CheckContext,
    install: AiInstallType,
    blocker: Option<&BackendSpawnBlocker>,
    app_dir: &Path,
) -> (Vec<WarningReason>, Vec<WarningReason>) {
    let Some(blocker) = blocker.filter(|_| ctx.ai_enabled) else {
        return (Vec::new(), Vec::new());
    };
    let app_dir = app_dir.display().to_string();
    let reason = match blocker {
        BackendSpawnBlocker::ScriptMissing => WarningReason::BackendScriptMissing { app_dir },
        BackendSpawnBlocker::PayloadOutdated => WarningReason::BackendPayloadOutdated { app_dir },
        BackendSpawnBlocker::PythonMissing(detail) => WarningReason::BackendPythonMissing { detail: detail.clone() },
    };
    let runtime_reasons = match runtime {
        AiRuntime::Backend => vec![reason.clone()],
        AiRuntime::Native => Vec::new(),
    };
    let python_expected_missing =
        matches!(blocker, BackendSpawnBlocker::PythonMissing(_)) && install == AiInstallType::None;
    let autostart_reasons =
        if ctx.backend_autostart && !python_expected_missing { vec![reason] } else { Vec::new() };
    (runtime_reasons, autostart_reasons)
}

/// Plain facts of one native selection evaluation, the input of
/// [`native_family_reasons`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeFacts {
    /// The effective build slug after the fallback.
    pub effective_build: String,
    /// Whether the effective build ships for this OS/arch (`build_shipped_here`).
    pub effective_build_shipped: bool,
    /// Why the configured selection falls back, if it does.
    pub fallback: Option<FallbackCause>,
    /// Whether the crash guard of the scope to load is `Suspect`.
    pub guard_suspect: bool,
    /// Whether the effective EP consumes the persisted device id at all (the report's
    /// effective device is not `Default`).
    pub device_consumed: bool,
    /// The persisted `General.ai_onnx_device_id`.
    pub device_id: Option<String>,
    /// Whether the EP offers the persisted id now; `None` when not probed.
    pub device_offer: Option<DeviceOffer>,
}

/// The answer of the device probe, the device input of [`native_family_reasons`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeviceOffer {
    /// `onnx_caps::device_id_offered` for the persisted id.
    pub offered: bool,
    /// How many devices the EP offers now (`ep_device_ids`), for the message.
    pub available: usize,
}

/// Evaluates the native selection and gathers [`NativeFacts`]; every native key is clean
/// under `--no-ai` or when the runtime is `Backend` (those items are not drawn then).
fn gather_native(cfg: &Value, ctx: CheckContext) -> [(SettingKey, Vec<WarningReason>); 4] {
    if !native_checks_apply(ctx, AiRuntime::from_user_settings(cfg)) {
        return NATIVE_FAMILY.map(|key| (key, Vec::new()));
    }
    let report = ms_native_runtime::evaluate_native_selection(cfg);
    // The scope the next native load of this process reads (committed, else configured);
    // the panel's Retry resets the same scope. Never resolves the process selection cache.
    let scope = ms_native_runtime::next_load_scope_key(&report);
    let guard_suspect =
        ms_config::ort_load_decision(ms_config::read_ort_load_guard(cfg, &scope)) == OrtLoadDecision::Suspect;
    let fallback = report.fallback.map(fallback_cause);
    let device_id = ms_config::ai_onnx_device_id_from_user_settings(cfg);
    let device_consumed = report.device != NativeDeviceSelection::Default;
    // The capability probe spawns system commands: only when the device rule can fire.
    let device_offer = match &device_id {
        Some(id) if fallback.is_none() && device_consumed => {
            let caps = probe_onnx_caps();
            Some(DeviceOffer {
                offered: device_id_offered(report.provider, id, &caps),
                available: ep_device_ids(report.provider, &caps).len(),
            })
        }
        _ => None,
    };
    native_family_reasons(&NativeFacts {
        effective_build: report.build.to_string(),
        effective_build_shipped: ms_onnx_runtime::build_shipped_here(report.build),
        fallback,
        guard_suspect,
        device_consumed,
        device_id,
        device_offer,
    })
}

/// Whether the native family is checked at all: not under `--no-ai`, and only for the
/// `Native` runtime (the native items are drawn only then).
pub(crate) fn native_checks_apply(ctx: CheckContext, runtime: AiRuntime) -> bool {
    match runtime {
        AiRuntime::Native => ctx.ai_enabled,
        AiRuntime::Backend => false,
    }
}

/// Maps the native-only reason onto its wasm-clean mirror.
fn fallback_cause(reason: NativeFallbackReason) -> FallbackCause {
    match reason {
        NativeFallbackReason::NoRunnableProvider => FallbackCause::NoRunnableProvider,
        NativeFallbackReason::CudaRuntimeMissing => FallbackCause::CudaRuntimeMissing,
        NativeFallbackReason::OpenVinoRuntimeMissing => FallbackCause::OpenVinoRuntimeMissing,
        NativeFallbackReason::WebGpuUnavailable => FallbackCause::WebGpuUnavailable,
        NativeFallbackReason::UnsupportedOnPlatform => FallbackCause::UnsupportedOnPlatform,
        NativeFallbackReason::BuildNotShippedForPlatform => FallbackCause::BuildNotShipped,
    }
}

/// Whether a fallback cause is about the EP (flagged on the EP item) rather than the
/// build (flagged on the build item).
fn fallback_is_provider_level(cause: FallbackCause) -> bool {
    match cause {
        FallbackCause::WebGpuUnavailable | FallbackCause::UnsupportedOnPlatform => true,
        FallbackCause::NoRunnableProvider
        | FallbackCause::CudaRuntimeMissing
        | FallbackCause::OpenVinoRuntimeMissing
        | FallbackCause::BuildNotShipped => false,
    }
}

/// The native-family rules, one entry per [`NATIVE_FAMILY`] key, in that order.
///
/// - `OnnxBuild` Red: the EFFECTIVE build is not shipped here (native AI cannot load at
///   all); the Yellow fallback text is then suppressed, since it would claim the CPU
///   build runs.
/// - `OnnxBuild` / `OnnxProvider` Yellow: the CPU fallback, on the build or EP item by
///   its cause.
/// - `OnnxDevice` Yellow: no fallback, the EP consumes the id, and the persisted id does not
///   name a device the EP offers now (`device_id_offered`; user decision Q4).
/// - `OrtCrashGuard` Red: the guard of the scope to load is `Suspect`.
pub(crate) fn native_family_reasons(facts: &NativeFacts) -> [(SettingKey, Vec<WarningReason>); 4] {
    let mut build = Vec::new();
    let mut provider = Vec::new();
    let mut device = Vec::new();
    let mut guard = Vec::new();

    if !facts.effective_build_shipped {
        build.push(WarningReason::OrtBuildNotShipped { build: facts.effective_build.clone() });
    } else if let Some(cause) = facts.fallback {
        let reason = WarningReason::NativeCpuFallback { reason: cause };
        if fallback_is_provider_level(cause) { provider.push(reason) } else { build.push(reason) }
    }

    if facts.fallback.is_none()
        && facts.device_consumed
        && let (Some(id), Some(offer)) = (&facts.device_id, facts.device_offer)
        && !offer.offered
    {
        device.push(WarningReason::DeviceMissing { device: id.clone(), available: offer.available });
    }

    if facts.guard_suspect {
        guard.push(WarningReason::OrtLoadCrashed);
    }

    [
        (SettingKey::OnnxBuild, build),
        (SettingKey::OnnxProvider, provider),
        (SettingKey::OnnxDevice, device),
        (SettingKey::OrtCrashGuard, guard),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ms_config::{AiInstallType, AiRuntime};

    use super::{
        DeviceOffer, NativeFacts, backend_reasons, native_checks_apply, native_family_reasons, projects_root_reasons,
        ui_catalog_reasons,
    };
    use crate::ai_backend_supervisor::BackendSpawnBlocker;
    use crate::settings_warnings::model::{FallbackCause, NATIVE_FAMILY, SettingKey, WarningReason};
    use crate::settings_warnings::runtime::CheckContext;

    const APP_DIR: &str = "/home/u/ManhwaStudio";

    fn ctx(ai_enabled: bool, backend_autostart: bool) -> CheckContext {
        CheckContext { ai_enabled, backend_autostart }
    }

    fn clean_native() -> NativeFacts {
        NativeFacts {
            effective_build: "cuda12".to_string(),
            effective_build_shipped: true,
            fallback: None,
            guard_suspect: false,
            device_consumed: true,
            device_id: Some("0".to_string()),
            device_offer: Some(DeviceOffer { offered: true, available: 2 }),
        }
    }

    /// The reasons of `key` in a native-family result.
    fn of(result: &[(SettingKey, Vec<WarningReason>); 4], key: SettingKey) -> Vec<WarningReason> {
        result.iter().find(|(entry, _)| *entry == key).map(|(_, reasons)| reasons.clone()).unwrap_or_default()
    }

    #[test]
    fn projects_root_default_missing_is_clean() {
        let default_root = Path::new("/home/u/Documents/manhwastudio_projects");
        assert!(projects_root_reasons(default_root, default_root, false, false).is_empty());
    }

    #[test]
    fn projects_root_custom_missing_and_not_dir_are_flagged() {
        let default_root = Path::new("/home/u/Documents/manhwastudio_projects");
        let custom = Path::new("/home/u/elsewhere");
        assert_eq!(
            projects_root_reasons(custom, default_root, false, false),
            vec![WarningReason::ProjectsRootMissing { path: custom.display().to_string() }]
        );
        assert_eq!(
            projects_root_reasons(custom, default_root, true, false),
            vec![WarningReason::ProjectsRootNotDirectory { path: custom.display().to_string() }]
        );
        // A FILE at the default path is a real problem: Q1 only covers "missing".
        assert_eq!(
            projects_root_reasons(default_root, default_root, true, false),
            vec![WarningReason::ProjectsRootNotDirectory { path: default_root.display().to_string() }]
        );
        assert!(projects_root_reasons(custom, default_root, true, true).is_empty());
    }

    #[test]
    fn ui_catalog_rule() {
        assert!(ui_catalog_reasons("ru", None, true).is_empty());
        assert_eq!(
            ui_catalog_reasons("ru", Some("bad json".to_string()), true),
            vec![WarningReason::UiCatalogEmbedded { tag: "ru".to_string(), error: "bad json".to_string() }]
        );
        assert_eq!(
            ui_catalog_reasons("de", Some("missing".to_string()), false),
            vec![WarningReason::UiCatalogEnglish { tag: "de".to_string() }]
        );
    }

    #[test]
    fn backend_silent_without_ai_or_blocker() {
        let blocker = BackendSpawnBlocker::ScriptMissing;
        let none = (Vec::new(), Vec::new());
        assert_eq!(
            backend_reasons(AiRuntime::Backend, ctx(false, true), AiInstallType::Full, Some(&blocker), Path::new(APP_DIR)),
            none
        );
        assert_eq!(
            backend_reasons(AiRuntime::Backend, ctx(true, true), AiInstallType::Full, None, Path::new(APP_DIR)),
            none
        );
    }

    #[test]
    fn backend_runtime_flagged_only_under_backend() {
        let blocker = BackendSpawnBlocker::PayloadOutdated;
        let expected = WarningReason::BackendPayloadOutdated { app_dir: Path::new(APP_DIR).display().to_string() };
        let (runtime, autostart) =
            backend_reasons(AiRuntime::Backend, ctx(true, false), AiInstallType::Full, Some(&blocker), Path::new(APP_DIR));
        assert_eq!(runtime, vec![expected.clone()]);
        assert!(autostart.is_empty());
        let (runtime, autostart) =
            backend_reasons(AiRuntime::Native, ctx(true, true), AiInstallType::Full, Some(&blocker), Path::new(APP_DIR));
        assert!(runtime.is_empty());
        assert_eq!(autostart, vec![expected]);
    }

    /// Q2: a missing Python while no AI is installed does not flag autostart, but an
    /// explicit Backend runtime stays flagged; other blockers are never suppressed.
    #[test]
    fn autostart_python_missing_suppressed_without_install() {
        let python = BackendSpawnBlocker::PythonMissing("no venv".to_string());
        let expected = WarningReason::BackendPythonMissing { detail: "no venv".to_string() };
        let (runtime, autostart) =
            backend_reasons(AiRuntime::Backend, ctx(true, true), AiInstallType::None, Some(&python), Path::new(APP_DIR));
        assert_eq!(runtime, vec![expected.clone()]);
        assert!(autostart.is_empty());
        let (_, autostart) =
            backend_reasons(AiRuntime::Native, ctx(true, true), AiInstallType::Base, Some(&python), Path::new(APP_DIR));
        assert_eq!(autostart, vec![expected]);
        let script = BackendSpawnBlocker::ScriptMissing;
        let (_, autostart) =
            backend_reasons(AiRuntime::Native, ctx(true, true), AiInstallType::None, Some(&script), Path::new(APP_DIR));
        assert_eq!(autostart.len(), 1);
    }

    #[test]
    fn native_checks_silent_under_backend_and_without_ai() {
        assert!(native_checks_apply(ctx(true, false), AiRuntime::Native));
        assert!(!native_checks_apply(ctx(true, true), AiRuntime::Backend));
        assert!(!native_checks_apply(ctx(false, false), AiRuntime::Native));
    }

    #[test]
    fn native_clean_selection_has_no_reasons() {
        let result = native_family_reasons(&clean_native());
        assert_eq!(result.each_ref().map(|(key, _)| *key), NATIVE_FAMILY);
        assert!(result.iter().all(|(_, reasons)| reasons.is_empty()));
    }

    #[test]
    fn native_unshipped_effective_build_is_red_and_hides_the_fallback_text() {
        let facts = NativeFacts {
            effective_build: "cpu".to_string(),
            effective_build_shipped: false,
            fallback: Some(FallbackCause::BuildNotShipped),
            ..clean_native()
        };
        let result = native_family_reasons(&facts);
        assert_eq!(
            of(&result, SettingKey::OnnxBuild),
            vec![WarningReason::OrtBuildNotShipped { build: "cpu".to_string() }]
        );
        assert!(of(&result, SettingKey::OnnxProvider).is_empty());
    }

    #[test]
    fn native_fallback_goes_to_the_build_or_ep_item_by_cause() {
        for cause in [
            FallbackCause::NoRunnableProvider,
            FallbackCause::CudaRuntimeMissing,
            FallbackCause::OpenVinoRuntimeMissing,
            FallbackCause::BuildNotShipped,
        ] {
            let result = native_family_reasons(&NativeFacts { fallback: Some(cause), ..clean_native() });
            assert_eq!(of(&result, SettingKey::OnnxBuild), vec![WarningReason::NativeCpuFallback { reason: cause }]);
            assert!(of(&result, SettingKey::OnnxProvider).is_empty(), "{cause:?}");
        }
        for cause in [FallbackCause::WebGpuUnavailable, FallbackCause::UnsupportedOnPlatform] {
            let result = native_family_reasons(&NativeFacts { fallback: Some(cause), ..clean_native() });
            assert_eq!(of(&result, SettingKey::OnnxProvider), vec![WarningReason::NativeCpuFallback { reason: cause }]);
            assert!(of(&result, SettingKey::OnnxBuild).is_empty(), "{cause:?}");
        }
    }

    #[test]
    fn native_device_rule() {
        let missing = NativeFacts {
            device_id: Some("3".to_string()),
            device_offer: Some(DeviceOffer { offered: false, available: 2 }),
            ..clean_native()
        };
        assert_eq!(
            of(&native_family_reasons(&missing), SettingKey::OnnxDevice),
            vec![WarningReason::DeviceMissing { device: "3".to_string(), available: 2 }]
        );
        // Silent when the EP ignores the id, after a fallback, or when not probed.
        let ignored = NativeFacts { device_consumed: false, ..missing.clone() };
        assert!(of(&native_family_reasons(&ignored), SettingKey::OnnxDevice).is_empty());
        let fell_back = NativeFacts { fallback: Some(FallbackCause::CudaRuntimeMissing), ..missing.clone() };
        assert!(of(&native_family_reasons(&fell_back), SettingKey::OnnxDevice).is_empty());
        let unprobed = NativeFacts { device_offer: None, ..missing };
        assert!(of(&native_family_reasons(&unprobed), SettingKey::OnnxDevice).is_empty());
    }

    #[test]
    fn native_suspect_guard_is_flagged() {
        let result = native_family_reasons(&NativeFacts { guard_suspect: true, ..clean_native() });
        assert_eq!(of(&result, SettingKey::OrtCrashGuard), vec![WarningReason::OrtLoadCrashed]);
    }
}
