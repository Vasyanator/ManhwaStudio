/*
FILE OVERVIEW: src/tabs/translation/backend_health.rs
Shared health helpers for Python AI backend used by Settings and Translation tabs.

Main constants:
- `AI_BACKEND_HOST` / `AI_BACKEND_PORT`: default backend TCP endpoint.
- `AI_BACKEND_OFFLINE_ERROR`: unified user-facing message when health check fails.

Main types:
- `AiBackendHealthSnapshot`: last probe result for UI status
  (`connected`, `details`, `checked_at`, backend version).
- `AiBackendProbeCommand`: control messages for probe thread
  (`CheckNow`, `RefreshDeviceInfo`, `SetDevice`, `SetOnnxDevice`, `RefreshCudaDiagnostics`, `Stop`).

Main functions:
- `ai_backend_port` / `set_ai_backend_port` / `ai_backend_addr_text`: runtime endpoint helpers
  used when the process manager starts the backend on a fallback port.
- `ensure_ai_backend_healthy`: lightweight gate before backend requests; maps any failure to
  a stable user-friendly error (`ИИ бэкенд отключен`).
- `check_ai_backend_health`: direct `/health` probe with short timeout, returns detailed error.
- `probe_ai_backend_once`: writes one probe result into shared snapshot.
- `spawn_ai_backend_probe`: starts periodic background probing thread for UI status updates.
*/

use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const AI_BACKEND_HOST: &str = "127.0.0.1";
pub const AI_BACKEND_PORT: u16 = 8765;
pub const AI_BACKEND_OFFLINE_ERROR: &str = "ИИ бэкенд отключен";

static AI_BACKEND_RUNTIME_PORT: AtomicU16 = AtomicU16::new(AI_BACKEND_PORT);

const AI_BACKEND_HEALTH_ENDPOINT: &str = "/health";
const AI_BACKEND_DEVICE_ENDPOINT: &str = "/device";
const AI_BACKEND_DEVICE_SET_ENDPOINT: &str = "/device/set";
const AI_BACKEND_CUDA_DIAGNOSTICS_ENDPOINT: &str = "/device/cuda_diagnostics";
const AI_BACKEND_POLL_INTERVAL: Duration = Duration::from_secs(2);
const AI_BACKEND_TIMEOUT: Duration = Duration::from_millis(700);
const AI_BACKEND_DEVICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const AI_BACKEND_DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(20);

#[must_use]
pub fn ai_backend_port() -> u16 {
    AI_BACKEND_RUNTIME_PORT.load(Ordering::Relaxed)
}

pub fn set_ai_backend_port(port: u16) {
    AI_BACKEND_RUNTIME_PORT.store(port, Ordering::Relaxed);
}

#[must_use]
pub fn ai_backend_addr_text() -> String {
    format!("{}:{}", AI_BACKEND_HOST, ai_backend_port())
}

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

pub fn ensure_ai_backend_healthy() -> Result<(), String> {
    check_ai_backend_health().map_err(|_| AI_BACKEND_OFFLINE_ERROR.to_string())
}

pub fn check_ai_backend_health() -> Result<(), String> {
    let addr_text = ai_backend_addr_text();
    let socket_addr = addr_text
        .to_socket_addrs()
        .map_err(|err| format!("Ошибка DNS/резолва адреса {addr_text}: {err}"))?
        .next()
        .ok_or_else(|| format!("Не удалось получить сокет-адрес для {addr_text}"))?;

    let mut stream = TcpStream::connect_timeout(&socket_addr, AI_BACKEND_TIMEOUT)
        .map_err(|err| format!("Ошибка подключения: {err}"))?;
    stream
        .set_read_timeout(Some(AI_BACKEND_TIMEOUT))
        .map_err(|err| format!("Не удалось выставить read timeout: {err}"))?;
    stream
        .set_write_timeout(Some(AI_BACKEND_TIMEOUT))
        .map_err(|err| format!("Не удалось выставить write timeout: {err}"))?;

    let request = format!(
        "GET {AI_BACKEND_HEALTH_ENDPOINT} HTTP/1.1\r\nHost: {addr_text}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("Не удалось отправить HTTP-запрос: {err}"))?;

    let mut head_buf = [0_u8; 256];
    let read_count = stream
        .read(&mut head_buf)
        .map_err(|err| format!("Не удалось прочитать ответ сервиса: {err}"))?;
    if read_count == 0 {
        return Err("Сервис вернул пустой ответ.".to_string());
    }

    let response_head = String::from_utf8_lossy(&head_buf[..read_count]);
    let status_line = response_head.lines().next().unwrap_or("UNKNOWN");
    if status_line.contains(" 200 ") {
        Ok(())
    } else {
        Err(format!("Некорректный ответ health endpoint: {status_line}"))
    }
}

pub fn probe_ai_backend_once(snapshot: &Arc<Mutex<AiBackendHealthSnapshot>>) {
    let (
        connected,
        details,
        is_torch_available,
        ocr_manga_ready,
        ocr_easy_ready,
        ocr_paddle_ready,
        ocr_surya_ready,
        backend_version,
    ) = match request_json("GET", AI_BACKEND_HEALTH_ENDPOINT, None, AI_BACKEND_TIMEOUT) {
        Ok(payload) => (
            true,
            "Health endpoint отвечает кодом 200.".to_string(),
            payload.get("is_torch_available").and_then(Value::as_bool),
            health_ocr_ready_flag(&payload, "mangaocr"),
            health_ocr_ready_flag(&payload, "easyocr"),
            health_ocr_ready_flag(&payload, "paddleocr"),
            health_ocr_ready_flag(&payload, "suryaocr"),
            health_backend_version(&payload),
        ),
        Err(err) => (false, err, None, None, None, None, None, None),
    };
    let checked_at = Instant::now();
    crate::ai_backend_capabilities::set_torch_available(is_torch_available);

    let mut should_refresh_devices = false;
    match snapshot.lock() {
        Ok(mut guard) => {
            guard.connected = connected;
            guard.details = details;
            guard.checked_at = Some(checked_at);
            guard.backend_version = backend_version.clone();
            guard.is_torch_available = is_torch_available;
            guard.ocr_manga_ready = ocr_manga_ready;
            guard.ocr_easy_ready = ocr_easy_ready;
            guard.ocr_paddle_ready = ocr_paddle_ready;
            guard.ocr_surya_ready = ocr_surya_ready;
            if connected
                && (guard.device_checked_at.is_none() || guard.available_devices.is_empty())
            {
                should_refresh_devices = true;
            }
        }
        Err(mut poisoned) => {
            let guard = poisoned.get_mut();
            guard.connected = connected;
            guard.details = details;
            guard.checked_at = Some(checked_at);
            guard.backend_version = backend_version;
            guard.is_torch_available = is_torch_available;
            guard.ocr_manga_ready = ocr_manga_ready;
            guard.ocr_easy_ready = ocr_easy_ready;
            guard.ocr_paddle_ready = ocr_paddle_ready;
            guard.ocr_surya_ready = ocr_surya_ready;
            if connected
                && (guard.device_checked_at.is_none() || guard.available_devices.is_empty())
            {
                should_refresh_devices = true;
            }
        }
    }

    if should_refresh_devices {
        refresh_ai_backend_device_info(snapshot);
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
    let payload = request_json(
        "GET",
        AI_BACKEND_DEVICE_ENDPOINT,
        None,
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
    let body = serde_json::json!({ "device": device }).to_string();
    let started_at = Instant::now();
    let payload = request_json(
        "POST",
        AI_BACKEND_DEVICE_SET_ENDPOINT,
        Some(body.as_str()),
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
    let body = serde_json::json!({
        "onnx_provider": provider,
        "onnx_device_id": device_id,
    })
    .to_string();
    let started_at = Instant::now();
    let payload = request_json(
        "POST",
        AI_BACKEND_DEVICE_SET_ENDPOINT,
        Some(body.as_str()),
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
    let body = serde_json::json!({
        "max_loaded_models": max_loaded_models,
    })
    .to_string();
    let started_at = Instant::now();
    let payload = request_json(
        "POST",
        AI_BACKEND_DEVICE_SET_ENDPOINT,
        Some(body.as_str()),
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
    let payload = request_json(
        "GET",
        AI_BACKEND_CUDA_DIAGNOSTICS_ENDPOINT,
        None,
        AI_BACKEND_DIAGNOSTICS_TIMEOUT,
    )?;
    payload
        .get("diagnostics")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Эндпоинт диагностики вернул пустое поле diagnostics.".to_string())
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

fn request_json(
    method: &str,
    path: &str,
    payload: Option<&str>,
    timeout: Duration,
) -> Result<Value, String> {
    let started_at = Instant::now();
    let should_log_device_request = path.starts_with("/device");
    if should_log_device_request {
        crate::runtime_log::log_info(format!(
            "[ai_backend_probe] request_json start method={method} path={path} \
             has_body={} timeout_ms={}",
            payload.is_some(),
            timeout.as_millis()
        ));
    }
    let addr_text = ai_backend_addr_text();
    let socket_addr = addr_text
        .to_socket_addrs()
        .map_err(|err| format!("Ошибка DNS/резолва адреса {addr_text}: {err}"))?
        .next()
        .ok_or_else(|| format!("Не удалось получить сокет-адрес для {addr_text}"))?;

    let mut stream = TcpStream::connect_timeout(&socket_addr, timeout)
        .map_err(|err| format!("Ошибка подключения: {err}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| format!("Не удалось выставить read timeout: {err}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| format!("Не удалось выставить write timeout: {err}"))?;

    let body = payload.unwrap_or("");
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: {addr_text}\r\nConnection: close\r\n");
    if payload.is_some() {
        request.push_str("Content-Type: application/json; charset=utf-8\r\n");
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");

    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("Не удалось отправить HTTP-заголовки: {err}"))?;
    if payload.is_some() {
        stream
            .write_all(body.as_bytes())
            .map_err(|err| format!("Не удалось отправить HTTP-body: {err}"))?;
    }

    let (status_code, body) = read_http_response(&mut stream)?;
    if should_log_device_request {
        crate::runtime_log::log_info(format!(
            "[ai_backend_probe] request_json http_response method={method} path={path} \
             status={status_code} body_bytes={} elapsed_ms={}",
            body.len(),
            started_at.elapsed().as_millis()
        ));
    }
    let body_text = String::from_utf8_lossy(&body).to_string();
    let payload: Value = serde_json::from_str(&body_text)
        .map_err(|err| format!("Сервис вернул не-JSON ({status_code}): {err}"))?;

    if status_code >= 400 {
        let message = payload
            .get("error")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Неизвестная ошибка AI backend.");
        return Err(format!("AI backend HTTP {status_code}: {message}"));
    }

    if should_log_device_request {
        crate::runtime_log::log_info(format!(
            "[ai_backend_probe] request_json ok method={method} path={path} elapsed_ms={}",
            started_at.elapsed().as_millis()
        ));
    }
    Ok(payload)
}

fn read_http_response(stream: &mut TcpStream) -> Result<(u16, Vec<u8>), String> {
    let mut raw = Vec::with_capacity(8 * 1024);
    let mut chunk = [0_u8; 8 * 1024];
    let mut header_end: Option<usize> = None;
    let mut expected_total_len: Option<usize> = None;

    loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|err| format!("Не удалось прочитать HTTP-ответ: {err}"))?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);

        if header_end.is_none() {
            header_end = raw.windows(4).position(|window| window == b"\r\n\r\n");
            if let Some(end) = header_end {
                let head = String::from_utf8_lossy(&raw[..end]);
                let content_len = parse_http_content_length(&head).ok_or_else(|| {
                    "HTTP-ответ AI backend не содержит Content-Length.".to_string()
                })?;
                expected_total_len = Some(end + 4 + content_len);
            }
        }

        if let Some(total_len) = expected_total_len
            && raw.len() >= total_len
        {
            break;
        }
    }

    if raw.is_empty() {
        return Err("AI backend вернул пустой HTTP-ответ.".to_string());
    }

    let Some(end) = header_end else {
        return Err("Некорректный HTTP-ответ AI backend (нет заголовков).".to_string());
    };
    let head = String::from_utf8_lossy(&raw[..end]);
    let status_line = head.lines().next().unwrap_or("HTTP/1.1 500");
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|part| part.parse::<u16>().ok())
        .ok_or_else(|| format!("Не удалось распарсить HTTP-статус AI backend: {status_line}"))?;

    let expected_len = parse_http_content_length(&head)
        .ok_or_else(|| "HTTP-ответ AI backend не содержит Content-Length.".to_string())?;
    let body_start = end + 4;
    if raw.len() < body_start + expected_len {
        return Err("AI backend закрыл соединение до полного тела HTTP-ответа.".to_string());
    }
    let body = raw[body_start..body_start + expected_len].to_vec();
    Ok((status_code, body))
}

fn parse_http_content_length(head: &str) -> Option<usize> {
    for line in head.lines() {
        let mut parts = line.splitn(2, ':');
        let key = parts.next()?.trim();
        if !key.eq_ignore_ascii_case("Content-Length") {
            continue;
        }
        let value = parts.next()?.trim();
        if let Ok(length) = value.parse::<usize>() {
            return Some(length);
        }
    }
    None
}

pub fn spawn_ai_backend_probe(
    snapshot: Arc<Mutex<AiBackendHealthSnapshot>>,
) -> (Sender<AiBackendProbeCommand>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<AiBackendProbeCommand>();
    let handle = thread::spawn(move || {
        probe_ai_backend_once(&snapshot);
        loop {
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
                Err(RecvTimeoutError::Timeout) => probe_ai_backend_once(&snapshot),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    (tx, handle)
}

#[cfg(test)]
mod tests {
    use super::health_backend_version;
    use serde_json::json;

    #[test]
    fn reads_backend_version_from_health_payload() {
        let payload = json!({"backend_version": " 3.4.0 "});
        assert_eq!(health_backend_version(&payload).as_deref(), Some("3.4.0"));
    }

    #[test]
    fn ignores_empty_backend_version() {
        let payload = json!({"backend_version": "   "});
        assert!(health_backend_version(&payload).is_none());
    }
}
