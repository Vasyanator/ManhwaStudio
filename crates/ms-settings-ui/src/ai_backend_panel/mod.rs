/*
FILE OVERVIEW: crates/ms-settings-ui/src/ai_backend_panel/mod.rs
Shared AI backend settings widget used by BOTH the studio settings tab and the
launcher settings page. This file is the pane's entry point: the per-UI state, the
shared option types, the sub-tab strip and `draw_ai_backend_panel`, which lays the pane
out; the tabs' bodies live in the sibling files (map: `MODULE_README.md` in this directory).

Layout:
Above the strip: the platform warning and the shared «Модели в памяти» limit (one value caps
both small-model LRUs). Then the house sub-tab strip (`ui.horizontal` + `selectable_label`,
session state only): «Основной PyTorch» (`backend_process.rs`), «ONNXruntime» (runtime switch
+ ONNX selection) and «Stable-Diffusion.cpp» (`sdcpp_tab.rs`). A tab is listed only where its
platform feature exists (`visible_tabs`); one visible tab draws no strip. A tab label carries
the corner badge of its `SettingGroupId` (`WarningSet::group_level`).

Why this is shared:
The backend is app-global (see `crate::ai_backend_supervisor`), so its status,
process controls, and device/ONNX selection must be reachable from the launcher as
well as the studio. This module renders that one panel against an
[`AiBackendHandle`] (the shared snapshots + command channels) plus a per-UI
[`AiBackendPanelState`] holding only local selection scratch fields, so the two
call sites stay in sync without duplicating the UI.

Build availability:
The native build picker does not decide which builds can run here: it asks the native
runtime's rule (`ms_native_runtime::native_build_fallback_reason` over the probed
`NativeHardwareFacts`), so the picker shows what the runtime's selection will do. The
per-build verdicts, the picker groups and the build labels are `crate::native_builds`
(`NativeBuildAvailability`); the probed capabilities and the EP device-id rule live in
`crate::onnx_caps`; `onnx_selection.rs` draws the pickers. The PyTorch build texts are
`crate::torch_build_text`.

Change reporting and warning badges:
`draw_ai_backend_panel` returns an `AiBackendPanelOutcome` whose `changed_settings` lists
the checked settings whose write LANDED: the off-thread savers (runtime, build, EP/device,
ORT guard reset) send their `SettingChange` over the state's completion channel after the
write returned, and the pane drains it every frame. Autostart is persisted by the
supervisor worker, so its change carries the new value and is reported on the click. With
a `WarningSet` (launcher) the runtime combo, autostart checkbox, build / EP / device rows
and the build-action row carry inline badges; `None` (studio) draws the layout without badges.

Platform gate:
Where `ms_platform::PlatformFeature::PythonBackend` is unavailable (Android, web) the pane
leads with the localized `Unavailable` message, offers no "Backend" runtime choice and hides
the «Основной PyTorch» tab (status, process control, log, PyTorch device, large models,
diagnostics); the «Stable-Diffusion.cpp» tab likewise needs `PlatformFeature::SdCppRuntime`. The supervisor never starts a backend there
(`AiBackendHandle::local_ai`) and refuses the spawn on its own as well
(`BackendSpawnBlocker::PlatformUnavailable`). Under `--no-ai` on desktop the blocks stay
visible but disabled, with the flag named (the sd.cpp tab too).
Where every onnxruntime build is bundled in the app package (Android:
`ms_onnx_runtime::ort_downloads_offered_here` is false) the build picker lists only the
builds shipped here, and a bundled build never offers or starts a download.

Key items:
- `AiBackendPanelState`: per-UI selection scratch (device/provider/onnx/max-models) and
  the completion channel of the off-thread writes (`PendingChanges`).
- `AiBackendPanelOutcome`: the landed setting changes of one frame.
- `draw_ai_backend_panel`: renders the shared limit, the tab strip and the selected tab,
  calling the tab renderers of the sibling files.
- `AiBackendTab`, `visible_tabs`, `effective_tab`, `draw_tab_strip`: the sub-tabs.
- `OnnxConfigRead`, `OrtDownloadStatus`, `OnnxProviderOption`, `OnnxDeviceOptionUi`: the
  option and progress types held by the state and shared by the sibling files (kept here
  so their private fields stay visible to every section).
*/

use crate::ai_backend_supervisor::AiBackendHandle;
use crate::settings_warnings::{SettingChange, SettingGroupId, WarningSet, paint_corner_badge};
#[cfg(not(target_arch = "wasm32"))]
use crate::onnx_caps::OnnxCaps;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{Receiver, Sender};

mod backend_process;
#[cfg(not(target_arch = "wasm32"))]
mod model_manager;
mod onnx_selection;
#[cfg(not(target_arch = "wasm32"))]
mod ort_build;
#[cfg(not(target_arch = "wasm32"))]
mod runtime_section;
#[cfg(not(target_arch = "wasm32"))]
mod sdcpp_tab;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;

use backend_process::draw_pytorch_tab;
#[cfg(not(target_arch = "wasm32"))]
use model_manager::draw_shared_model_limit;
use onnx_selection::draw_onnx_section;
#[cfg(not(target_arch = "wasm32"))]
use runtime_section::draw_ai_runtime_section;

/// The sub-tabs of the AI pane, in strip order. Session state only (never persisted): the
/// pane opens on «Основной PyTorch», or on «ONNXruntime» where the Python backend is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum AiBackendTab {
    /// «Основной PyTorch»: the Python backend process, PyTorch device, large models,
    /// diagnostics.
    #[default]
    PyTorch,
    /// «ONNXruntime»: the runtime switch, ONNX provider / device / build, ORT download and
    /// crash-guard reset.
    Onnx,
    /// «Stable-Diffusion.cpp»: the sd.cpp build, its install / remove and status.
    SdCpp,
}

impl AiBackendTab {
    /// The localized tab label.
    fn label(self) -> &'static str {
        match self {
            AiBackendTab::PyTorch => t!("ai_backend.tab_pytorch"),
            AiBackendTab::Onnx => t!("ai_backend.tab_onnx"),
            AiBackendTab::SdCpp => t!("ai_backend.tab_sdcpp"),
        }
    }

    /// The settings-warnings group whose worst level badges this tab's label; `None` while
    /// no checked setting lives on the tab.
    const fn warning_group(self) -> Option<SettingGroupId> {
        match self {
            AiBackendTab::PyTorch => Some(SettingGroupId::AiTorch),
            AiBackendTab::Onnx => Some(SettingGroupId::AiOnnx),
            AiBackendTab::SdCpp => None,
        }
    }
}

/// The tabs this platform shows, in strip order: «Основной PyTorch» only where the Python
/// backend exists (`backend_platform`), «ONNXruntime» always, «Stable-Diffusion.cpp» only
/// where the sd.cpp runtime exists (`sdcpp_platform`). Both inputs are platform answers
/// (`PlatformFeature`), never `--no-ai`: a runtime switched off by the flag keeps its tab,
/// drawn disabled.
fn visible_tabs(backend_platform: bool, sdcpp_platform: bool) -> Vec<AiBackendTab> {
    let mut tabs = Vec::with_capacity(3);
    if backend_platform {
        tabs.push(AiBackendTab::PyTorch);
    }
    tabs.push(AiBackendTab::Onnx);
    if sdcpp_platform {
        tabs.push(AiBackendTab::SdCpp);
    }
    tabs
}

/// `selected` when it is among `tabs`, else the first visible tab (`Onnx` for an empty list,
/// which `visible_tabs` never returns).
fn effective_tab(selected: AiBackendTab, tabs: &[AiBackendTab]) -> AiBackendTab {
    if tabs.contains(&selected) { selected } else { tabs.first().copied().unwrap_or(AiBackendTab::Onnx) }
}

/// The house sub-tab strip (`ui.horizontal` + `selectable_label`): one label per tab, the
/// `current` one selected, each badged on its corner with the worst warning of its group
/// (launcher only; the studio passes `None`). Returns the tab the user clicked, if any.
fn draw_tab_strip(ui: &mut egui::Ui, tabs: &[AiBackendTab], current: AiBackendTab, warnings: Option<&WarningSet>) -> Option<AiBackendTab> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        for &tab in tabs {
            let response = ui.selectable_label(tab == current, tab.label());
            if let Some(level) = tab.warning_group().and_then(|group| warnings.and_then(|set| set.group_level(group))) {
                paint_corner_badge(ui.painter(), response.rect, level);
            }
            if response.clicked() && tab != current {
                clicked = Some(tab);
            }
        }
    });
    clicked
}

/// Per-UI scratch state for the shared panel. Each call site owns one instance so
/// the studio and launcher panels can hold independent in-progress selections.
#[derive(Debug, Default)]
pub struct AiBackendPanelState {
    /// The visible sub-tab (session only; reconciled against the platform every frame).
    tab: AiBackendTab,
    /// The «Stable-Diffusion.cpp» tab's state (build pick, install / remove job).
    #[cfg(not(target_arch = "wasm32"))]
    sdcpp: sdcpp_tab::SdCppTabState,
    /// The «Большие модели» section's own state (caps buffer, cap writer, pool actions).
    pub large_models: crate::large_models_panel::LargeModelsPanelState,
    pub selected_backend_device: String,
    pub selected_onnx_provider: String,
    pub selected_onnx_device_id: String,
    pub selected_max_loaded_models: u32,
    pub requested_initial_device_refresh: bool,
    /// Lazily-read AI runtime selection (`General.ai_runtime`). `None` until the
    /// one-shot background read completes, so the GUI thread never reads config
    /// directly. Desktop-only feature; unused on the web build.
    #[cfg(not(target_arch = "wasm32"))]
    pub ai_runtime_selection: std::sync::Arc<std::sync::Mutex<Option<ms_config::AiRuntime>>>,
    /// Whether the one-shot background read of `ai_runtime_selection` has started.
    #[cfg(not(target_arch = "wasm32"))]
    pub ai_runtime_read_started: bool,
    /// Lazily-probed ONNX capabilities (system CUDA + DirectML adapters) used to
    /// build the OFFLINE provider/device list. `None` until the one-shot background
    /// probe completes; the GUI thread never runs the blocking probes directly.
    /// Desktop-only feature; unused on the web build.
    #[cfg(not(target_arch = "wasm32"))]
    pub onnx_caps: std::sync::Arc<std::sync::Mutex<Option<OnnxCaps>>>,
    /// Whether the one-shot background ONNX capability probe has started.
    #[cfg(not(target_arch = "wasm32"))]
    pub onnx_caps_probe_started: bool,
    /// Lazily-read unified ONNX selection + model limit from config, used to seed the
    /// combos offline. `None` until the one-shot background read completes.
    #[cfg(not(target_arch = "wasm32"))]
    pub onnx_config: std::sync::Arc<std::sync::Mutex<Option<OnnxConfigRead>>>,
    /// Whether the one-shot background ONNX config read has started.
    #[cfg(not(target_arch = "wasm32"))]
    pub onnx_config_read_started: bool,
    /// Whether the in-memory ONNX provider/device selection has been seeded once from
    /// the persisted config. Seeding happens exactly once so a backend-only provider
    /// (e.g. MIGraphX) chosen by the user survives a transient backend outage — the
    /// selection is not reset just because the union temporarily drops it.
    #[cfg(not(target_arch = "wasm32"))]
    pub onnx_selection_seeded: bool,
    /// Progress/result of the auto-download of the onnxruntime dylib for the selected
    /// provider (Task 4). Written by a worker thread, rendered on the AI pane.
    #[cfg(not(target_arch = "wasm32"))]
    pub ort_download: std::sync::Arc<std::sync::Mutex<OrtDownloadStatus>>,
    /// Currently-selected onnxruntime BUILD slug (native runtime only). Seeded once from
    /// `General.ai_onnx_build` (or the per-OS default) via `onnx_build_seeded`.
    #[cfg(not(target_arch = "wasm32"))]
    pub selected_onnx_build: String,
    /// Whether `selected_onnx_build` has been seeded once from the persisted config.
    #[cfg(not(target_arch = "wasm32"))]
    pub onnx_build_seeded: bool,
    /// Per-build onnxruntime dylib presence cache (`slug -> Some(present)`), probed off
    /// the GUI thread. A key mapped to `None` marks an in-flight probe; absent means "not
    /// probed yet". Drives the build-action button (Retry vs load-other-build) and the
    /// auto-download gate. The blocking directory scan never runs on the GUI thread.
    #[cfg(not(target_arch = "wasm32"))]
    pub ort_build_present: std::sync::Arc<std::sync::Mutex<HashMap<String, Option<bool>>>>,
    /// Completion channel of the off-thread config writes: each saver sends its
    /// `SettingChange` after the write returned; drained into the outcome every frame.
    #[cfg(not(target_arch = "wasm32"))]
    pending_changes: PendingChanges,
    /// The device id of the last unified ONNX selection write that SUCCEEDED from this
    /// pane (set by the saver thread); `None` until one did. It supersedes the one-shot
    /// `onnx_config` seed as "the persisted id" when an explicit re-pick decides whether
    /// to save, so only a genuinely different (or a first fixing) pick writes.
    #[cfg(not(target_arch = "wasm32"))]
    onnx_device_saved: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl AiBackendPanelState {
    /// Appends every off-thread write that landed since the last drain to `changes`, in
    /// completion order, and returns the vector. Lets `SharedSettingsPanels::draw` report
    /// AI-pane writes while another shared section is drawn.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn append_landed_changes(&self, mut changes: Vec<SettingChange>) -> Vec<SettingChange> {
        self.pending_changes.drain_into(&mut changes);
        changes
    }

    /// Web build: no off-thread writer exists, so nothing ever lands; returns `changes`.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn append_landed_changes(&self, changes: Vec<SettingChange>) -> Vec<SettingChange> {
        changes
    }
}

/// The runtime effects of one [`draw_ai_backend_panel`] frame. The launcher forwards
/// `changed_settings` to its warning rechecks; the studio ignores the outcome.
#[must_use]
#[derive(Debug, Default)]
pub struct AiBackendPanelOutcome {
    /// Checked settings whose write landed (or whose autostart toggle was sent) this
    /// frame, in completion order. A write is reported whether it succeeded or failed:
    /// the recheck re-reads the disk, so a failure only re-confirms the old state.
    pub changed_settings: Vec<SettingChange>,
}

/// The completion channel of the pane's off-thread writes. A channel has no `Default`,
/// hence this wrapper with a manual one; the pane owns both ends, so the receiver never
/// sees a disconnect while the state lives.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct PendingChanges {
    tx: Sender<SettingChange>,
    rx: Receiver<SettingChange>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for PendingChanges {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self { tx, rx }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl PendingChanges {
    /// A sender for one writer thread, which reports through [`report_landed`].
    fn sender(&self) -> Sender<SettingChange> {
        self.tx.clone()
    }

    /// Moves every queued change into `out`, in send order, without blocking.
    fn drain_into(&self, out: &mut Vec<SettingChange>) {
        out.extend(self.rx.try_iter());
    }
}

/// Reports a landed write from a writer thread. A failed send means the pane state (and
/// its receiver) was dropped meanwhile: nobody is left to recheck, so it is only logged.
#[cfg(not(target_arch = "wasm32"))]
fn report_landed(tx: &Sender<SettingChange>, change: SettingChange) {
    if tx.send(change).is_err() {
        ms_log::runtime_log::log_info(format!(
            "[ai-backend-panel] settings pane closed before {change:?} landed; warning recheck skipped"
        ));
    }
}

/// The unified ONNX selection + model limit read once from config to seed the UI.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Default)]
pub struct OnnxConfigRead {
    /// Persisted ONNX Runtime build slug (`General.ai_onnx_build`), if any.
    pub build: Option<String>,
    /// Persisted ORT provider token (`General.ai_onnx_provider`), if any.
    pub provider_token: Option<String>,
    /// Persisted adapter index string (`General.ai_onnx_device_id`), if any.
    pub device_id: Option<String>,
    /// Persisted model limit (`General.ai_max_loaded_models`), clamped 1..=10.
    pub max_loaded_models: u32,
}

/// Progress/result of the onnxruntime dylib auto-download for one provider.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
pub struct OrtDownloadStatus {
    /// The build slug currently being (or last) downloaded. Keys the "one at a time" +
    /// "do not re-run a finished build" dedup and the presence-cache update on success.
    pub build_slug: Option<String>,
    /// Latest progress snapshot reported by the worker.
    pub progress: Option<ms_onnx_runtime::OrtDownloadProgress>,
    /// Whether a download worker is currently running.
    pub running: bool,
    /// Whether the last download finished successfully.
    pub done: bool,
    /// Error text when the last download failed.
    pub error: Option<String>,
    /// Whether the user cancelled the last download (its `.part` is kept; the panel
    /// offers to resume instead of auto-restarting it).
    pub cancelled: bool,
    /// Cancel flag of the running download, polled by `resolve_or_download_ort_dylib`;
    /// replaced by a fresh flag at every start.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// One selectable ONNX execution provider in the UNIFIED list (local-native ∪
/// backend-reported), tracking both native and backend facts so the label,
/// usability, device sourcing, and auto-download can be resolved per active runtime.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct OnnxProviderOption {
    /// ORT provider token (e.g. `"DmlExecutionProvider"`); persisted + sent to backend.
    token: String,
    /// Human-readable base label (availability suffix added at render time).
    label: String,
    /// Whether the token maps to a real native execution provider the in-process ONNX
    /// path can load (CPU/DirectML/CoreML/CUDA). CPU counts as native-capable;
    /// backend-only providers (e.g. MIGraphX, ROCm) do NOT.
    native_capable: bool,
    /// Local-capability heuristic of the Backend-runtime provider list (CPU always;
    /// DirectML iff a DirectML adapter was detected; CUDA iff the system CUDA 12 runtime
    /// is present; CoreML on macOS). NOT the native runtime's availability rule
    /// (`ms_native_runtime::native_fallback_reason`): its live use is the Backend
    /// default-provider seed (`default_onnx_provider`). Only meaningful when
    /// `native_capable`.
    native_available: bool,
    /// Whether the connected Python backend reported this provider in
    /// `available_onnx_providers` (always `false` when the backend is offline).
    backend_available: bool,
    /// Selectable adapter devices (id = adapter index string). Native-capable providers
    /// use the local probe devices; backend-only providers use the backend-reported
    /// device list.
    devices: Vec<OnnxDeviceOptionUi>,
}

/// One selectable ONNX device (accelerator adapter) in the offline list.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct OnnxDeviceOptionUi {
    /// Adapter index as a string (matches the backend `device_id` contract).
    id: String,
    /// Human-readable device label.
    label: String,
}

/// Renders the AI backend pane against the app-global `handle` and this surface's
/// `state`, and returns the setting changes that landed this frame.
///
/// `warnings` is the launcher's warning set (inline item badges); the studio passes
/// `None`, which draws the layout without badges. Config writes never run on the
/// GUI thread: they run on named workers and are reported here once they returned.
pub fn draw_ai_backend_panel(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
    warnings: Option<&WarningSet>,
) -> AiBackendPanelOutcome {
    let mut outcome = AiBackendPanelOutcome::default();
    let snapshot = handle.health_snapshot();
    // Whether this process may use the Python backend at all: `false` under `--no-ai` and on
    // a platform without it (`LocalAiAvailability`, built once in `run_main`).
    let ai_enabled = handle.local_ai.backend.is_ok();
    // The capability table decides whether this platform can run the Python backend at
    // all (not on Android). There the reason leads the pane, and the «Основной PyTorch» tab
    // (status, process control, log, PyTorch device, large models, diagnostics) and the
    // "Backend" runtime option are hidden: nothing in them could ever work.
    let backend_platform = ms_platform::PlatformFeature::PythonBackend.require();
    if let Err(unavailable) = &backend_platform {
        ui.colored_label(ms_theme::status::WARNING, unavailable.user_message());
    }

    // The model limit caps BOTH small-model LRUs (backend and native ONNX), so it sits above
    // the tabs instead of inside one runtime's tab. sd.cpp holds one model and ignores it.
    #[cfg(not(target_arch = "wasm32"))]
    draw_shared_model_limit(ui, handle, state, &snapshot);

    let tabs = visible_tabs(backend_platform.is_ok(), ms_platform::PlatformFeature::SdCppRuntime.is_available());
    let tab = effective_tab(state.tab, &tabs);
    state.tab = tab;
    if tabs.len() > 1 {
        if let Some(opened) = draw_tab_strip(ui, &tabs, tab, warnings) {
            state.tab = opened;
            #[cfg(not(target_arch = "wasm32"))]
            if opened == AiBackendTab::SdCpp {
                state.sdcpp.request_refresh_on_open();
            }
        }
        ui.separator();
    }

    match state.tab {
        AiBackendTab::PyTorch => {
            draw_pytorch_tab(ui, handle, state, &snapshot, warnings, &mut outcome);
        }
        AiBackendTab::Onnx => {
            // Native ONNX runtime selector (desktop-only; the native path depends on
            // `ms-onnx`/`ort`, compiled out on the web build).
            #[cfg(not(target_arch = "wasm32"))]
            draw_ai_runtime_section(ui, state, warnings, backend_platform.is_ok());
            // ONNX provider/device/build. On desktop this is populated from local OS/GPU
            // capabilities and stays usable even without a running backend; the same
            // selection drives the native path (config on load) and the backend (device.set
            // when connected).
            draw_onnx_section(ui, handle, state, &snapshot, ai_enabled, warnings);
        }
        AiBackendTab::SdCpp => {
            #[cfg(not(target_arch = "wasm32"))]
            sdcpp_tab::draw_sdcpp_tab(ui, handle, &mut state.sdcpp);
        }
    }

    // Drained last so a write that landed while this frame was drawn is reported now.
    outcome.changed_settings = state.append_landed_changes(outcome.changed_settings);
    outcome
}
