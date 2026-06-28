/*
FILE OVERVIEW: src/tabs/translation/backend_health.rs
Shared health helpers for Python AI backend used by Settings and Translation tabs.

Transport:
All backend requests go through the v2 framed IPC client (`crate::backend_ipc`)
over the AF_UNIX socket. This file no longer owns any TCP endpoint or HTTP state.

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
- `AI_BACKEND_OFFLINE_ERROR`: unified user-facing message when health check fails.

Main types:
- `AiBackendHealthSnapshot`: last health result for UI status
  (`connected`, `details`, `checked_at`, backend version).
- `AiBackendProbeCommand`: control messages for probe thread
  (`CheckNow`, `RefreshDeviceInfo`, `SetDevice`, `SetOnnxDevice`, `RefreshCudaDiagnostics`, `Stop`).

Main functions:
- `check_ai_backend_health`: one-shot v2 `health` pull readiness gate with a short
  timeout, returns a detailed error (used by reline + the probe priming step).
- `probe_ai_backend_once`: pulls one `health` snapshot and writes it into the shared snapshot.
- `apply_health_payload`: folds a health `Value` (pulled response OR pushed event header)
  into the shared snapshot — identical field mapping for both transports.
- `spawn_ai_backend_probe`: starts the background `TOPIC_HEALTH` subscription + device-control thread.
*/

use crate::backend_ipc::{self, CallError};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const AI_BACKEND_OFFLINE_ERROR: &str = "ИИ бэкенд отключен";

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
    pub ocr_surya_ready: Option<bool>,
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
}

impl Default for AiBackendHealthSnapshot {
    fn default() -> Self {
        Self {
            connected: false,
            details: "Ожидание первой проверки...".to_string(),
            checked_at: None,
            backend_version: None,
            is_torch_available: None,
            ocr_manga_ready: None,
            ocr_easy_ready: None,
            ocr_paddle_ready: None,
            ocr_paddle_vl_ready: None,
            ocr_surya_ready: None,
            selected_device: None,
            available_devices: Vec::new(),
            device_options: Vec::new(),
            torch_device_needs_selection: false,
            device_details: "Список устройств ещё не запрошен.".to_string(),
            selected_onnx_provider: None,
            available_onnx_providers: Vec::new(),
            selected_onnx_device_id: None,
            onnx_device_options: Vec::new(),
            onnx_devices_by_provider: HashMap::new(),
            onnx_device_needs_selection: false,
            max_loaded_models: 3,
            onnx_details: "Список ONNX-провайдеров ещё не запрошен.".to_string(),
            device_checked_at: None,
            cuda_diagnostics: "Диагностика CUDA/ROCm ещё не запускалась.".to_string(),
            cuda_checked_at: None,
        }
    }
}

impl AiBackendHealthSnapshot {
    pub fn disabled() -> Self {
        Self {
            connected: false,
            details: "Отключено флагом --no-ai.".to_string(),
            checked_at: None,
            backend_version: None,
            is_torch_available: Some(false),
            ocr_manga_ready: None,
            ocr_easy_ready: None,
            ocr_paddle_ready: None,
            ocr_paddle_vl_ready: None,
            ocr_surya_ready: None,
            selected_device: None,
            available_devices: Vec::new(),
            device_options: Vec::new(),
            torch_device_needs_selection: false,
            device_details: "Управление устройством отключено (--no-ai).".to_string(),
            selected_onnx_provider: None,
            available_onnx_providers: Vec::new(),
            selected_onnx_device_id: None,
            onnx_device_options: Vec::new(),
            onnx_devices_by_provider: HashMap::new(),
            onnx_device_needs_selection: false,
            max_loaded_models: 3,
            onnx_details: "Управление ONNX отключено (--no-ai).".to_string(),
            device_checked_at: None,
            cuda_diagnostics: "Диагностика CUDA/ROCm отключена (--no-ai).".to_string(),
            cuda_checked_at: None,
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
            CallError::Interrupted(msg) => format!("Запрос health прерван: {msg}"),
            // A transport failure means the backend is offline / the socket is
            // dead; surface the unified offline message (matching device calls)
            // rather than the raw OS/framing string.
            CallError::Transport(_) => AI_BACKEND_OFFLINE_ERROR.to_string(),
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
    ocr_surya_ready: Option<bool>,
    backend_version: Option<String>,
}

/// Maps a successful health payload (`ok`, `is_torch_available`, `ocr{...}`,
/// `backend_version`, optional `snapshot_state:"warming_up"`) to [`HealthFields`].
///
/// The warming-up snapshot has no `ocr` block, so the per-engine ready flags fall
/// back to `None` (UI shows "still warming up"), exactly as the old polled parse.
fn parse_health_payload(payload: &Value) -> HealthFields {
    HealthFields {
        connected: true,
        details: "Состояние получено по IPC (v2 health).".to_string(),
        is_torch_available: payload.get("is_torch_available").and_then(Value::as_bool),
        ocr_manga_ready: health_ocr_ready_flag(payload, "mangaocr"),
        ocr_easy_ready: health_ocr_ready_flag(payload, "easyocr"),
        ocr_paddle_ready: health_ocr_ready_flag(payload, "paddleocr"),
        ocr_paddle_vl_ready: health_ocr_ready_flag(payload, "paddleocrvl"),
        ocr_surya_ready: health_ocr_ready_flag(payload, "suryaocr"),
        backend_version: health_backend_version(payload),
    }
}

/// Folds parsed [`HealthFields`] into the shared snapshot and returns whether a
/// device-info refresh is warranted (first successful connect). Poison-safe: a
/// poisoned mutex is recovered via `get_mut`, mirroring the rest of this file.
fn apply_health_fields(
    snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>,
    fields: HealthFields,
) -> bool {
    let checked_at = Instant::now();
    crate::ai_backend_capabilities::set_torch_available(fields.is_torch_available);

    let mut guard = match snapshot.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.connected = fields.connected;
    guard.details = fields.details;
    guard.checked_at = Some(checked_at);
    guard.backend_version = fields.backend_version;
    guard.is_torch_available = fields.is_torch_available;
    guard.ocr_manga_ready = fields.ocr_manga_ready;
    guard.ocr_easy_ready = fields.ocr_easy_ready;
    guard.ocr_paddle_ready = fields.ocr_paddle_ready;
    guard.ocr_paddle_vl_ready = fields.ocr_paddle_vl_ready;
    guard.ocr_surya_ready = fields.ocr_surya_ready;
    fields.connected && (guard.device_checked_at.is_none() || guard.available_devices.is_empty())
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
            ocr_surya_ready: None,
            backend_version: None,
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
    crate::runtime_log::log_info("[ai_backend_probe] refresh_device_info start");
    let checked_at = Instant::now();
    match fetch_ai_backend_device_info() {
        Ok(device_state) => match snapshot.lock() {
            Ok(mut guard) => {
                crate::runtime_log::log_info(format!(
                    "[ai_backend_probe] refresh_device_info ok {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(&mut guard, &device_state);
                guard.device_details = format!(
                    "Текущее устройство PyTorch: {}",
                    device_state.selected_device
                );
                guard.onnx_details = format!(
                    "Текущий ONNX: {} / {}",
                    device_state.selected_onnx_provider, device_state.selected_onnx_device_id
                );
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                crate::runtime_log::log_info(format!(
                    "[ai_backend_probe] refresh_device_info ok poisoned_lock {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(guard, &device_state);
                guard.device_details = format!(
                    "Текущее устройство PyTorch: {}",
                    device_state.selected_device
                );
                guard.onnx_details = format!(
                    "Текущий ONNX: {} / {}",
                    device_state.selected_onnx_provider, device_state.selected_onnx_device_id
                );
                guard.device_checked_at = Some(checked_at);
            }
        },
        Err(err) => match snapshot.lock() {
            Ok(mut guard) => {
                crate::runtime_log::log_warn(format!(
                    "[ai_backend_probe] refresh_device_info failed: {err}"
                ));
                guard.device_details = err;
                guard.onnx_details = guard.device_details.clone();
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                crate::runtime_log::log_warn(format!(
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
    crate::runtime_log::log_info(format!(
        "[ai_backend_probe] set_torch_device start device={device}"
    ));
    let checked_at = Instant::now();
    match apply_ai_backend_device(&device) {
        Ok(device_state) => match snapshot.lock() {
            Ok(mut guard) => {
                crate::runtime_log::log_info(format!(
                    "[ai_backend_probe] set_torch_device ok {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(&mut guard, &device_state);
                guard.device_details = format!(
                    "Устройство PyTorch применено: {}",
                    device_state.selected_device
                );
                guard.onnx_details = format!(
                    "Текущий ONNX: {} / {}",
                    device_state.selected_onnx_provider, device_state.selected_onnx_device_id
                );
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                crate::runtime_log::log_info(format!(
                    "[ai_backend_probe] set_torch_device ok poisoned_lock {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(guard, &device_state);
                guard.device_details = format!(
                    "Устройство PyTorch применено: {}",
                    device_state.selected_device
                );
                guard.onnx_details = format!(
                    "Текущий ONNX: {} / {}",
                    device_state.selected_onnx_provider, device_state.selected_onnx_device_id
                );
                guard.device_checked_at = Some(checked_at);
            }
        },
        Err(err) => match snapshot.lock() {
            Ok(mut guard) => {
                crate::runtime_log::log_warn(format!(
                    "[ai_backend_probe] set_torch_device failed: {err}"
                ));
                guard.device_details = err;
                guard.onnx_details = guard.device_details.clone();
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                crate::runtime_log::log_warn(format!(
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
    crate::runtime_log::log_info(format!(
        "[ai_backend_probe] set_onnx_device start provider={provider} device_id={device_id}"
    ));
    let checked_at = Instant::now();
    match apply_ai_backend_onnx_device(&provider, &device_id) {
        Ok(device_state) => match snapshot.lock() {
            Ok(mut guard) => {
                crate::runtime_log::log_info(format!(
                    "[ai_backend_probe] set_onnx_device ok {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(&mut guard, &device_state);
                guard.device_details = format!(
                    "Текущее устройство PyTorch: {}",
                    device_state.selected_device
                );
                guard.onnx_details = format!(
                    "Устройство ONNX применено: {} / {}",
                    device_state.selected_onnx_provider, device_state.selected_onnx_device_id
                );
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                crate::runtime_log::log_info(format!(
                    "[ai_backend_probe] set_onnx_device ok poisoned_lock {}",
                    summarize_device_state(&device_state)
                ));
                apply_device_state_snapshot(guard, &device_state);
                guard.device_details = format!(
                    "Текущее устройство PyTorch: {}",
                    device_state.selected_device
                );
                guard.onnx_details = format!(
                    "Устройство ONNX применено: {} / {}",
                    device_state.selected_onnx_provider, device_state.selected_onnx_device_id
                );
                guard.device_checked_at = Some(checked_at);
            }
        },
        Err(err) => match snapshot.lock() {
            Ok(mut guard) => {
                crate::runtime_log::log_warn(format!(
                    "[ai_backend_probe] set_onnx_device failed: {err}"
                ));
                guard.onnx_details = err;
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                crate::runtime_log::log_warn(format!(
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
                guard.device_details = format!(
                    "Лимит загруженных моделей применён: {}",
                    device_state.max_loaded_models
                );
                guard.onnx_details = format!(
                    "Текущий ONNX: {} / {}",
                    device_state.selected_onnx_provider, device_state.selected_onnx_device_id
                );
                guard.device_checked_at = Some(checked_at);
            }
            Err(mut poisoned) => {
                let guard = poisoned.get_mut();
                apply_device_state_snapshot(guard, &device_state);
                guard.device_details = format!(
                    "Лимит загруженных моделей применён: {}",
                    device_state.max_loaded_models
                );
                guard.onnx_details = format!(
                    "Текущий ONNX: {} / {}",
                    device_state.selected_onnx_provider, device_state.selected_onnx_device_id
                );
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
    crate::runtime_log::log_info(format!(
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
    crate::runtime_log::log_info(format!(
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
    crate::runtime_log::log_info(format!(
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
    crate::runtime_log::log_info(format!(
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
        .ok_or_else(|| "Метод диагностики не вернул поле diagnostics.".to_string())?;
    render_cuda_diagnostics(diagnostics)
        .ok_or_else(|| "Метод диагностики вернул пустое поле diagnostics.".to_string())
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
    crate::runtime_log::log_info(format!(
        "[ai_backend_probe] parse_device_state start {}",
        summarize_device_payload(payload)
    ));
    let selected = payload
        .get("selected_device")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Эндпоинт устройств не вернул selected_device.".to_string())?;

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
    crate::runtime_log::log_info(format!(
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
/// [`AI_BACKEND_OFFLINE_ERROR`].
///
/// `CallError` mapping preserves the previous device UX:
/// - `Error`       → the backend error message (same as the old HTTP 4xx/5xx).
/// - `Interrupted` → transient abort surfaced to the device status line.
/// - `Transport`   → connect/framing failure (backend offline path).
fn device_call(method: &str, header_fields: Value, timeout: Duration) -> Result<Value, String> {
    let client = backend_ipc::shared_client().map_err(|_| AI_BACKEND_OFFLINE_ERROR.to_string())?;
    let (header, _blob) = client
        .call(method, header_fields, &[], timeout)
        .map_err(map_device_call_error)?;
    Ok(header)
}

/// Shared `CallError` → user-facing `String` mapping for device calls.
fn map_device_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => format!("Запрос к AI backend прерван: {msg}"),
        CallError::Transport(_) => AI_BACKEND_OFFLINE_ERROR.to_string(),
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
            apply_health_offline(snapshot, AI_BACKEND_OFFLINE_ERROR.to_string());
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
    use crate::backend_ipc::CallError;
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
        // Pre-seed device_checked_at so apply_health_fields reports no refresh need;
        // we assert the returned flag separately below.
        {
            let mut g = snapshot.lock().unwrap();
            g.device_checked_at = Some(std::time::Instant::now());
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
                "paddleocrvl": { "ready": false },
                "suryaocr": { "ready": true }
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
        assert_eq!(guard.ocr_surya_ready, Some(true));
        assert!(guard.checked_at.is_some());
        assert_eq!(guard.details, "Состояние получено по IPC (v2 health).");
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

    /// The warming-up snapshot carries no `ocr` block: it must still mark the
    /// backend connected, propagate `is_torch_available`, and leave the per-engine
    /// ready flags `None` (UI shows "still warming up"), exactly like the old parse.
    #[test]
    fn applies_warming_up_health_event_to_snapshot() {
        let snapshot = Arc::new(Mutex::new(AiBackendHealthSnapshot::default()));
        {
            let mut g = snapshot.lock().unwrap();
            g.device_checked_at = Some(std::time::Instant::now());
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
            g.device_checked_at = Some(std::time::Instant::now());
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
                details: super::AI_BACKEND_OFFLINE_ERROR.to_string(),
                is_torch_available: None,
                ocr_manga_ready: None,
                ocr_easy_ready: None,
                ocr_paddle_ready: None,
                ocr_paddle_vl_ready: None,
                ocr_surya_ready: None,
                backend_version: None,
            },
        );
        let guard = snapshot.lock().unwrap();
        assert!(!guard.connected);
        assert_eq!(guard.details, super::AI_BACKEND_OFFLINE_ERROR);
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
        assert!(map_device_call_error(CallError::Interrupted("x".to_string())).contains("прерван"));
        // Transport failures present as the unified offline message.
        assert_eq!(
            map_device_call_error(CallError::Transport("dead".to_string())),
            super::AI_BACKEND_OFFLINE_ERROR
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
}
