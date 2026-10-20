/*
FILE OVERVIEW: crates/ms-local-ai/src/backend_health.rs
Shared health helpers for the Python AI backend, used by the Settings panes, the backend
supervisor, the Translation and Cleaning tabs, the studio root and the launcher's reline flow.

Transport:
All backend requests go through the v2 framed IPC client (`ms_backend_ipc`),
which picks the transport (AF_UNIX on unix, loopback WebSocket on Windows). This
file owns no endpoint, socket or HTTP state.

Health transport (v2 push):
Health is delivered by the backend as `TOPIC_HEALTH` events pushed over the v2
frame socket (~once/sec). `spawn_ai_backend_probe` runs a background thread that
`subscribe(TOPIC_HEALTH)`s on the shared client and folds each pushed event header
`Value` into the shared `AiBackendHealthSnapshot`. A one-shot `call(METHOD_HEALTH)`
pull primes the snapshot on startup (before the first event) and serves as the
liveness probe; a failed `shared_client()` / dead subscription maps to the same
unchanged offline state. If the subscription's connection dies, the thread
re-subscribes via `shared_client()` (which auto-reconnects) so health resumes when
the backend comes back.

Main constants:
- `ai_backend_offline_error()`: unified user-facing message when health check fails.

Main types:
- `AiBackendHealthSnapshot`: last health result for UI status
  (`connected`, `details`, `checked_at`, backend version); `unavailable(LocalAiOff)` is the
  fixed snapshot of a process whose backend is off (`--no-ai` or platform).
- `AiBackendHealthSnapshot::large_pool`: the large-model pool part of the pushed
  `model_manager` payload, parsed by `large_models::parse_pool_status` (no polling).
- `TorchBuildInfo` / `TorchGpuInfo` / `KernelMismatch` / `TorchBuildError`: the snapshot's
  `torch` object (protocol v18: PyTorch build flavour + versions, visible GPUs, installed AMD
  per-arch kernel wheels) and the "main GPU's arch has no installed kernels" check
  (`TorchBuildInfo::kernel_mismatch`). Diagnostic only: shown in the AI backend pane.
- `AiBackendProbeCommand`: control messages for probe thread
  (`CheckNow`, `RefreshDeviceInfo`, `SetDevice`, `SetOnnxDevice`, `RefreshCudaDiagnostics`, `Stop`).

Main functions:
- `check_ai_backend_health`: one-shot v2 `health` pull readiness gate with a short
  timeout, returns a detailed error (used by reline + the probe priming step).
- `probe_ai_backend_once`: pulls one `health` snapshot and writes it into the shared snapshot.
- `apply_health_payload`: folds a health `Value` (pulled response OR pushed event header)
  into the shared snapshot — identical field mapping for both transports.
- `spawn_ai_backend_probe`: starts the background `TOPIC_HEALTH` subscription + device-control thread.
- `paddle_vl_ready_for`: readiness of the SELECTED PaddleOCR-VL variant (the backend reports
  one resident variant in `ocr.paddleocrvl.model`).
*/

use crate::large_models::{LargeModelsError, LargePoolStatus, parse_pool_status};
use crate::local_ai::LocalAiOff;
use ms_backend_ipc::{self as backend_ipc, CallError};
use ms_config::torch_choice::{GfxArch, TorchFlavour};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fmt;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use ms_thread::{self as thread, JoinHandle};
use web_time::{Duration, Instant};

/// Unified user-facing message shown when the AI backend is unreachable.
///
/// Runtime (not `const`) because `t!` is not const; the returned `&'static str`
/// is the active catalog value and stays safe to compare against as the
/// canonical offline message.
#[must_use]
pub fn ai_backend_offline_error() -> &'static str {
    t!("translation.backend_health.offline_error")
}

/// Cadence for control-command polling while no event is pending. The health
/// snapshot itself is push-driven (`TOPIC_HEALTH` events ~once/sec); this only
/// bounds how often the thread re-checks the (re)subscription liveness.
const AI_BACKEND_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Short timeout for the one-shot `health` pull (priming / liveness probe).
const AI_BACKEND_TIMEOUT: Duration = Duration::from_millis(700);
const AI_BACKEND_DEVICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const AI_BACKEND_DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct AiBackendDeviceOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct AiBackendHealthSnapshot {
    pub connected: bool,
    pub details: String,
    pub checked_at: Option<Instant>,
    pub backend_version: Option<String>,
    pub is_torch_available: Option<bool>,
    pub ocr_manga_ready: Option<bool>,
    pub ocr_easy_ready: Option<bool>,
    pub ocr_paddle_ready: Option<bool>,
    pub ocr_paddle_vl_ready: Option<bool>,
    /// PaddleOCR-VL variant key the backend has resident (`ocr.paddleocrvl.model`); its
    /// `ready` describes THIS variant only, see [`paddle_vl_ready_for`].
    pub ocr_paddle_vl_model: Option<String>,
    pub ocr_surya_ready: Option<bool>,
    /// Baberu OCR backend fallback engine readiness (`ocr.baberuocr.ready`).
    pub ocr_baberu_ready: Option<bool>,
    pub selected_device: Option<String>,
    pub available_devices: Vec<String>,
    pub device_options: Vec<AiBackendDeviceOption>,
    pub torch_device_needs_selection: bool,
    pub device_details: String,
    pub selected_onnx_provider: Option<String>,
    pub available_onnx_providers: Vec<String>,
    pub selected_onnx_device_id: Option<String>,
    pub onnx_device_options: Vec<AiBackendDeviceOption>,
    pub onnx_devices_by_provider: HashMap<String, Vec<AiBackendDeviceOption>>,
    pub onnx_device_needs_selection: bool,
    pub max_loaded_models: u32,
    pub onnx_details: String,
    pub device_checked_at: Option<Instant>,
    pub cuda_diagnostics: String,
    pub cuda_checked_at: Option<Instant>,
    /// The large-model pool as the last pushed snapshot reported it
    /// (`model_manager.large_models` + `model_manager.limits`). `None` while not connected,
    /// warming up, or when the snapshot carries no pool part; `Some(Err)` when the backend
    /// sent its `{"status":"error"}` placeholder or an unreadable shape.
    pub large_pool: Option<Result<LargePoolStatus, LargeModelsError>>,
    /// The backend's PyTorch build as the last pushed snapshot reported it (`torch`, protocol
    /// v18). `None` while not connected, warming up, or when the backend sent `null` (no torch,
    /// or a build fact it could not read); `Some(Err)` when the object has an unreadable shape
    /// or an unknown flavour token (shown in the pane, not logged per snapshot).
    pub torch_build: Option<Result<TorchBuildInfo, TorchBuildError>>,
}

impl Default for AiBackendHealthSnapshot {
    fn default() -> Self {
        Self {
            connected: false,
            details: t!("translation.backend_health.awaiting_first_probe_status").to_string(),
            checked_at: None,
            backend_version: None,
            is_torch_available: None,
            ocr_manga_ready: None,
            ocr_easy_ready: None,
            ocr_paddle_ready: None,
            ocr_paddle_vl_ready: None,
            ocr_paddle_vl_model: None,
            ocr_surya_ready: None,
            ocr_baberu_ready: None,
            selected_device: None,
            available_devices: Vec::new(),
            device_options: Vec::new(),
            torch_device_needs_selection: false,
            device_details: t!("translation.backend_health.devices_not_requested_status").to_string(),
            selected_onnx_provider: None,
            available_onnx_providers: Vec::new(),
            selected_onnx_device_id: None,
            onnx_device_options: Vec::new(),
            onnx_devices_by_provider: HashMap::new(),
            onnx_device_needs_selection: false,
            max_loaded_models: 3,
            onnx_details: t!("translation.backend_health.onnx_providers_not_requested_status").to_string(),
            device_checked_at: None,
            cuda_diagnostics: t!("translation.backend_health.cuda_diagnostics_not_run_status").to_string(),
            cuda_checked_at: None,
            large_pool: None,
            torch_build: None,
        }
    }
}

impl AiBackendHealthSnapshot {
    /// The final snapshot of a process whose Python backend is off (`--no-ai`, or a platform
    /// without it): `checked_at` is set and `connected` is false, so every "is the backend
    /// unavailable?" gate (`checked_at.is_some() && !connected`) refuses backend routes, and
    /// `details` names the real reason. No probe ever overwrites it (none is started).
    #[must_use]
    pub fn unavailable(off: LocalAiOff) -> Self {
        Self {
            connected: false,
            details: off.user_message(),
            checked_at: Some(Instant::now()),
            backend_version: None,
            is_torch_available: Some(false),
            ocr_manga_ready: None,
            ocr_easy_ready: None,
            ocr_paddle_ready: None,
            ocr_paddle_vl_ready: None,
            ocr_paddle_vl_model: None,
            ocr_surya_ready: None,
            ocr_baberu_ready: None,
            selected_device: None,
            available_devices: Vec::new(),
            device_options: Vec::new(),
            torch_device_needs_selection: false,
            device_details: t!("translation.backend_health.device_mgmt_unavailable_status").to_string(),
            selected_onnx_provider: None,
            available_onnx_providers: Vec::new(),
            selected_onnx_device_id: None,
            onnx_device_options: Vec::new(),
            onnx_devices_by_provider: HashMap::new(),
            onnx_device_needs_selection: false,
            max_loaded_models: 3,
            onnx_details: t!("translation.backend_health.onnx_mgmt_unavailable_status").to_string(),
            device_checked_at: None,
            cuda_diagnostics: t!("translation.backend_health.cuda_diagnostics_unavailable_status").to_string(),
            cuda_checked_at: None,
            large_pool: None,
            torch_build: None,
        }
    }
}

/// One GPU the backend's torch sees (`torch.devices[]` of the health snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorchGpuInfo {
    /// torch device index (`cuda:<index>`; ROCm builds use the same namespace).
    pub index: u32,
    /// Device name as torch reports it (may be empty).
    pub name: String,
    /// Base AMD architecture (`gfx1201`); `None` on CUDA devices.
    pub gcn_arch: Option<GfxArch>,
    /// Whether torch reports it as an integrated GPU (APU).
    pub integrated: bool,
}

/// The backend's PyTorch build and visible GPUs (`torch` object of the health snapshot,
/// protocol v18). Diagnostic only: nothing gates on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorchBuildInfo {
    pub flavour: TorchFlavour,
    /// `torch.__version__` verbatim (e.g. `2.13.0+rocm10.1.0`).
    pub version: String,
    /// `torch.version.hip`, ROCm builds only.
    pub hip_version: Option<String>,
    /// `torch.version.cuda` (CUDA builds; a ROCm build may report one too).
    pub cuda_version: Option<String>,
    /// Visible devices in index order.
    pub devices: Vec<TorchGpuInfo>,
    /// gfx archs of the installed `amd-torch-device-gfxN` wheels, sorted. Empty for CPU/CUDA
    /// builds AND for a pytorch.org ROCm build, which bundles its kernels instead.
    pub rocm_device_archs: Vec<GfxArch>,
    /// `rocm-sdk-core` is installed: the AMD whl-next layout, whose torch carries NO GPU kernels
    /// and gets each arch's kernels from its `amd-torch-device-gfxN` wheel. False for a
    /// pytorch.org ROCm build (kernels bundled) and for CPU/CUDA installs.
    pub rocm_split_kernels: bool,
}

/// The main GPU's architecture has no installed per-arch ROCm kernel wheel: torch will fail
/// (or fall back) on it. Produced by [`TorchBuildInfo::kernel_mismatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelMismatch {
    /// Architecture of the main GPU.
    pub device: GfxArch,
    /// The archs whose kernel wheels ARE installed; empty when none is (bare whl-next torch,
    /// no GPU kernels at all).
    pub installed: Vec<GfxArch>,
}

impl TorchBuildInfo {
    /// The GPU the user most likely runs on: the first discrete device, else the first device.
    /// `None` when torch sees no GPU.
    #[must_use]
    pub fn primary_device(&self) -> Option<&TorchGpuInfo> {
        self.devices.iter().find(|device| !device.integrated).or_else(|| self.devices.first())
    }

    /// `Some` when this is a split-kernel ROCm build ([`rocm_split_kernels`](Self::rocm_split_kernels))
    /// whose primary device ([`primary_device`](Self::primary_device)) has an arch not among
    /// [`rocm_device_archs`](Self::rocm_device_archs) — including an empty list (no kernels).
    ///
    /// `None` for CPU/CUDA builds, for a ROCm build with bundled kernels (pytorch.org: the
    /// installed wheel set says nothing about coverage there), and without a primary device or
    /// its arch.
    #[must_use]
    pub fn kernel_mismatch(&self) -> Option<KernelMismatch> {
        if self.flavour != TorchFlavour::Rocm || !self.rocm_split_kernels {
            return None;
        }
        let arch = self.primary_device()?.gcn_arch.as_ref()?;
        (!self.rocm_device_archs.contains(arch))
            .then(|| KernelMismatch { device: arch.clone(), installed: self.rocm_device_archs.clone() })
    }
}

/// Why the snapshot's `torch` object could not be read. The UI shows the technical
/// [`Display`](fmt::Display) form inside a localized line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorchBuildError {
    /// `flavour` is a string but not a known token (a newer backend); carries the token.
    UnknownFlavour(String),
    /// A required field is missing or has the wrong type / an unparsable value; names its path.
    Malformed { field: &'static str },
}

impl fmt::Display for TorchBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFlavour(token) => write!(f, "unknown torch flavour {token:?}"),
            Self::Malformed { field } => write!(f, "malformed torch field `{field}`"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AiBackendProbeCommand {
    CheckNow,
    RefreshDeviceInfo,
    SetDevice(String),
    SetOnnxDevice { provider: String, device_id: String },
    SetMaxLoadedModels(u32),
    RefreshCudaDiagnostics,
    Stop,
}

#[derive(Debug, Clone)]
struct AiBackendDeviceState {
    selected_device: String,
    available_devices: Vec<String>,
    device_options: Vec<AiBackendDeviceOption>,
    torch_device_needs_selection: bool,
    selected_onnx_provider: String,
    available_onnx_providers: Vec<String>,
    selected_onnx_device_id: String,
    onnx_device_options: Vec<AiBackendDeviceOption>,
    onnx_devices_by_provider: HashMap<String, Vec<AiBackendDeviceOption>>,
    onnx_device_needs_selection: bool,
    max_loaded_models: u32,
}

/// One-shot readiness gate: pulls the current `health` snapshot over the v2 frame
/// socket. Returns `Ok(())` if the backend answers a `health` request; any
/// connect / handshake / call failure is surfaced as a detailed error. Used both
/// by reline (gate before a long run) and by the probe thread to detect "backend
/// down" (its error maps to the unchanged offline state).
pub fn check_ai_backend_health() -> Result<(), String> {
    pull_health_snapshot(AI_BACKEND_TIMEOUT).map(|_| ())
}

/// Issues the one-shot v2 `health` pull and returns the response header `Value`
/// (the snapshot object). Mirrors the field shape of the pushed `TOPIC_HEALTH`
/// event, so callers map it identically via [`apply_health_payload`].
fn pull_health_snapshot(timeout: Duration) -> Result<Value, String> {
    let client = backend_ipc::shared_client()?;
    let (header, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_HEALTH,
            json!({}),
            &[],
            timeout,
        )
        .map_err(|err| match err {
            CallError::Error(msg) => msg,
            CallError::Interrupted(msg) => tf!("translation.backend_health.health_request_aborted_error", msg = msg),
            // A transport failure means the backend is offline / the socket is
            // dead; surface the unified offline message (matching device calls)
            // rather than the raw OS/framing string.
            CallError::Transport(_) => ai_backend_offline_error().to_string(),
        })?;
    Ok(header)
}

/// Fields parsed out of a health `Value` (one-shot pull response OR pushed
/// `TOPIC_HEALTH` event header). Both transports carry the same shape, so the
/// mapping lives in one place ([`parse_health_payload`]).
struct HealthFields {
    connected: bool,
    details: String,
    is_torch_available: Option<bool>,
    ocr_manga_ready: Option<bool>,
    ocr_easy_ready: Option<bool>,
    ocr_paddle_ready: Option<bool>,
    ocr_paddle_vl_ready: Option<bool>,
    ocr_paddle_vl_model: Option<String>,
    ocr_surya_ready: Option<bool>,
    ocr_baberu_ready: Option<bool>,
    backend_version: Option<String>,
    large_pool: Option<Result<LargePoolStatus, LargeModelsError>>,
    torch_build: Option<Result<TorchBuildInfo, TorchBuildError>>,
}

/// Maps a successful health payload (`ok`, `is_torch_available`, `ocr{...}`,
/// `backend_version`, optional `snapshot_state:"warming_up"`) to [`HealthFields`].
///
/// The warming-up snapshot has no `ocr` block, so the per-engine ready flags fall
/// back to `None` (UI shows "still warming up"), exactly as the old polled parse.
fn parse_health_payload(payload: &Value) -> HealthFields {
    HealthFields {
        connected: true,
        details: t!("translation.backend_health.state_received_status").to_string(),
        is_torch_available: payload.get("is_torch_available").and_then(Value::as_bool),
        ocr_manga_ready: health_ocr_ready_flag(payload, "mangaocr"),
        ocr_easy_ready: health_ocr_ready_flag(payload, "easyocr"),
        ocr_paddle_ready: health_ocr_ready_flag(payload, "paddleocr"),
        ocr_paddle_vl_ready: health_ocr_ready_flag(payload, "paddleocrvl"),
        ocr_paddle_vl_model: health_ocr_model(payload, "paddleocrvl"),
        ocr_surya_ready: health_ocr_ready_flag(payload, "suryaocr"),
        ocr_baberu_ready: health_ocr_ready_flag(payload, "baberuocr"),
        backend_version: health_backend_version(payload),
        large_pool: health_large_pool(payload),
        torch_build: health_torch_build(payload),
    }
}

/// The `torch` part of a health payload: `None` when absent (warming up, pre-v18) or `null`
/// (no torch / the backend could not read a fact), else the strict parse of the object.
/// Not logged here: the snapshot arrives about once a second, the pane shows the error.
fn health_torch_build(payload: &Value) -> Option<Result<TorchBuildInfo, TorchBuildError>> {
    match payload.get("torch") {
        None | Some(Value::Null) => None,
        Some(torch) => Some(parse_torch_build(torch)),
    }
}

/// Strict parse of a non-null `torch` object (shape: `PROTOCOL.md` §7 "`torch` payload").
/// Every key is required; `hip_version` / `cuda_version` / `gcn_arch` may be `null`.
fn parse_torch_build(torch: &Value) -> Result<TorchBuildInfo, TorchBuildError> {
    let object = torch.as_object().ok_or(TorchBuildError::Malformed { field: "torch" })?;
    let flavour_token = required_str(object, "flavour", "torch.flavour")?;
    let flavour = TorchFlavour::from_token(flavour_token)
        .ok_or_else(|| TorchBuildError::UnknownFlavour(flavour_token.to_string()))?;
    let devices = object
        .get("devices")
        .and_then(Value::as_array)
        .ok_or(TorchBuildError::Malformed { field: "torch.devices" })?
        .iter()
        .map(parse_torch_gpu)
        .collect::<Result<Vec<_>, _>>()?;
    let rocm_device_archs = object
        .get("rocm_device_archs")
        .and_then(Value::as_array)
        .ok_or(TorchBuildError::Malformed { field: "torch.rocm_device_archs" })?
        .iter()
        .map(|arch| arch.as_str().and_then(GfxArch::parse).ok_or(TorchBuildError::Malformed { field: "torch.rocm_device_archs[]" }))
        .collect::<Result<Vec<_>, _>>()?;
    let rocm_split_kernels = object
        .get("rocm_split_kernels")
        .and_then(Value::as_bool)
        .ok_or(TorchBuildError::Malformed { field: "torch.rocm_split_kernels" })?;
    Ok(TorchBuildInfo {
        flavour,
        version: required_str(object, "version", "torch.version")?.to_string(),
        hip_version: nullable_str(object, "hip_version", "torch.hip_version")?,
        cuda_version: nullable_str(object, "cuda_version", "torch.cuda_version")?,
        devices,
        rocm_device_archs,
        rocm_split_kernels,
    })
}

/// One `torch.devices[]` entry.
fn parse_torch_gpu(device: &Value) -> Result<TorchGpuInfo, TorchBuildError> {
    let object = device.as_object().ok_or(TorchBuildError::Malformed { field: "torch.devices[]" })?;
    let index = object
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .ok_or(TorchBuildError::Malformed { field: "torch.devices[].index" })?;
    let gcn_arch = match nullable_str(object, "gcn_arch", "torch.devices[].gcn_arch")? {
        None => None,
        Some(text) => Some(GfxArch::parse(&text).ok_or(TorchBuildError::Malformed { field: "torch.devices[].gcn_arch" })?),
    };
    let integrated = object
        .get("integrated")
        .and_then(Value::as_bool)
        .ok_or(TorchBuildError::Malformed { field: "torch.devices[].integrated" })?;
    Ok(TorchGpuInfo { index, name: required_str(object, "name", "torch.devices[].name")?.to_string(), gcn_arch, integrated })
}

/// A required string key; `field` is the path reported on failure.
fn required_str<'a>(object: &'a Map<String, Value>, key: &str, field: &'static str) -> Result<&'a str, TorchBuildError> {
    object.get(key).and_then(Value::as_str).ok_or(TorchBuildError::Malformed { field })
}

/// A required key whose value is a string or `null`; a blank string reads as `None`.
fn nullable_str(object: &Map<String, Value>, key: &str, field: &'static str) -> Result<Option<String>, TorchBuildError> {
    match object.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.trim()).filter(|text| !text.is_empty()).map(str::to_string)),
        _ => Err(TorchBuildError::Malformed { field }),
    }
}

/// The large-model pool part of a health payload: `None` when `model_manager` or either of
/// its two pool keys is absent (warming-up snapshot), else what the parser makes of them.
/// Not logged here: the snapshot arrives about once a second, the panel shows the error.
fn health_large_pool(payload: &Value) -> Option<Result<LargePoolStatus, LargeModelsError>> {
    let manager = payload.get("model_manager")?;
    Some(parse_pool_status(manager.get("large_models")?, manager.get("limits")?))
}

/// Folds parsed [`HealthFields`] into the shared snapshot and returns whether a
/// device-info refresh is warranted (first successful connect). Poison-safe: a
/// poisoned mutex is recovered via `get_mut`, mirroring the rest of this file.
fn apply_health_fields(
    snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>,
    fields: HealthFields,
) -> bool {
    let checked_at = Instant::now();
    ms_sysprobe::ai_backend_capabilities::set_torch_available(fields.is_torch_available);
    ms_sysprobe::ai_backend_capabilities::set_backend_available(Some(fields.connected));

    let mut guard = match snapshot.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    // Capture the previous connection state before overwriting it so we can detect
    // a disconnect->reconnect transition and force a fresh device re-enumeration
    // (a restarted backend may have different capabilities).
    let was_connected = guard.connected;
    guard.connected = fields.connected;
    guard.details = fields.details;
    guard.checked_at = Some(checked_at);
    guard.backend_version = fields.backend_version;
    guard.is_torch_available = fields.is_torch_available;
    guard.ocr_manga_ready = fields.ocr_manga_ready;
    guard.ocr_easy_ready = fields.ocr_easy_ready;
    guard.ocr_paddle_ready = fields.ocr_paddle_ready;
    guard.ocr_paddle_vl_ready = fields.ocr_paddle_vl_ready;
    guard.ocr_paddle_vl_model = fields.ocr_paddle_vl_model;
    guard.ocr_surya_ready = fields.ocr_surya_ready;
    guard.ocr_baberu_ready = fields.ocr_baberu_ready;
    guard.large_pool = fields.large_pool;
    guard.torch_build = fields.torch_build;
    // Backend onnxruntime reachability: available only while connected AND a
    // provider list has been enumerated. The retained list is not cleared on
    // disconnect, so on reconnect this restores immediately from the last-known
    // providers; the reconnect-triggered device refresh below then re-enumerates
    // and corrects it if the restarted backend lost onnxruntime. The native ORT
    // slot is independent (driven by the route-inputs refresh) and untouched here.
    let backend_ort = fields.connected && !guard.available_onnx_providers.is_empty();
    ms_sysprobe::ai_backend_capabilities::set_backend_ort_available(Some(backend_ort));
    // Refresh device info on the first connect (no prior enumeration) and on every
    // reconnect (false->true), so device/onnx capabilities never go stale after a
    // backend restart.
    fields.connected
        && (!was_connected
            || guard.device_checked_at.is_none()
            || guard.available_devices.is_empty())
}

/// Folds a health `Value` (pulled response OR pushed event header) into the shared
/// snapshot, triggering a one-time device-info refresh on the first connect.
fn apply_health_payload(snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>, payload: &Value) {
    let should_refresh_devices = apply_health_fields(snapshot, parse_health_payload(payload));
    if should_refresh_devices {
        refresh_ai_backend_device_info(snapshot);
    }
}

/// Records an offline/error health state in the shared snapshot (no device
/// refresh). `details` is the detailed failure message (e.g. transport error).
fn apply_health_offline(snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>, details: String) {
    apply_health_fields(
        snapshot,
        HealthFields {
            connected: false,
            details,
            is_torch_available: None,
            ocr_manga_ready: None,
            ocr_easy_ready: None,
            ocr_paddle_ready: None,
            ocr_paddle_vl_ready: None,
            ocr_paddle_vl_model: None,
            ocr_surya_ready: None,
            ocr_baberu_ready: None,
            backend_version: None,
            large_pool: None,
            torch_build: None,
        },
    );
}

pub fn probe_ai_backend_once(snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>) {
    match pull_health_snapshot(AI_BACKEND_TIMEOUT) {
        Ok(payload) => apply_health_payload(snapshot, &payload),
        Err(err) => apply_health_offline(snapshot, err),
    }
}

fn refresh_ai_backend_device_info(snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>) {
    ms_log::runtime_log::log_info("[ai_backend_probe] refresh_device_info start");
    let checked_at = Instant::now();
    match fetch_ai_backend_device_info() {
        Ok(device_state) => match snapshot.lock() {
            Ok(mut guard) => {
                ms_log::runtime_log::log_info(format!(
                    "[ai_backend_probe] refresh_device_info ok {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(&mut guard, &device_state);
                guard.device_details = tf!("translation.backend_health.current_pytorch_device_status", device = device_state.selected_device);
                guard.onnx_details = tf!("translation.backend_health.current_onnx_status", provider = device_state.selected_onnx_provider, device_id = device_state.selected_onnx_device_id);
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                ms_log::runtime_log::log_info(format!(
                    "[ai_backend_probe] refresh_device_info ok poisoned_lock {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(guard, &device_state);
                guard.device_details = tf!("translation.backend_health.current_pytorch_device_status", device = device_state.selected_device);
                guard.onnx_details = tf!("translation.backend_health.current_onnx_status", provider = device_state.selected_onnx_provider, device_id = device_state.selected_onnx_device_id);
                guard.device_checked_at = Some(checked_at);
            }
        },
        Err(err) => match snapshot.lock() {
            Ok(mut guard) => {
                ms_log::runtime_log::log_warn(format!(
                    "[ai_backend_probe] refresh_device_info failed: {err}"
                ));
                guard.device_details = err;
                guard.onnx_details = guard.device_details.clone();
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                ms_log::runtime_log::log_warn(format!(
                    "[ai_backend_probe] refresh_device_info failed poisoned_lock: {err}"
                ));
                guard.device_details = err;
                guard.onnx_details = guard.device_details.clone();
                guard.device_checked_at = Some(checked_at);
            }
        },
    }
}

fn set_ai_backend_device(snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>, device: String) {
    ms_log::runtime_log::log_info(format!(
        "[ai_backend_probe] set_torch_device start device={device}"
    ));
    let checked_at = Instant::now();
    match apply_ai_backend_device(&device) {
        Ok(device_state) => match snapshot.lock() {
            Ok(mut guard) => {
                ms_log::runtime_log::log_info(format!(
                    "[ai_backend_probe] set_torch_device ok {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(&mut guard, &device_state);
                guard.device_details = tf!("translation.backend_health.pytorch_device_applied_status", device = device_state.selected_device);
                guard.onnx_details = tf!("translation.backend_health.current_onnx_status", provider = device_state.selected_onnx_provider, device_id = device_state.selected_onnx_device_id);
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                ms_log::runtime_log::log_info(format!(
                    "[ai_backend_probe] set_torch_device ok poisoned_lock {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(guard, &device_state);
                guard.device_details = tf!("translation.backend_health.pytorch_device_applied_status", device = device_state.selected_device);
                guard.onnx_details = tf!("translation.backend_health.current_onnx_status", provider = device_state.selected_onnx_provider, device_id = device_state.selected_onnx_device_id);
                guard.device_checked_at = Some(checked_at);
            }
        },
        Err(err) => match snapshot.lock() {
            Ok(mut guard) => {
                ms_log::runtime_log::log_warn(format!(
                    "[ai_backend_probe] set_torch_device failed: {err}"
                ));
                guard.device_details = err;
                guard.onnx_details = guard.device_details.clone();
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                ms_log::runtime_log::log_warn(format!(
                    "[ai_backend_probe] set_torch_device failed poisoned_lock: {err}"
                ));
                guard.device_details = err;
                guard.onnx_details = guard.device_details.clone();
                guard.device_checked_at = Some(checked_at);
            }
        },
    }
}

fn set_ai_backend_onnx_device(
    snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>,
    provider: String,
    device_id: String,
) {
    ms_log::runtime_log::log_info(format!(
        "[ai_backend_probe] set_onnx_device start provider={provider} device_id={device_id}"
    ));
    let checked_at = Instant::now();
    match apply_ai_backend_onnx_device(&provider, &device_id) {
        Ok(device_state) => match snapshot.lock() {
            Ok(mut guard) => {
                ms_log::runtime_log::log_info(format!(
                    "[ai_backend_probe] set_onnx_device ok {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(&mut guard, &device_state);
                guard.device_details = tf!("translation.backend_health.current_pytorch_device_status", device = device_state.selected_device);
                guard.onnx_details = tf!("translation.backend_health.onnx_device_applied_status", provider = device_state.selected_onnx_provider, device_id = device_state.selected_onnx_device_id);
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                ms_log::runtime_log::log_info(format!(
                    "[ai_backend_probe] set_onnx_device ok poisoned_lock {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(guard, &device_state);
                guard.device_details = tf!("translation.backend_health.current_pytorch_device_status", device = device_state.selected_device);
                guard.onnx_details = tf!("translation.backend_health.onnx_device_applied_status", provider = device_state.selected_onnx_provider, device_id = device_state.selected_onnx_device_id);
                guard.device_checked_at = Some(checked_at);
            }
        },
        Err(err) => match snapshot.lock() {
            Ok(mut guard) => {
                ms_log::runtime_log::log_warn(format!(
                    "[ai_backend_probe] set_onnx_device failed: {err}"
                ));
                guard.onnx_details = err;
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                ms_log::runtime_log::log_warn(format!(
                    "[ai_backend_probe] set_onnx_device failed poisoned_lock: {err}"
                ));
                guard.onnx_details = err;
                guard.device_checked_at = Some(checked_at);
            }
        },
    }
}

fn set_ai_backend_max_loaded_models(
    snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>,
    max_loaded_models: u32,
) {
    let checked_at = Instant::now();
    match apply_ai_backend_max_loaded_models(max_loaded_models) {
        Ok(device_state) => match snapshot.lock() {
            Ok(mut guard) => {
                apply_device_state_snapshot(&mut guard, &device_state);
                guard.device_details = tf!("translation.backend_health.max_models_applied_status", limit = device_state.max_loaded_models);
                guard.onnx_details = tf!("translation.backend_health.current_onnx_status", provider = device_state.selected_onnx_provider, device_id = device_state.selected_onnx_device_id);
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                apply_device_state_snapshot(guard, &device_state);
                guard.device_details = tf!("translation.backend_health.max_models_applied_status", limit = device_state.max_loaded_models);
                guard.onnx_details = tf!("translation.backend_health.current_onnx_status", provider = device_state.selected_onnx_provider, device_id = device_state.selected_onnx_device_id);
                guard.device_checked_at = Some(checked_at);
            }
        },
        Err(err) => match snapshot.lock() {
            Ok(mut guard) => {
                guard.device_details = err;
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                guard.device_details = err;
                guard.device_checked_at = Some(checked_at);
            }
        },
    }
}

fn refresh_ai_backend_cuda_diagnostics(snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>) {
    let checked_at = Instant::now();
    match fetch_ai_backend_cuda_diagnostics() {
        Ok(diagnostics) => match snapshot.lock() {
            Ok(mut guard) => {
                guard.cuda_diagnostics = diagnostics;
                guard.cuda_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                guard.cuda_diagnostics = diagnostics;
                guard.cuda_checked_at = Some(checked_at);
            }
        },
        Err(err) => match snapshot.lock() {
            Ok(mut guard) => {
                guard.cuda_diagnostics = err;
                guard.cuda_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                guard.cuda_diagnostics = err;
                guard.cuda_checked_at = Some(checked_at);
            }
        },
    }
}

fn fetch_ai_backend_device_info() -> Result<AiBackendDeviceState, String> {
    let started_at = Instant::now();
    let payload = device_call(
        backend_ipc::protocol::METHOD_DEVICE_GET,
        json!({}),
        AI_BACKEND_DEVICE_REQUEST_TIMEOUT,
    )?;
    ms_log::runtime_log::log_info(format!(
        "[ai_backend_probe] GET /device response elapsed_ms={} {}",
        started_at.elapsed().as_millis(),
        summarize_device_payload(&payload)
    ));
    parse_device_state_payload(&payload)
}

fn health_ocr_ready_flag(payload: &Value, engine_key: &str) -> Option<bool> {
    payload
        .get("ocr")
        .and_then(Value::as_object)
        .and_then(|ocr| ocr.get(engine_key))
        .and_then(Value::as_object)
        .and_then(|service| service.get("ready"))
        .and_then(Value::as_bool)
}

/// The trimmed non-empty `ocr.<engine_key>.model` string of a health payload (the
/// PaddleOCR-VL variant the backend has resident), `None` when absent.
fn health_ocr_model(payload: &Value, engine_key: &str) -> Option<String> {
    payload
        .get("ocr")
        .and_then(Value::as_object)
        .and_then(|ocr| ocr.get(engine_key))
        .and_then(Value::as_object)
        .and_then(|service| service.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// PaddleOCR-VL readiness of the SELECTED variant `selected_key`: the backend reports one
/// resident variant, so its `ready` holds only when `model` names `selected_key`; a ready
/// engine of another variant means the selected one is not loaded (`Some(false)`). `None`
/// stays `None` (no health block yet), so a variant switch never shows a stale "ready".
#[must_use]
pub fn paddle_vl_ready_for(snapshot: &AiBackendHealthSnapshot, selected_key: &str) -> Option<bool> {
    let ready = snapshot.ocr_paddle_vl_ready?;
    Some(ready && snapshot.ocr_paddle_vl_model.as_deref() == Some(selected_key))
}

fn health_backend_version(payload: &Value) -> Option<String> {
    payload
        .get("backend_version")
        .or_else(|| payload.get("version"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn apply_ai_backend_device(device: &str) -> Result<AiBackendDeviceState, String> {
    let started_at = Instant::now();
    let payload = device_call(
        backend_ipc::protocol::METHOD_DEVICE_SET,
        device_set_header(Some(device), None, None, None),
        AI_BACKEND_DEVICE_REQUEST_TIMEOUT,
    )?;
    ms_log::runtime_log::log_info(format!(
        "[ai_backend_probe] POST /device/set torch response elapsed_ms={} {}",
        started_at.elapsed().as_millis(),
        summarize_device_payload(&payload)
    ));
    parse_device_state_payload(&payload)
}

fn apply_ai_backend_onnx_device(
    provider: &str,
    device_id: &str,
) -> Result<AiBackendDeviceState, String> {
    let started_at = Instant::now();
    let payload = device_call(
        backend_ipc::protocol::METHOD_DEVICE_SET,
        device_set_header(None, Some(provider), Some(device_id), None),
        AI_BACKEND_DEVICE_REQUEST_TIMEOUT,
    )?;
    ms_log::runtime_log::log_info(format!(
        "[ai_backend_probe] POST /device/set onnx response elapsed_ms={} {}",
        started_at.elapsed().as_millis(),
        summarize_device_payload(&payload)
    ));
    parse_device_state_payload(&payload)
}

fn apply_ai_backend_max_loaded_models(
    max_loaded_models: u32,
) -> Result<AiBackendDeviceState, String> {
    let started_at = Instant::now();
    let payload = device_call(
        backend_ipc::protocol::METHOD_DEVICE_SET,
        device_set_header(None, None, None, Some(max_loaded_models)),
        AI_BACKEND_DEVICE_REQUEST_TIMEOUT,
    )?;
    ms_log::runtime_log::log_info(format!(
        "[ai_backend_probe] POST /device/set max_models response elapsed_ms={} {}",
        started_at.elapsed().as_millis(),
        summarize_device_payload(&payload)
    ));
    parse_device_state_payload(&payload)
}

fn fetch_ai_backend_cuda_diagnostics() -> Result<String, String> {
    let payload = device_call(
        backend_ipc::protocol::METHOD_DEVICE_CUDA_DIAGNOSTICS,
        json!({}),
        AI_BACKEND_DIAGNOSTICS_TIMEOUT,
    )?;
    let diagnostics = payload
        .get("diagnostics")
        .ok_or_else(|| t!("translation.backend_health.diagnostics_field_missing_error").to_string())?;
    render_cuda_diagnostics(diagnostics)
        .ok_or_else(|| t!("translation.backend_health.diagnostics_field_empty_error").to_string())
}

/// Turns the `diagnostics` value from `device.cuda_diagnostics` into the
/// human-readable text the settings UI shows in a monospace block.
///
/// The backend currently returns `diagnostics` as a plain string, but the IPC
/// handler contract also allows an object (structured CUDA/ROCm probe result).
/// A string is rendered verbatim (trimmed); an object/array is pretty-printed as
/// JSON. Returns `None` for a genuinely empty / null value so the caller can keep
/// the "empty diagnostics → error" behaviour.
fn render_cuda_diagnostics(diagnostics: &Value) -> Option<String> {
    match diagnostics {
        Value::Null => None,
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Object(map) if map.is_empty() => None,
        Value::Array(items) if items.is_empty() => None,
        other => serde_json::to_string_pretty(other)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    }
}

fn parse_device_state_payload(payload: &Value) -> Result<AiBackendDeviceState, String> {
    ms_log::runtime_log::log_info(format!(
        "[ai_backend_probe] parse_device_state start {}",
        summarize_device_payload(payload)
    ));
    let selected = payload
        .get("selected_device")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| t!("translation.backend_health.selected_device_missing_error").to_string())?;

    let mut devices = payload
        .get("available_devices")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();

    if devices.is_empty() {
        devices.push("cpu".to_string());
    }
    if !devices.iter().any(|item| item == &selected) {
        devices.insert(0, selected.clone());
    }

    let mut deduped: Vec<String> = Vec::with_capacity(devices.len());
    for item in devices {
        if !deduped.iter().any(|existing| existing == &item) {
            deduped.push(item);
        }
    }

    let device_options = parse_named_options(
        payload.get("available_device_options"),
        &deduped,
        Some(&selected),
    );

    let mut providers = payload
        .get("available_onnx_providers")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();

    let selected_onnx_provider = payload
        .get("selected_onnx_provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "CPUExecutionProvider".to_string());

    if providers.is_empty() {
        providers.push(selected_onnx_provider.clone());
    } else if !providers
        .iter()
        .any(|provider| provider == &selected_onnx_provider)
    {
        providers.insert(0, selected_onnx_provider.clone());
    }
    let mut provider_deduped = Vec::with_capacity(providers.len());
    for provider in providers {
        if !provider_deduped
            .iter()
            .any(|existing| existing == &provider)
        {
            provider_deduped.push(provider);
        }
    }

    let selected_onnx_device_id = payload
        .get("selected_onnx_device_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "0".to_string());

    let onnx_devices_by_provider =
        parse_onnx_devices_by_provider(payload.get("available_onnx_devices_by_provider"));
    let onnx_device_options = onnx_devices_by_provider
        .get(&selected_onnx_provider)
        .cloned()
        .filter(|options| {
            options
                .iter()
                .any(|option| option.id == selected_onnx_device_id)
        })
        .unwrap_or_else(|| {
            parse_named_options(
                payload.get("available_onnx_device_options"),
                &[],
                Some(&selected_onnx_device_id),
            )
        });

    let state = AiBackendDeviceState {
        selected_device: selected,
        available_devices: deduped,
        device_options,
        torch_device_needs_selection: payload
            .get("torch_device_needs_selection")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        selected_onnx_provider,
        available_onnx_providers: provider_deduped,
        selected_onnx_device_id,
        onnx_device_options,
        onnx_devices_by_provider,
        onnx_device_needs_selection: payload
            .get("onnx_device_needs_selection")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        max_loaded_models: payload
            .get("max_loaded_models")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .map(|value| value.clamp(1, 10))
            .unwrap_or(3),
    };
    ms_log::runtime_log::log_info(format!(
        "[ai_backend_probe] parse_device_state ok {}",
        summarize_device_state(&state)
    ));
    Ok(state)
}

fn summarize_device_payload(payload: &Value) -> String {
    let torch_options = payload
        .get("available_device_options")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let torch_devices = payload
        .get("available_devices")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let onnx_providers = payload
        .get("available_onnx_providers")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let onnx_options = payload
        .get("available_onnx_device_options")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let provider_counts = payload
        .get("available_onnx_devices_by_provider")
        .and_then(Value::as_object)
        .map(|providers| {
            let mut parts = providers
                .iter()
                .map(|(provider, options)| {
                    let count = options.as_array().map_or(0, Vec::len);
                    format!("{provider}:{count}")
                })
                .collect::<Vec<_>>();
            parts.sort();
            parts.join(",")
        })
        .unwrap_or_default();
    format!(
        "selected_device={:?} torch_devices={} torch_options={} torch_needs_selection={:?} \
         selected_onnx_provider={:?} onnx_providers={} selected_onnx_device_id={:?} \
         onnx_options={} onnx_provider_counts=[{}] onnx_needs_selection={:?}",
        payload.get("selected_device").and_then(Value::as_str),
        torch_devices,
        torch_options,
        payload
            .get("torch_device_needs_selection")
            .and_then(Value::as_bool),
        payload
            .get("selected_onnx_provider")
            .and_then(Value::as_str),
        onnx_providers,
        payload
            .get("selected_onnx_device_id")
            .and_then(Value::as_str),
        onnx_options,
        provider_counts,
        payload
            .get("onnx_device_needs_selection")
            .and_then(Value::as_bool)
    )
}

fn summarize_device_state(device_state: &AiBackendDeviceState) -> String {
    let mut provider_counts = device_state
        .onnx_devices_by_provider
        .iter()
        .map(|(provider, options)| format!("{provider}:{}", options.len()))
        .collect::<Vec<_>>();
    provider_counts.sort();
    format!(
        "selected_device={} available_devices={} device_options={} \
         torch_needs_selection={} selected_onnx_provider={} onnx_providers={} \
         selected_onnx_device_id={} onnx_options={} onnx_provider_counts=[{}] \
         onnx_needs_selection={} max_loaded_models={}",
        device_state.selected_device,
        device_state.available_devices.len(),
        device_state.device_options.len(),
        device_state.torch_device_needs_selection,
        device_state.selected_onnx_provider,
        device_state.available_onnx_providers.len(),
        device_state.selected_onnx_device_id,
        device_state.onnx_device_options.len(),
        provider_counts.join(","),
        device_state.onnx_device_needs_selection,
        device_state.max_loaded_models
    )
}

fn parse_named_options(
    payload: Option<&Value>,
    fallback_ids: &[String],
    ensure_id: Option<&str>,
) -> Vec<AiBackendDeviceOption> {
    let mut labels_by_id: HashMap<String, String> = HashMap::new();
    let mut ids: Vec<String> = Vec::new();
    if let Some(options) = payload.and_then(Value::as_array) {
        for option in options {
            let Some(id) = option
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            else {
                continue;
            };

            let label = option
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| id.clone());
            ids.push(id.clone());
            labels_by_id.insert(id, label);
        }
    }

    ids.extend(fallback_ids.iter().cloned());
    if let Some(id) = ensure_id {
        ids.push(id.to_string());
    }

    let mut deduped = Vec::with_capacity(ids.len());
    for id in ids {
        if !deduped.iter().any(|existing| existing == &id) {
            deduped.push(id);
        }
    }

    deduped
        .into_iter()
        .map(|id| AiBackendDeviceOption {
            label: labels_by_id.get(&id).cloned().unwrap_or_else(|| id.clone()),
            id,
        })
        .collect()
}

fn parse_onnx_devices_by_provider(
    payload: Option<&Value>,
) -> HashMap<String, Vec<AiBackendDeviceOption>> {
    let mut mapping = HashMap::new();
    let Some(object) = payload.and_then(Value::as_object) else {
        return mapping;
    };

    for (provider, options_payload) in object {
        let provider_key = provider.trim();
        if provider_key.is_empty() {
            continue;
        }
        mapping.insert(
            provider_key.to_string(),
            parse_named_options(Some(options_payload), &[], None),
        );
    }

    mapping
}

fn apply_device_state_snapshot(
    snapshot: &mut AiBackendHealthSnapshot,
    device_state: &AiBackendDeviceState,
) {
    snapshot.selected_device = Some(device_state.selected_device.clone());
    snapshot.available_devices = device_state.available_devices.clone();
    snapshot.device_options = device_state.device_options.clone();
    snapshot.torch_device_needs_selection = device_state.torch_device_needs_selection;
    snapshot.selected_onnx_provider = Some(device_state.selected_onnx_provider.clone());
    snapshot.available_onnx_providers = device_state.available_onnx_providers.clone();
    // Providers are enumerated by the backend via onnxruntime, so a non-empty list
    // is authoritative that the backend's onnxruntime works (CPUExecutionProvider is
    // always present when onnxruntime imports; empty => not-yet-queried or broken).
    ms_sysprobe::ai_backend_capabilities::set_backend_ort_available(Some(
        !device_state.available_onnx_providers.is_empty(),
    ));
    snapshot.selected_onnx_device_id = Some(device_state.selected_onnx_device_id.clone());
    snapshot.onnx_device_options = device_state.onnx_device_options.clone();
    snapshot.onnx_devices_by_provider = device_state.onnx_devices_by_provider.clone();
    snapshot.onnx_device_needs_selection = device_state.onnx_device_needs_selection;
    snapshot.max_loaded_models = device_state.max_loaded_models;
}

/// Issues a blocking v2 framed device call (`device.get` / `device.set` /
/// `device.cuda_diagnostics`). These methods carry no images: the request blob is
/// empty and the result is the response header `Value` (the response blob is
/// ignored). The readiness gate mirrors the other migrated subsystems: a failed
/// `shared_client()` (backend offline / handshake failure) maps to
/// [`ai_backend_offline_error`].
///
/// `CallError` mapping preserves the previous device UX:
/// - `Error`       → the backend error message (same as the old HTTP 4xx/5xx).
/// - `Interrupted` → transient abort surfaced to the device status line.
/// - `Transport`   → connect/framing failure (backend offline path).
fn device_call(method: &str, header_fields: Value, timeout: Duration) -> Result<Value, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (header, _blob) = client
        .call(method, header_fields, &[], timeout)
        .map_err(map_device_call_error)?;
    Ok(header)
}

/// Shared `CallError` → user-facing `String` mapping for device calls.
fn map_device_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => tf!("translation.backend_health.request_aborted_error", msg = msg),
        CallError::Transport(_) => ai_backend_offline_error().to_string(),
    }
}

/// Builds the inline `device.set` request header from the four optional fields.
/// Absent/`None` fields are omitted entirely so the backend leaves them
/// unchanged (matching the legacy partial-body POST semantics).
fn device_set_header(
    device: Option<&str>,
    onnx_provider: Option<&str>,
    onnx_device_id: Option<&str>,
    max_loaded_models: Option<u32>,
) -> Value {
    let mut header = serde_json::Map::new();
    if let Some(device) = device {
        header.insert("device".to_string(), json!(device));
    }
    if let Some(provider) = onnx_provider {
        header.insert("onnx_provider".to_string(), json!(provider));
    }
    if let Some(device_id) = onnx_device_id {
        header.insert("onnx_device_id".to_string(), json!(device_id));
    }
    if let Some(max_loaded_models) = max_loaded_models {
        header.insert("max_loaded_models".to_string(), json!(max_loaded_models));
    }
    Value::Object(header)
}

/// If no pushed `TOPIC_HEALTH` event has been folded into the snapshot within this
/// window, the probe thread falls back to a one-shot `health` pull. This bounds how
/// long the UI can stay stale if events stop arriving (e.g. the backend went down
/// and the subscription has not yet been observed dead).
const AI_BACKEND_EVENT_STALENESS: Duration = Duration::from_secs(5);

/// (Re)subscribes to `TOPIC_HEALTH` on the shared v2 client. On success the
/// snapshot is primed with a one-shot pull (so the UI has a current state before
/// the first pushed event arrives). On failure (backend not running / handshake
/// failure) the snapshot is marked offline and `None` is returned; the caller
/// retries on its next tick (`shared_client()` auto-reconnects when the backend
/// comes back).
fn subscribe_health(snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>) -> Option<Receiver<Value>> {
    match backend_ipc::shared_client() {
        Ok(client) => {
            let rx = client.subscribe(backend_ipc::protocol::TOPIC_HEALTH);
            // Prime immediately so the UI is not blank until the next ~1s push, and
            // so a backend that is up but momentarily quiet still shows connected.
            probe_ai_backend_once(snapshot);
            Some(rx)
        }
        Err(_) => {
            apply_health_offline(snapshot, ai_backend_offline_error().to_string());
            None
        }
    }
}

/// Drains every currently-pending pushed `TOPIC_HEALTH` event into the snapshot
/// without blocking, returning:
/// - `Ok(true)`  — at least one event was folded in,
/// - `Ok(false)` — no event was pending (channel still live),
/// - `Err(())`   — the subscription channel disconnected (connection died); the
///   caller must re-subscribe.
fn drain_health_events(
    rx: &Receiver<Value>,
    snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>,
) -> Result<bool, ()> {
    let mut got_event = false;
    loop {
        match rx.try_recv() {
            Ok(event) => {
                apply_health_payload(snapshot, &event);
                got_event = true;
            }
            Err(mpsc::TryRecvError::Empty) => return Ok(got_event),
            Err(mpsc::TryRecvError::Disconnected) => return Err(()),
        }
    }
}

/// Starts the background health/device-control thread.
///
/// Health is push-driven: the thread subscribes to `TOPIC_HEALTH` on the shared v2
/// client and folds each pushed event header into `snapshot` (events arrive
/// ~once/sec from the backend's snapshot worker). It never blocks on events — it
/// drains them non-blockingly each tick and otherwise blocks only on the control
/// command channel with a short timeout, so device commands stay responsive and
/// the egui frame (which only reads `snapshot`) is never blocked.
///
/// Liveness/recovery: a one-shot `health` pull primes the snapshot on (re)connect;
/// if the subscription disconnects (connection died) the thread re-subscribes via
/// `shared_client()` (auto-reconnects), and if no event has arrived within
/// [`AI_BACKEND_EVENT_STALENESS`] it falls back to a one-shot pull (which also
/// surfaces the unchanged offline state when the backend is down).
pub fn spawn_ai_backend_probe(
    snapshot: Arc<Mutex<AiBackendHealthSnapshot>>,
) -> (Sender<AiBackendProbeCommand>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<AiBackendProbeCommand>();
    let handle = thread::spawn(move || {
        let mut health_rx = subscribe_health(&snapshot);
        let mut last_event_at = Instant::now();
        loop {
            // 1. Fold in any pushed health events (non-blocking). Re-subscribe if the
            //    subscription channel died with the connection.
            match &health_rx {
                Some(rx) => match drain_health_events(rx, &snapshot) {
                    Ok(true) => last_event_at = Instant::now(),
                    Ok(false) => {
                        // No event yet: if we have been quiet too long, pull once so
                        // a silently-dead backend is surfaced as offline promptly.
                        if last_event_at.elapsed() >= AI_BACKEND_EVENT_STALENESS {
                            probe_ai_backend_once(&snapshot);
                            last_event_at = Instant::now();
                        }
                    }
                    Err(()) => {
                        // Subscription died (connection dropped). Re-subscribe; this
                        // also re-primes the snapshot and marks offline on failure.
                        health_rx = subscribe_health(&snapshot);
                        last_event_at = Instant::now();
                    }
                },
                None => {
                    // Previous subscribe failed (backend down): retry on each tick so
                    // health resumes when the backend comes back.
                    health_rx = subscribe_health(&snapshot);
                    last_event_at = Instant::now();
                }
            }

            // 2. Service one control command (or time out to re-poll events).
            match rx.recv_timeout(AI_BACKEND_POLL_INTERVAL) {
                Ok(AiBackendProbeCommand::CheckNow) => probe_ai_backend_once(&snapshot),
                Ok(AiBackendProbeCommand::RefreshDeviceInfo) => {
                    refresh_ai_backend_device_info(&snapshot)
                }
                Ok(AiBackendProbeCommand::SetDevice(device)) => {
                    set_ai_backend_device(&snapshot, device)
                }
                Ok(AiBackendProbeCommand::SetOnnxDevice {
                    provider,
                    device_id,
                }) => set_ai_backend_onnx_device(&snapshot, provider, device_id),
                Ok(AiBackendProbeCommand::SetMaxLoadedModels(max_loaded_models)) => {
                    set_ai_backend_max_loaded_models(&snapshot, max_loaded_models)
                }
                Ok(AiBackendProbeCommand::RefreshCudaDiagnostics) => {
                    refresh_ai_backend_cuda_diagnostics(&snapshot)
                }
                Ok(AiBackendProbeCommand::Stop) => break,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    (tx, handle)
}

#[cfg(test)]
mod tests {
    use super::{
        AiBackendHealthSnapshot, apply_health_fields, device_set_header, health_backend_version,
        map_device_call_error, parse_device_state_payload, parse_health_payload,
        render_cuda_diagnostics,
    };
    use ms_backend_ipc::CallError;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    #[test]
    fn reads_backend_version_from_health_payload() {
        let payload = json!({"backend_version": " 3.4.0 "});
        assert_eq!(health_backend_version(&payload).as_deref(), Some("3.4.0"));
    }

    /// A full `TOPIC_HEALTH` event header (same shape as the old polled `/health`
    /// JSON) maps to the connected snapshot with the per-engine ready flags and
    /// version extracted identically. Drives `apply_health_fields` directly so the
    /// test stays in-process (no device-refresh network call on first connect).
    #[test]
    fn applies_full_health_event_to_snapshot() {
        let snapshot = Arc::new(Mutex::new(AiBackendHealthSnapshot::default()));
        // Pre-seed a steady-state connected snapshot with device info already
        // enumerated: apply_health_fields must then report no refresh need (this is
        // not a disconnect->reconnect transition). We assert the returned flag below.
        {
            let mut g = snapshot.lock().unwrap();
            g.connected = true;
            g.device_checked_at = Some(web_time::Instant::now());
            g.available_devices = vec!["cpu".to_string()];
        }

        // Mirrors the pushed event header: kind/topic/id are present alongside the
        // snapshot fields; `parse_health_payload` ignores the framing fields.
        let event = json!({
            "kind": "event",
            "topic": "health",
            "id": 0,
            "ok": true,
            "service": "mf_ai_backend",
            "backend_version": " 3.4.2 ",
            "snapshot_unix_s": 1.0,
            "is_torch_available": true,
            "ocr": {
                "mangaocr": { "ready": true },
                "easyocr": { "ready": false },
                "paddleocr": { "ready": true },
                "paddleocrvl": { "ready": false, "device": "cpu", "model": " manga_ja ", "last_error": null },
                "suryaocr": { "ready": true },
                "baberuocr": { "ready": true, "device": "cpu", "last_error": null }
            }
        });
        let refresh = apply_health_fields(&snapshot, parse_health_payload(&event));
        assert!(!refresh, "device already checked: no refresh requested");

        let guard = snapshot.lock().unwrap();
        assert!(guard.connected);
        assert_eq!(guard.backend_version.as_deref(), Some("3.4.2"));
        assert_eq!(guard.is_torch_available, Some(true));
        assert_eq!(guard.ocr_manga_ready, Some(true));
        assert_eq!(guard.ocr_easy_ready, Some(false));
        assert_eq!(guard.ocr_paddle_ready, Some(true));
        assert_eq!(guard.ocr_paddle_vl_ready, Some(false));
        assert_eq!(guard.ocr_paddle_vl_model.as_deref(), Some("manga_ja"));
        assert_eq!(guard.ocr_surya_ready, Some(true));
        assert_eq!(guard.ocr_baberu_ready, Some(true));
        assert!(guard.checked_at.is_some());
        assert_eq!(
            guard.details,
            t!("translation.backend_health.state_received_status")
        );
    }

    /// First successful connect (no prior device info) requests a one-time device
    /// refresh.
    #[test]
    fn first_connect_requests_device_refresh() {
        let snapshot = Arc::new(Mutex::new(AiBackendHealthSnapshot::default()));
        let event = json!({ "is_torch_available": true, "ocr": {} });
        let refresh = apply_health_fields(&snapshot, parse_health_payload(&event));
        assert!(refresh, "first connect should request a device refresh");
    }

    /// The pool part of `model_manager` reaches the snapshot; a part the backend failed to
    /// build arrives as an error; a snapshot without `model_manager` (warming up) carries
    /// none, and an offline state clears it.
    #[test]
    fn the_large_pool_part_is_folded_into_the_snapshot() {
        let snapshot = Arc::new(Mutex::new(AiBackendHealthSnapshot::default()));
        let event = json!({
            "ocr": {},
            "model_manager": {
                "max_loaded_models": 3,
                "large_models": [{
                    "model": "flux2_klein", "key": "flux2_klein@cuda:0", "format": "bf16", "device": "cuda:0",
                    "vram_step": "max", "ram_step": "keep", "device_bytes": 1u64, "host_bytes": 2u64,
                    "in_use": true, "degraded": false, "degrade_reason": null, "components": {}
                }],
                "limits": { "vram_cap": 8u64, "ram_cap": null }
            }
        });
        apply_health_fields(&snapshot, parse_health_payload(&event));
        {
            let guard = snapshot.lock().unwrap();
            let pool = guard.large_pool.clone().expect("the pool part is present").expect("and readable");
            assert_eq!(pool.entries.len(), 1);
            assert!(pool.entries[0].in_use);
            assert_eq!(pool.limits.vram_cap, Some(8));
        }
        let failed = json!({
            "ocr": {},
            "model_manager": { "large_models": { "status": "error", "error": "boom" }, "limits": { "vram_cap": null, "ram_cap": null } }
        });
        assert!(matches!(parse_health_payload(&failed).large_pool, Some(Err(_))));
        let warming = json!({ "snapshot_state": "warming_up" });
        assert!(parse_health_payload(&warming).large_pool.is_none());
        super::apply_health_offline(&snapshot, "down".to_string());
        assert!(snapshot.lock().unwrap().large_pool.is_none());
    }

    /// A disconnect->reconnect transition re-requests a device refresh even when
    /// device info was previously enumerated, so a restarted backend's capabilities
    /// (and the derived onnx availability) are re-read rather than left stale.
    #[test]
    fn reconnect_requests_device_refresh() {
        let snapshot = Arc::new(Mutex::new(AiBackendHealthSnapshot::default()));
        // Simulate a prior connect that enumerated devices, then a disconnect
        // (connected=false) that retained the stale device list.
        {
            let mut g = snapshot.lock().unwrap();
            g.connected = false;
            g.device_checked_at = Some(web_time::Instant::now());
            g.available_devices = vec!["cpu".to_string()];
        }
        let event = json!({ "is_torch_available": true, "ocr": {} });
        let refresh = apply_health_fields(&snapshot, parse_health_payload(&event));
        assert!(refresh, "reconnect should re-request a device refresh");
    }

    /// The warming-up snapshot carries no `ocr` block: it must still mark the
    /// backend connected, propagate `is_torch_available`, and leave the per-engine
    /// ready flags `None` (UI shows "still warming up"), exactly like the old parse.
    #[test]
    fn applies_warming_up_health_event_to_snapshot() {
        let snapshot = Arc::new(Mutex::new(AiBackendHealthSnapshot::default()));
        {
            let mut g = snapshot.lock().unwrap();
            g.device_checked_at = Some(web_time::Instant::now());
            g.available_devices = vec!["cpu".to_string()];
        }
        let event = json!({
            "kind": "event",
            "topic": "health",
            "id": 0,
            "ok": true,
            "service": "mf_ai_backend",
            "backend_version": "3.4.2",
            "snapshot_unix_s": 1.0,
            "snapshot_state": "warming_up",
            "is_torch_available": false
        });
        let fields = parse_health_payload(&event);
        assert!(fields.connected);
        assert_eq!(fields.is_torch_available, Some(false));
        assert!(fields.ocr_manga_ready.is_none());
        assert!(fields.ocr_surya_ready.is_none());

        apply_health_fields(&snapshot, parse_health_payload(&event));
        let guard = snapshot.lock().unwrap();
        assert!(guard.connected);
        assert_eq!(guard.is_torch_available, Some(false));
        assert!(guard.ocr_manga_ready.is_none());
        assert!(guard.ocr_easy_ready.is_none());
    }

    /// An offline transition (failed pull / dead subscription) records the detailed
    /// failure message, clears connected, and resets the per-engine flags to `None`.
    #[test]
    fn offline_health_state_resets_snapshot() {
        let snapshot = Arc::new(Mutex::new(AiBackendHealthSnapshot::default()));
        {
            let mut g = snapshot.lock().unwrap();
            g.device_checked_at = Some(web_time::Instant::now());
            g.available_devices = vec!["cpu".to_string()];
        }
        // Seed a connected state first to prove offline clears it.
        apply_health_fields(
            &snapshot,
            parse_health_payload(&json!({
                "is_torch_available": true,
                "backend_version": "3.4.2",
                "ocr": { "mangaocr": { "ready": true } }
            })),
        );
        assert!(snapshot.lock().unwrap().connected);

        // The offline field set mirrors `apply_health_offline`'s payload.
        apply_health_fields(
            &snapshot,
            super::HealthFields {
                connected: false,
                details: super::ai_backend_offline_error().to_string(),
                is_torch_available: None,
                ocr_manga_ready: None,
                ocr_easy_ready: None,
                ocr_paddle_ready: None,
                ocr_paddle_vl_ready: None,
                ocr_paddle_vl_model: None,
                ocr_surya_ready: None,
                ocr_baberu_ready: None,
                backend_version: None,
                large_pool: None,
                torch_build: None,
            },
        );
        let guard = snapshot.lock().unwrap();
        assert!(!guard.connected);
        assert_eq!(guard.details, super::ai_backend_offline_error());
        assert!(guard.is_torch_available.is_none());
        assert!(guard.ocr_manga_ready.is_none());
        assert!(guard.backend_version.is_none());
    }

    #[test]
    fn ignores_empty_backend_version() {
        let payload = json!({"backend_version": "   "});
        assert!(health_backend_version(&payload).is_none());
    }

    #[test]
    fn device_set_header_omits_absent_fields() {
        // Torch device-only set: only `device` is present.
        let header = device_set_header(Some("cuda"), None, None, None);
        assert_eq!(header, json!({ "device": "cuda" }));

        // ONNX provider+device set.
        let header = device_set_header(None, Some("CUDAExecutionProvider"), Some("1"), None);
        assert_eq!(
            header,
            json!({ "onnx_provider": "CUDAExecutionProvider", "onnx_device_id": "1" })
        );

        // Max-loaded-models set.
        let header = device_set_header(None, None, None, Some(5));
        assert_eq!(header, json!({ "max_loaded_models": 5 }));

        // All four together.
        let header = device_set_header(
            Some("cpu"),
            Some("CPUExecutionProvider"),
            Some("0"),
            Some(3),
        );
        assert_eq!(
            header,
            json!({
                "device": "cpu",
                "onnx_provider": "CPUExecutionProvider",
                "onnx_device_id": "0",
                "max_loaded_models": 3,
            })
        );
    }

    #[test]
    fn parses_full_eleven_key_device_state() {
        // The full device.get / device.set response header (11-key dict).
        let payload = json!({
            "selected_device": "cuda:0",
            "available_devices": ["cpu", "cuda:0"],
            "available_device_options": [
                { "id": "cpu", "label": "CPU" },
                { "id": "cuda:0", "label": "NVIDIA GPU 0" }
            ],
            "torch_device_needs_selection": true,
            "max_loaded_models": 4,
            "selected_onnx_provider": "CUDAExecutionProvider",
            "available_onnx_providers": ["CPUExecutionProvider", "CUDAExecutionProvider"],
            "selected_onnx_device_id": "0",
            "available_onnx_device_options": [{ "id": "0", "label": "GPU 0" }],
            "available_onnx_devices_by_provider": {
                "CUDAExecutionProvider": [{ "id": "0", "label": "GPU 0" }]
            },
            "onnx_device_needs_selection": false
        });
        let state = parse_device_state_payload(&payload).expect("device state should parse");
        assert_eq!(state.selected_device, "cuda:0");
        assert_eq!(state.available_devices, vec!["cpu", "cuda:0"]);
        assert!(state.torch_device_needs_selection);
        assert_eq!(state.max_loaded_models, 4);
        assert_eq!(state.selected_onnx_provider, "CUDAExecutionProvider");
        assert_eq!(
            state.available_onnx_providers,
            vec!["CPUExecutionProvider", "CUDAExecutionProvider"]
        );
        assert_eq!(state.selected_onnx_device_id, "0");
        assert!(!state.onnx_device_needs_selection);
        assert!(
            state
                .onnx_devices_by_provider
                .contains_key("CUDAExecutionProvider")
        );
    }

    #[test]
    fn device_state_requires_selected_device() {
        let payload: Value = json!({ "available_devices": ["cpu"] });
        assert!(parse_device_state_payload(&payload).is_err());
    }

    #[test]
    fn maps_device_call_errors() {
        assert_eq!(
            map_device_call_error(CallError::Error("boom".to_string())),
            "boom"
        );
        // Pin the exact catalog key, not just that the payload survived: a mapping
        // regression that picked a different message would still pass a `contains` check.
        assert_eq!(
            map_device_call_error(CallError::Interrupted("MARKER".to_string())),
            tf!("translation.backend_health.request_aborted_error", msg = "MARKER")
        );
        // Transport failures present as the unified offline message.
        assert_eq!(
            map_device_call_error(CallError::Transport("dead".to_string())),
            super::ai_backend_offline_error()
        );
    }

    /// `device.cuda_diagnostics` may return `diagnostics` as an OBJECT (structured
    /// CUDA/ROCm probe). It must render to non-empty human-readable text (the
    /// pretty-printed JSON), NOT collapse to the "empty diagnostics" error.
    #[test]
    fn cuda_diagnostics_object_renders_non_empty_text() {
        let diagnostics = json!({
            "cuda": true,
            "version": "12.0",
            "driver": "555.58"
        });
        let rendered = render_cuda_diagnostics(&diagnostics).expect("object renders to text");
        assert!(
            !rendered.is_empty(),
            "object diagnostics must render non-empty"
        );
        assert!(rendered.contains("cuda"), "rendered JSON keeps the keys");
        assert!(rendered.contains("12.0"), "rendered JSON keeps the values");
    }

    /// A plain-string `diagnostics` (the current backend shape) is rendered
    /// verbatim (trimmed).
    #[test]
    fn cuda_diagnostics_string_renders_verbatim() {
        let diagnostics = json!("  CUDA/ROCm доступна.  ");
        assert_eq!(
            render_cuda_diagnostics(&diagnostics).as_deref(),
            Some("CUDA/ROCm доступна.")
        );
    }

    /// Genuinely empty / missing values still map to `None` so the caller keeps
    /// the "empty diagnostics → error message" behaviour.
    #[test]
    fn cuda_diagnostics_empty_values_render_none() {
        assert!(render_cuda_diagnostics(&Value::Null).is_none());
        assert!(render_cuda_diagnostics(&json!("   ")).is_none());
        assert!(render_cuda_diagnostics(&json!({})).is_none());
        assert!(render_cuda_diagnostics(&json!([])).is_none());
    }

    /// The resident PaddleOCR-VL variant decides readiness of the selected one: a ready
    /// engine of another variant reads as "not loaded", never as a stale "ready".
    #[test]
    fn paddle_vl_readiness_is_per_variant() {
        let mut snapshot = AiBackendHealthSnapshot::default();
        assert_eq!(super::paddle_vl_ready_for(&snapshot, "official_1_6"), None);
        snapshot.ocr_paddle_vl_ready = Some(true);
        snapshot.ocr_paddle_vl_model = Some("manga_ja".to_string());
        assert_eq!(super::paddle_vl_ready_for(&snapshot, "manga_ja"), Some(true));
        assert_eq!(super::paddle_vl_ready_for(&snapshot, "official_1_6"), Some(false));
        snapshot.ocr_paddle_vl_ready = Some(false);
        assert_eq!(super::paddle_vl_ready_for(&snapshot, "manga_ja"), Some(false));
        // A ready flag without a model name cannot vouch for any variant.
        snapshot.ocr_paddle_vl_ready = Some(true);
        snapshot.ocr_paddle_vl_model = None;
        assert_eq!(super::paddle_vl_ready_for(&snapshot, "manga_ja"), Some(false));
    }

    fn gfx(name: &str) -> ms_config::torch_choice::GfxArch {
        ms_config::torch_choice::GfxArch::parse(name).expect("test arch is valid")
    }

    /// A ROCm build report as `runtime/torch_flavour.torch_health_payload` emits it.
    fn rocm_torch_payload() -> Value {
        json!({
            "flavour": "rocm",
            "version": "2.13.0+rocm10.1.0",
            "hip_version": "10.1.0",
            "cuda_version": null,
            "devices": [
                { "index": 0, "name": "AMD Radeon Graphics", "gcn_arch": "gfx1036", "integrated": true },
                { "index": 1, "name": "AMD Radeon RX 9070 XT", "gcn_arch": "gfx1201", "integrated": false }
            ],
            "rocm_device_archs": ["gfx1201"],
            "rocm_split_kernels": true
        })
    }

    /// The `torch` object round-trips into the snapshot field by field, and an offline state
    /// clears it.
    #[test]
    fn torch_build_is_parsed_into_the_snapshot() {
        use ms_config::torch_choice::TorchFlavour;
        let snapshot = Arc::new(Mutex::new(AiBackendHealthSnapshot::default()));
        let event = json!({ "ocr": {}, "torch": rocm_torch_payload() });
        apply_health_fields(&snapshot, parse_health_payload(&event));
        {
            let guard = snapshot.lock().unwrap();
            let build = guard.torch_build.clone().expect("present").expect("readable");
            assert_eq!(
                build,
                super::TorchBuildInfo {
                    flavour: TorchFlavour::Rocm,
                    version: "2.13.0+rocm10.1.0".to_string(),
                    hip_version: Some("10.1.0".to_string()),
                    cuda_version: None,
                    devices: vec![
                        super::TorchGpuInfo { index: 0, name: "AMD Radeon Graphics".to_string(), gcn_arch: Some(gfx("gfx1036")), integrated: true },
                        super::TorchGpuInfo { index: 1, name: "AMD Radeon RX 9070 XT".to_string(), gcn_arch: Some(gfx("gfx1201")), integrated: false },
                    ],
                    rocm_device_archs: vec![gfx("gfx1201")],
                    rocm_split_kernels: true,
                }
            );
            assert_eq!(build.primary_device().map(|device| device.index), Some(1));
        }
        super::apply_health_offline(&snapshot, "down".to_string());
        assert!(snapshot.lock().unwrap().torch_build.is_none());
    }

    /// Absent (warming up, pre-v18) and `null` (no torch) both read as "not reported".
    #[test]
    fn absent_or_null_torch_is_none() {
        assert!(parse_health_payload(&json!({ "snapshot_state": "warming_up" })).torch_build.is_none());
        assert!(parse_health_payload(&json!({ "ocr": {}, "torch": null })).torch_build.is_none());
    }

    /// An unknown flavour token and every unreadable shape are typed errors, never a guess.
    #[test]
    fn unreadable_torch_objects_are_errors() {
        use super::TorchBuildError;
        let parse = |torch: Value| parse_health_payload(&json!({ "torch": torch })).torch_build.expect("reported");
        let mut unknown = rocm_torch_payload();
        unknown["flavour"] = json!("xpu");
        assert_eq!(parse(unknown), Err(TorchBuildError::UnknownFlavour("xpu".to_string())));
        assert_eq!(parse(json!("rocm")), Err(TorchBuildError::Malformed { field: "torch" }));
        let cases: [(&str, Value, &str); 7] = [
            ("rocm_split_kernels", json!("yes"), "torch.rocm_split_kernels"),
            ("version", Value::Null, "torch.version"),
            ("hip_version", json!(10), "torch.hip_version"),
            ("devices", json!({}), "torch.devices"),
            ("rocm_device_archs", json!(["gfx12-generic"]), "torch.rocm_device_archs[]"),
            ("devices", json!([{ "index": -1, "name": "x", "gcn_arch": null, "integrated": false }]), "torch.devices[].index"),
            ("devices", json!([{ "index": 0, "name": "x", "gcn_arch": "sm_90", "integrated": false }]), "torch.devices[].gcn_arch"),
        ];
        for (key, value, field) in cases {
            let mut torch = rocm_torch_payload();
            torch[key] = value;
            assert_eq!(parse(torch), Err(TorchBuildError::Malformed { field }), "{key}");
        }
        let mut missing = rocm_torch_payload();
        missing.as_object_mut().unwrap().remove("cuda_version");
        assert_eq!(parse(missing), Err(TorchBuildError::Malformed { field: "torch.cuda_version" }));
    }

    /// `kernel_mismatch` looks at the primary (first discrete, else first) device of a ROCm
    /// build and stays silent whenever the installed set cannot answer.
    #[test]
    fn kernel_mismatch_table() {
        use ms_config::torch_choice::TorchFlavour;
        let base = match super::parse_torch_build(&rocm_torch_payload()) {
            Ok(build) => build,
            Err(err) => panic!("fixture parses: {err}"),
        };
        // Discrete gfx1201 covered: the uncovered iGPU does not matter.
        assert_eq!(base.kernel_mismatch(), None);

        let mut other_kernels = base.clone();
        other_kernels.rocm_device_archs = vec![gfx("gfx1100"), gfx("gfx1036")];
        assert_eq!(
            other_kernels.kernel_mismatch(),
            Some(super::KernelMismatch { device: gfx("gfx1201"), installed: vec![gfx("gfx1100"), gfx("gfx1036")] })
        );

        // Only integrated devices: the first one is primary.
        let mut igpu_only = base.clone();
        igpu_only.devices.retain(|device| device.integrated);
        igpu_only.rocm_device_archs = vec![gfx("gfx1201")];
        assert_eq!(igpu_only.kernel_mismatch().map(|m| m.device), Some(gfx("gfx1036")));

        // Bare whl-next torch: no per-arch wheel at all means no kernels for the GPU.
        let mut bare = base.clone();
        bare.rocm_device_archs.clear();
        assert_eq!(bare.kernel_mismatch(), Some(super::KernelMismatch { device: gfx("gfx1201"), installed: Vec::new() }));

        // Bundled kernels (pytorch.org ROCm build), no devices, no arch, not ROCm: silent.
        let mut bundled = other_kernels.clone();
        bundled.rocm_split_kernels = false;
        assert_eq!(bundled.kernel_mismatch(), None);
        bundled.rocm_device_archs.clear();
        assert_eq!(bundled.kernel_mismatch(), None);
        let mut no_devices = other_kernels.clone();
        no_devices.devices.clear();
        assert_eq!(no_devices.kernel_mismatch(), None);
        let mut no_arch = other_kernels.clone();
        no_arch.devices.iter_mut().for_each(|device| device.gcn_arch = None);
        assert_eq!(no_arch.kernel_mismatch(), None);
        let mut cuda = other_kernels;
        cuda.flavour = TorchFlavour::Cuda;
        assert_eq!(cuda.kernel_mismatch(), None);
    }
}
