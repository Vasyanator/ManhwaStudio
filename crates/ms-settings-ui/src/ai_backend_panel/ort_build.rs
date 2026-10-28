/*
FILE OVERVIEW: crates/ms-settings-ui/src/ai_backend_panel/ort_build.rs
The background side of the native ONNX selection: every probe, config read, download and
write the pickers start, plus the build-action button they depend on.

Key structures:
- `OrtBuildAction`: the build-action button decided from the committed-dylib state.

Key functions:
- `ensure_build_presence_probe` / `build_dylib_present`: the per-build dylib presence cache.
- `ort_build_action` (pure, unit-tested) / `draw_build_action_button`: download / retry /
  restart note; Retry resets the load guard via `spawn_reset_ort_guard`.
- `maybe_start_ort_download` / `draw_ort_download_progress`: the onnxruntime auto-download.
- `start_onnx_caps_probe`, `start_onnx_config_read`: one-shot capability probe and config read.
- `spawn_save_onnx_build`, `spawn_save_onnx_provider_device`: off-thread config writes.

Notes:
Desktop only (declared under `cfg(not(target_arch = "wasm32"))` in `mod.rs`). Blocking work
runs on named worker threads; the GUI thread only reads the shared slots. Writers report
landed changes through `super::report_landed`.
*/

use super::{AiBackendPanelState, OnnxConfigRead, report_landed};
use crate::onnx_caps::probe_onnx_caps;
use crate::settings_warnings::SettingChange;
use ms_onnx_runtime::{OrtDownloadProgress, OrtDownloadStage};
use std::sync::mpsc::Sender;

/// Ensures a background presence probe for build `slug` runs once (idempotent): a cache
/// miss marks the slot in-flight (`None`) and spawns a worker that asks
/// `ms_onnx_runtime::ort_build_installed` and records `Some(present)`. The blocking check
/// never runs on the GUI thread.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn ensure_build_presence_probe(state: &AiBackendPanelState, slug: &str) {
    {
        let mut map = match state.ort_build_present.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if map.contains_key(slug) {
            return; // already probed, or an in-flight probe (value == None).
        }
        map.insert(slug.to_string(), None); // mark in-flight before spawning.
    }
    let slot = state.ort_build_present.clone();
    let owned = slug.to_string();
    if let Err(err) = std::thread::Builder::new()
        .name("ort-build-presence".to_string())
        .spawn(move || {
            // The resolver's own "installed" rule (marker + member sizes), so the panel
            // never disagrees with what `resolve_or_download_ort_dylib` would do.
            let present = ms_onnx_runtime::ort_build_installed(&owned);
            if let Ok(mut map) = slot.lock() {
                map.insert(owned, Some(present));
            }
        })
    {
        ms_log::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start build-presence probe thread: {err}"
        ));
        // Drop the in-flight marker so a later frame can retry the probe.
        if let Ok(mut map) = state.ort_build_present.lock() {
            map.remove(slug);
        }
    }
}

/// The cached presence answer for build `slug`: `Some(true/false)` once probed, `None`
/// while a probe is in flight or before it started. Cheap lock read — GUI-thread safe.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn build_dylib_present(state: &AiBackendPanelState, slug: &str) -> Option<bool> {
    let map = match state.ort_build_present.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    map.get(slug).copied().flatten()
}

/// The build-action button the native panel shows, decided from the committed-dylib state.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OrtBuildAction {
    /// Same-build SIGILL-guard retry ("Повторить попытку ORT"): reset the load guard so
    /// the native runtime re-attempts loading the selected build.
    Retry,
    /// Download a not-yet-committed / not-present build ("Загрузить другую сборку ort")
    /// and reset the load latch so the next OCR loads it. Safe: no dylib committed yet.
    LoadOtherBuild,
    /// A different build is already committed process-globally; the ort dylib cannot be
    /// hot-swapped, so show a static "restart required" note.
    RestartNote,
}

/// Decides the build-action button purely from the committed-dylib state.
///
/// - `ort_committed` == false (no dylib loaded this session): a not-yet-present selected
///   build offers `LoadOtherBuild` (download); an already-present build offers a same-build
///   `Retry`.
/// - `ort_committed` == true: if the selected build IS the committed `active_build`, a
///   same-build `Retry`; otherwise `RestartNote` (the committed dylib is pinned).
/// - `bundled` (the library ships in the app package, Android): never `LoadOtherBuild`,
///   there is nothing to download; an uncommitted bundled build offers `Retry`.
///
/// `active_build` is the committed build slug; `None` (nothing committed) is treated as
/// `Retry` in the committed branch, which cannot occur in practice.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn ort_build_action(
    ort_committed: bool,
    active_build: Option<&str>,
    selected_build: &str,
    dylib_present: bool,
    bundled: bool,
) -> OrtBuildAction {
    if ort_committed {
        match active_build {
            Some(active) if active == selected_build => OrtBuildAction::Retry,
            Some(_) => OrtBuildAction::RestartNote,
            None => OrtBuildAction::Retry,
        }
    } else if dylib_present || bundled {
        OrtBuildAction::Retry
    } else {
        OrtBuildAction::LoadOtherBuild
    }
}

/// Renders the build-action button/note per [`ort_build_action`], reading the committed
/// state from `native_runtime` on the GUI thread (cheap atomic / `OnceLock` reads — no
/// disk I/O once a dylib is committed).
///
/// `dylib_present` is `None` while the presence probe is in flight; it is treated as "not
/// present" so the uncommitted branch offers a download until the build is proven cached.
/// `bundled` (Android) suppresses the download offer (see [`ort_build_action`]).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn draw_build_action_button(
    ui: &mut egui::Ui,
    state: &AiBackendPanelState,
    dylib_present: Option<bool>,
    bundled: bool,
) {
    let committed = ms_native_runtime::ort_dylib_committed();
    let active = ms_native_runtime::active_build();
    let action = ort_build_action(
        committed,
        active.as_deref(),
        &state.selected_onnx_build,
        dylib_present.unwrap_or(false),
        bundled,
    );
    match action {
        OrtBuildAction::Retry => {
            if ui
                .button(t!("ai_backend.ort_retry_button"))
                .on_hover_text(
                    t!("ai_backend.ort_retry_tooltip"),
                )
                .clicked()
            {
                spawn_reset_ort_guard(state.pending_changes.sender());
            }
        }
        OrtBuildAction::LoadOtherBuild => {
            if ui
                .button(t!("ai_backend.ort_load_other_build_button"))
                .on_hover_text(
                    t!("ai_backend.ort_load_other_build_tooltip"),
                )
                .clicked()
            {
                let build = state.selected_onnx_build.clone();
                maybe_start_ort_download(state, &build);
                // No dylib committed yet, so the next OCR can pick up the new build.
                ms_native_runtime::reset_load_latch();
            }
        }
        OrtBuildAction::RestartNote => {
            ui.colored_label(
                ms_theme::status::WARNING,
                t!("ai_backend.restart_required_note"),
            );
        }
    }
}

/// Persists the selected ONNX build slug off the GUI thread, then reports
/// `SettingChange::OnnxBuild` on `landed` (whatever the write's result).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn spawn_save_onnx_build(build_slug: String, landed: Sender<SettingChange>) {
    let path = ms_config::user_config_path();
    if let Err(err) = std::thread::Builder::new()
        .name("onnx-build-save".to_string())
        .spawn(move || {
            if let Err(err) = ms_config::save_onnx_build(&path, &build_slug) {
                ms_log::runtime_log::log_error(format!(
                    "[ai-backend-panel] failed to persist ONNX build '{build_slug}': {err}"
                ));
            }
            report_landed(&landed, SettingChange::OnnxBuild);
        })
    {
        ms_log::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ONNX build save thread: {err}"
        ));
    }
}

/// Starts the one-shot background ONNX capability probe (system CUDA + DirectML
/// adapters). The blocking probes never run on the GUI thread.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn start_onnx_caps_probe(state: &mut AiBackendPanelState) {
    if state.onnx_caps_probe_started {
        return;
    }
    state.onnx_caps_probe_started = true;
    let slot = state.onnx_caps.clone();
    if let Err(err) = std::thread::Builder::new()
        .name("onnx-caps-probe".to_string())
        .spawn(move || {
            let caps = probe_onnx_caps();
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(caps);
            }
        })
    {
        ms_log::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ONNX capability probe thread: {err}"
        ));
    }
}

/// Starts the one-shot background read of the unified ONNX selection + model limit
/// from config so the GUI thread never reads config directly.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn start_onnx_config_read(state: &mut AiBackendPanelState) {
    if state.onnx_config_read_started {
        return;
    }
    state.onnx_config_read_started = true;
    let slot = state.onnx_config.clone();
    if let Err(err) = std::thread::Builder::new()
        .name("onnx-config-read".to_string())
        .spawn(move || {
            let cfg = ms_config::load_raw_user_settings_for_startup()
                .unwrap_or(serde_json::Value::Null);
            let read = OnnxConfigRead {
                build: ms_config::ai_onnx_build_from_user_settings(&cfg),
                provider_token: ms_config::ai_onnx_provider_token_from_user_settings(&cfg),
                device_id: ms_config::ai_onnx_device_id_from_user_settings(&cfg),
                max_loaded_models: ms_config::ai_max_loaded_models_from_user_settings(&cfg),
            };
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(read);
            }
        })
    {
        ms_log::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ONNX config read thread: {err}"
        ));
    }
}

/// Localized label for an onnxruntime download stage (a retry names the remaining wait
/// and the attempt).
#[cfg(not(target_arch = "wasm32"))]
fn ort_stage_label(stage: OrtDownloadStage) -> String {
    match stage {
        OrtDownloadStage::Probing => t!("ai_backend.ort_stage_probe").to_owned(),
        OrtDownloadStage::WaitingForOtherDownload => t!("ai_backend.ort_stage_waiting").to_owned(),
        OrtDownloadStage::Downloading => t!("ai_backend.ort_stage_download").to_owned(),
        OrtDownloadStage::Retrying { attempt, max_attempts, delay_secs } => {
            tf!("ai_backend.ort_stage_retrying", seconds = delay_secs, attempt = attempt, max = max_attempts)
        }
        OrtDownloadStage::Verifying => t!("ai_backend.ort_stage_verify").to_owned(),
        OrtDownloadStage::Extracting => t!("ai_backend.ort_stage_extract").to_owned(),
        OrtDownloadStage::Done => t!("ai_backend.ort_stage_done").to_owned(),
    }
}

/// Starts a background onnxruntime dylib download for BUILD `build`, unless one is
/// already running or this exact build has already finished, errored or been cancelled
/// (the resume button clears `cancelled` first).
///
/// Never dlopens/loads here — it only fetches the library (or returns immediately when
/// the build is already cached) so the native OCR load (via `native_runtime`, still under
/// the SIGILL guard) finds it ready. On success the presence cache for `build` is set to
/// `Some(true)`. Worker-thread only.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn maybe_start_ort_download(state: &AiBackendPanelState, build: &str) {
    let build = build.to_string();
    {
        let mut status = match state.ort_download.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        // One download at a time; and do not re-run a completed/errored build (a build
        // re-select changes `build_slug` and resets this below).
        if status.running {
            return;
        }
        if status.build_slug.as_deref() == Some(build.as_str())
            && (status.done || status.error.is_some() || status.cancelled)
        {
            return;
        }
        status.build_slug = Some(build.clone());
        status.progress = None;
        status.done = false;
        status.error = None;
        status.cancelled = false;
        status.running = true;
        status.cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    }

    let slot = state.ort_download.clone();
    let present = state.ort_build_present.clone();
    let cancel = match state.ort_download.lock() {
        Ok(status) => status.cancel.clone(),
        Err(poisoned) => poisoned.into_inner().cancel.clone(),
    };
    if let Err(err) = std::thread::Builder::new()
        .name("ort-download".to_string())
        .spawn(move || {
            let mut report = |progress: OrtDownloadProgress| {
                if let Ok(mut status) = slot.lock() {
                    status.progress = Some(progress);
                }
            };
            let result = ms_onnx_runtime::resolve_or_download_ort_dylib(&build, &cancel, &mut report);
            let mut status = match slot.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            status.running = false;
            // Clear the last reported progress so the UI shows a static terminal
            // state (a green "ready" or the error) rather than keeping the final
            // `Done` progress alive, which would otherwise render a spinner forever.
            status.progress = None;
            match result {
                Ok(_path) => {
                    status.done = true;
                    // The build is now on disk; record it so the button flips to a
                    // same-build retry without waiting for a fresh presence probe.
                    if let Ok(mut map) = present.lock() {
                        map.insert(build.clone(), Some(true));
                    }
                }
                Err(ms_onnx_runtime::OrtRuntimeError::Cancelled) => {
                    status.cancelled = true;
                    ms_log::runtime_log::log_info(format!(
                        "[ai-backend-panel] onnxruntime download cancelled for build '{build}'; staged bytes kept for resume"
                    ));
                }
                Err(err) => {
                    // `status.error` is user-facing, so it uses the localized Display;
                    // the log uses Debug (stable English variant name) to stay grep-able
                    // regardless of the selected UI language.
                    status.error = Some(err.to_string());
                    ms_log::runtime_log::log_warn(format!(
                        "[ai-backend-panel] onnxruntime download failed for build '{build}': {err:?}"
                    ));
                }
            }
        })
    {
        ms_log::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ORT download thread: {err}"
        ));
        // Clear the running flag so a later attempt can retry.
        if let Ok(mut status) = state.ort_download.lock() {
            status.running = false;
        }
    }
}

/// Renders the onnxruntime auto-download progress/result: a determinate `ProgressBar`
/// when the total size is known, otherwise a spinner + stage text (a retry shows its
/// countdown and attempt), with a Cancel button while the worker runs. A cancelled
/// download shows a Resume button that restarts it (the worker resumes the kept `.part`).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn draw_ort_download_progress(ui: &mut egui::Ui, state: &AiBackendPanelState) {
    let (progress, running, done, error, cancelled, cancel, build_slug) = match state.ort_download.lock() {
        Ok(status) => (status.progress, status.running, status.done, status.error.clone(), status.cancelled, status.cancel.clone(), status.build_slug.clone()),
        Err(poisoned) => {
            let status = poisoned.into_inner();
            (status.progress, status.running, status.done, status.error.clone(), status.cancelled, status.cancel.clone(), status.build_slug.clone())
        }
    };

    if cancelled && !running {
        ui.horizontal(|ui| {
            ui.colored_label(ms_theme::status::WARNING, t!("ai_backend.ort_cancelled"));
            if ui.button(t!("ai_backend.ort_resume_button")).clicked()
                && let Some(build) = build_slug
            {
                // Clear the latch that keeps the auto-download from restarting a build
                // the user cancelled, then start it again (the `.part` resumes).
                match state.ort_download.lock() {
                    Ok(mut status) => status.cancelled = false,
                    Err(poisoned) => poisoned.into_inner().cancelled = false,
                }
                maybe_start_ort_download(state, &build);
            }
        });
        return;
    }

    if let Some(error) = error {
        ui.colored_label(
            ms_theme::status::ERROR,
            tf!("ai_backend.ort_prepare_error", error = error),
        );
        return;
    }

    // A completed download is terminal: render a static "ready", never a spinner.
    // The worker clears `progress` on completion, but a lingering final `Done`
    // progress is also treated as terminal here so the spinner can never persist.
    if done || progress.map(|p| p.stage) == Some(OrtDownloadStage::Done) {
        ui.colored_label(
            ms_theme::status::SUCCESS,
            t!("ai_backend.ort_ready"),
        );
        return;
    }

    // Otherwise a download is in flight: determinate bar when the size is known,
    // else a spinner with the current stage.
    if running || progress.is_some() {
        match progress {
            Some(progress) => {
                let stage = ort_stage_label(progress.stage);
                let retrying = matches!(progress.stage, OrtDownloadStage::Retrying { .. });
                match progress.total {
                    Some(total) if total > 0 && !retrying => {
                        // Display-only ratio; the f64/f32 casts cannot meaningfully
                        // lose precision for a 0..=1 progress fraction.
                        let fraction =
                            (progress.downloaded.min(total) as f64 / total as f64) as f32;
                        let percent = (fraction * 100.0) as u32;
                        ui.add(egui::ProgressBar::new(fraction).text(format!("{stage} {percent}%")));
                    }
                    Some(_) | None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("{stage}…"));
                        });
                    }
                }
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t!("ai_backend.ort_preparing"));
                });
            }
        }
        // The worker polls the flag during the busy wait, the transfer, the retry wait and
        // the hash pass; it then reports `Cancelled` and clears `running`.
        if running {
            let requested = cancel.load(std::sync::atomic::Ordering::Relaxed);
            if ui.add_enabled(!requested, egui::Button::new(t!("ai_backend.ort_cancel_button"))).clicked() {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// Persists the unified ONNX selection off the GUI thread, records `device_id` in `saved`
/// when the write succeeded, then reports `SettingChange::OnnxProviderDevice` on `landed`
/// (whatever the write's result).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn spawn_save_onnx_provider_device(
    provider_token: String,
    device_id: String,
    landed: Sender<SettingChange>,
    saved: std::sync::Arc<std::sync::Mutex<Option<String>>>,
) {
    let path = ms_config::user_config_path();
    if let Err(err) = std::thread::Builder::new()
        .name("onnx-selection-save".to_string())
        .spawn(move || {
            match ms_config::save_onnx_provider_device(&path, &provider_token, &device_id) {
                Ok(()) => match saved.lock() {
                    Ok(mut guard) => *guard = Some(device_id),
                    Err(poisoned) => *poisoned.into_inner() = Some(device_id),
                },
                Err(err) => ms_log::runtime_log::log_error(format!(
                    "[ai-backend-panel] failed to persist ONNX selection '{provider_token}'/'{device_id}': {err}"
                )),
            }
            report_landed(&landed, SettingChange::OnnxProviderDevice);
        })
    {
        ms_log::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ONNX selection save thread: {err}"
        ));
    }
}

/// Resets the ONNX Runtime SIGILL load-guard for the EFFECTIVE native scope
/// (provider + adapter) — on disk and the in-process latch — so a retry can
/// re-attempt the native runtime without an app restart.
///
/// The guard scope is `ms_native_runtime::next_load_scope_key` over a fresh config read
/// (the committed scope, else the configured effective one: the same rule as the settings
/// guard warning), computed inside the worker thread (disk I/O + hardware probes), never
/// on the GUI thread. It does NOT resolve the process selection, so a build changed after
/// a Retry still takes effect on the next load. An unreadable config skips the reset
/// (logged). Reports `SettingChange::OrtGuardReset` on `landed` once the worker finished
/// (whatever its result).
#[cfg(not(target_arch = "wasm32"))]
fn spawn_reset_ort_guard(landed: Sender<SettingChange>) {
    // Reset the in-process latch first so a retry re-attempts loading.
    ms_native_runtime::reset_load_latch();

    let path = ms_config::user_config_path();
    if let Err(err) = std::thread::Builder::new()
        .name("ort-guard-reset".to_string())
        .spawn(move || {
            match ms_config::load_raw_user_settings_for_startup() {
                Ok(cfg) => {
                    let report = ms_native_runtime::evaluate_native_selection(&cfg);
                    let scope = ms_native_runtime::next_load_scope_key(&report);
                    if let Err(err) = ms_config::ort_load_guard::reset_ort_load_guard(&path, &scope) {
                        ms_log::runtime_log::log_error(format!(
                            "[ai-backend-panel] failed to reset ORT load guard for '{scope}': {err}"
                        ));
                    }
                }
                Err(err) => ms_log::runtime_log::log_error(format!(
                    "[ai-backend-panel] could not read user config to find the ORT load guard scope; guard not reset; error: {err:#}"
                )),
            }
            report_landed(&landed, SettingChange::OrtGuardReset);
        })
    {
        ms_log::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ORT guard reset thread: {err}"
        ));
    }
}
