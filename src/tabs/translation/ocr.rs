// ============================================================================
// OCR CONTROLLER (Translation tab)
// ----------------------------------------------------------------------------
// Что в файле:
// - `TranslationOcrController`: state-машина OCR
//   (NotLoaded/DownloadingModel/Loading/Ready/Error),
//   очередь команд в worker и публикация событий в UI.
// - Worker-команды: загрузка движка и OCR-запросы с возвратом результата/ошибки.
// - Runtime-оптимизации:
//   1) persistent HTTP-клиент к Python backend (keep-alive, без reconnect на
//      каждый OCR-запрос),
//   2) LRU-кэш декодированных страниц для повторных crop при OCR по блокам.
//   3) advanced-recognition может передать уже скомпозитенный PNG crop с
//      пользовательским оверлеем, который worker использует вместо crop страницы.
// - Вспомогательные функции: crop по UV, PNG/base64, HTTP-парсер и JSON.
// ============================================================================
use crate::tabs::translation::backend_health::{ai_backend_addr_text, ensure_ai_backend_healthy};
use crate::{ai_models, config};
use image::{DynamicImage, GenericImageView, ImageFormat};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{Cursor, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const OCR_BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const OCR_BACKEND_READ_TIMEOUT: Duration = Duration::from_secs(300);
const OCR_BACKEND_WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const OCR_EVENT_POLL_BUDGET: usize = 16;
const OCR_HTTP_RETRY_LIMIT: usize = 1;
const OCR_PAGE_CACHE_MAX_ITEMS: usize = 8;
const OCR_PAGE_CACHE_MAX_BYTES: usize = 256 * 1024 * 1024;
const DUMMY_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAADElEQVR4nGNgGB4AAADIAAGtQHYiAAAAAElFTkSuQmCC";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OcrLoadState {
    NotLoaded,
    DownloadingModel,
    Loading,
    Ready,
    Error,
}

impl OcrLoadState {
    pub fn is_busy(self) -> bool {
        matches!(self, Self::DownloadingModel | Self::Loading)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OcrEngine {
    MangaOcr,
    EasyOcr,
    PaddleOcr,
    Surya,
}

impl OcrEngine {
    fn endpoint_path(self) -> Option<&'static str> {
        match self {
            OcrEngine::MangaOcr => Some("/ocr/manga"),
            OcrEngine::EasyOcr => Some("/ocr/easy"),
            OcrEngine::PaddleOcr => Some("/ocr/paddle"),
            OcrEngine::Surya => Some("/ocr/surya"),
        }
    }

    pub fn requires_backend(self) -> bool {
        self.endpoint_path().is_some()
    }
}

#[derive(Debug, Clone)]
pub struct OcrRuntimeOptions {
    pub manga_model: String,
    pub paddle_lang: String,
    pub easy_langs: String,
    pub surya_task_name: String,
    pub surya_recognize_math: bool,
    pub surya_sort_lines: bool,
    pub surya_drop_repeated_text: bool,
    pub surya_max_sliding_window: u32,
    pub surya_max_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct OcrRecognizeRequest {
    pub request_id: u64,
    pub engine: OcrEngine,
    pub options: OcrRuntimeOptions,
    pub page_path: PathBuf,
    pub uv_rect: [f32; 4],
    pub image_override_png: Option<Vec<u8>>,
    pub join_newlines: bool,
    pub reflect_strings: bool,
}

#[derive(Debug, Clone)]
pub struct OcrRecognizeResult {
    pub lines: Vec<String>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub enum OcrControllerEvent {
    StateChanged(OcrLoadState),
    Recognized {
        request_id: u64,
        result: OcrRecognizeResult,
    },
    RecognizeFailed {
        request_id: u64,
        error: String,
    },
}

#[derive(Debug)]
pub struct TranslationOcrController {
    state: OcrLoadState,
    last_error: Option<String>,
    last_result: Option<OcrRecognizeResult>,
    pending_request: Option<OcrRecognizeRequest>,
    cmd_tx: Sender<WorkerCommand>,
    evt_rx: Receiver<WorkerEvent>,
    worker_thread: Option<JoinHandle<()>>,
}

impl Default for TranslationOcrController {
    fn default() -> Self {
        Self::new()
    }
}

impl TranslationOcrController {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();
        let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
        let worker_thread = thread::spawn(move || worker_loop(cmd_rx, evt_tx));
        Self {
            state: OcrLoadState::NotLoaded,
            last_error: None,
            last_result: None,
            pending_request: None,
            cmd_tx,
            evt_rx,
            worker_thread: Some(worker_thread),
        }
    }

    pub fn state(&self) -> OcrLoadState {
        self.state
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn last_result(&self) -> Option<&OcrRecognizeResult> {
        self.last_result.as_ref()
    }

    pub fn request_load(&mut self, engine: OcrEngine, options: OcrRuntimeOptions) {
        if self.state.is_busy() {
            return;
        }
        self.set_state(OcrLoadState::Loading);
        self.last_error = None;
        if self
            .cmd_tx
            .send(WorkerCommand::Load { engine, options })
            .is_err()
        {
            self.last_error = Some("OCR worker недоступен.".to_string());
            self.set_state(OcrLoadState::Error);
        }
    }

    pub fn request_recognize(&mut self, request: OcrRecognizeRequest) {
        match self.state {
            OcrLoadState::Ready => {
                if self.cmd_tx.send(WorkerCommand::Recognize(request)).is_err() {
                    self.last_error = Some("OCR worker недоступен.".to_string());
                    self.set_state(OcrLoadState::Error);
                }
            }
            OcrLoadState::DownloadingModel | OcrLoadState::Loading => {
                self.pending_request = Some(request);
            }
            OcrLoadState::NotLoaded | OcrLoadState::Error => {
                let engine = request.engine;
                let options = request.options.clone();
                self.pending_request = Some(request);
                self.request_load(engine, options);
            }
        }
    }

    pub fn poll_events(&mut self) -> Vec<OcrControllerEvent> {
        let mut out = Vec::new();
        for _ in 0..OCR_EVENT_POLL_BUDGET {
            match self.evt_rx.try_recv() {
                Ok(WorkerEvent::ModelDownloadStarted) => {
                    if self.state != OcrLoadState::DownloadingModel {
                        self.set_state(OcrLoadState::DownloadingModel);
                        out.push(OcrControllerEvent::StateChanged(
                            OcrLoadState::DownloadingModel,
                        ));
                    }
                }
                Ok(WorkerEvent::BackendLoadStarted) => {
                    if self.state != OcrLoadState::Loading {
                        self.set_state(OcrLoadState::Loading);
                        out.push(OcrControllerEvent::StateChanged(OcrLoadState::Loading));
                    }
                }
                Ok(WorkerEvent::LoadOk) => {
                    self.last_error = None;
                    if self.state != OcrLoadState::Ready {
                        self.set_state(OcrLoadState::Ready);
                        out.push(OcrControllerEvent::StateChanged(OcrLoadState::Ready));
                    }
                    if let Some(request) = self.pending_request.take()
                        && self.cmd_tx.send(WorkerCommand::Recognize(request)).is_err()
                    {
                        self.last_error = Some("OCR worker недоступен.".to_string());
                        self.set_state(OcrLoadState::Error);
                        out.push(OcrControllerEvent::StateChanged(OcrLoadState::Error));
                    }
                }
                Ok(WorkerEvent::LoadErr(err)) => {
                    let dropped_request_id = self.pending_request.take().map(|req| req.request_id);
                    self.last_error = Some(err.clone());
                    self.set_state(OcrLoadState::Error);
                    out.push(OcrControllerEvent::StateChanged(OcrLoadState::Error));
                    if let Some(request_id) = dropped_request_id {
                        out.push(OcrControllerEvent::RecognizeFailed {
                            request_id,
                            error: err,
                        });
                    }
                }
                Ok(WorkerEvent::RecognizeOk { request_id, result }) => {
                    self.last_error = None;
                    if self.state != OcrLoadState::Ready {
                        self.set_state(OcrLoadState::Ready);
                        out.push(OcrControllerEvent::StateChanged(OcrLoadState::Ready));
                    }
                    self.last_result = Some(result.clone());
                    out.push(OcrControllerEvent::Recognized { request_id, result });
                }
                Ok(WorkerEvent::RecognizeErr { request_id, error }) => {
                    self.last_error = Some(error.clone());
                    self.set_state(OcrLoadState::Error);
                    out.push(OcrControllerEvent::StateChanged(OcrLoadState::Error));
                    out.push(OcrControllerEvent::RecognizeFailed { request_id, error });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.last_error = Some("OCR worker отключился.".to_string());
                    self.set_state(OcrLoadState::Error);
                    out.push(OcrControllerEvent::StateChanged(OcrLoadState::Error));
                    break;
                }
            }
        }
        out
    }

    fn set_state(&mut self, new_state: OcrLoadState) {
        self.state = new_state;
    }
}

impl Drop for TranslationOcrController {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCommand::Stop);
        if let Some(handle) = self.worker_thread.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Debug)]
enum WorkerCommand {
    Load {
        engine: OcrEngine,
        options: OcrRuntimeOptions,
    },
    Recognize(OcrRecognizeRequest),
    Stop,
}

#[derive(Debug)]
enum WorkerEvent {
    ModelDownloadStarted,
    BackendLoadStarted,
    LoadOk,
    LoadErr(String),
    RecognizeOk {
        request_id: u64,
        result: OcrRecognizeResult,
    },
    RecognizeErr {
        request_id: u64,
        error: String,
    },
}

fn worker_loop(cmd_rx: Receiver<WorkerCommand>, evt_tx: Sender<WorkerEvent>) {
    let mut http_client = BackendHttpClient::default();
    let mut page_cache = PageImageCache::new(OCR_PAGE_CACHE_MAX_ITEMS, OCR_PAGE_CACHE_MAX_BYTES);

    while let Ok(command) = cmd_rx.recv() {
        match command {
            WorkerCommand::Stop => break,
            WorkerCommand::Load { engine, options } => {
                match warmup_ocr_engine(engine, &options, &mut http_client, &evt_tx) {
                    Ok(()) => {
                        let _ = evt_tx.send(WorkerEvent::LoadOk);
                    }
                    Err(err) => {
                        let _ = evt_tx.send(WorkerEvent::LoadErr(err));
                    }
                }
            }
            WorkerCommand::Recognize(request) => {
                let request_id = request.request_id;
                match run_ocr_request(&request, &mut page_cache, &mut http_client) {
                    Ok(result) => {
                        let _ = evt_tx.send(WorkerEvent::RecognizeOk { request_id, result });
                    }
                    Err(err) => {
                        let _ = evt_tx.send(WorkerEvent::RecognizeErr {
                            request_id,
                            error: err,
                        });
                    }
                }
            }
        }
    }
}

fn warmup_ocr_engine(
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
    http_client: &mut BackendHttpClient,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<(), String> {
    ensure_backend_ocr_models(engine, options, evt_tx)?;
    let _ = evt_tx.send(WorkerEvent::BackendLoadStarted);
    let endpoint = engine
        .endpoint_path()
        .ok_or_else(|| "Не задан endpoint OCR backend.".to_string())?;
    let payload = json!({
        "image_base64": DUMMY_PNG_BASE64,
        "join_newlines": true,
        "reflect_strings": false,
        "manga_model": options.manga_model,
        "paddle_lang": options.paddle_lang,
        "easy_langs": options.easy_langs,
        "surya_task_name": options.surya_task_name,
        "surya_recognize_math": options.surya_recognize_math,
        "surya_sort_lines": options.surya_sort_lines,
        "surya_drop_repeated_text": options.surya_drop_repeated_text,
        "surya_max_sliding_window": non_zero_u32_to_option(options.surya_max_sliding_window),
        "surya_max_tokens": non_zero_u32_to_option(options.surya_max_tokens)
    });
    let response = http_client.post_json(endpoint, &payload.to_string())?;
    parse_backend_ok_only(&response)
}

fn run_ocr_request(
    request: &OcrRecognizeRequest,
    page_cache: &mut PageImageCache,
    http_client: &mut BackendHttpClient,
) -> Result<OcrRecognizeResult, String> {
    ensure_backend_ocr_models_for_request(request.engine, &request.options)?;
    let endpoint = request
        .engine
        .endpoint_path()
        .ok_or_else(|| "Не задан endpoint OCR backend.".to_string())?;
    let crop_png = crop_image_as_png(request, page_cache)?;
    let payload = json!({
        "image_base64": base64_encode(&crop_png),
        "join_newlines": request.join_newlines,
        "reflect_strings": request.reflect_strings,
        "manga_model": request.options.manga_model,
        "paddle_lang": request.options.paddle_lang,
        "easy_langs": request.options.easy_langs,
        "surya_task_name": request.options.surya_task_name,
        "surya_recognize_math": request.options.surya_recognize_math,
        "surya_sort_lines": request.options.surya_sort_lines,
        "surya_drop_repeated_text": request.options.surya_drop_repeated_text,
        "surya_max_sliding_window": non_zero_u32_to_option(request.options.surya_max_sliding_window),
        "surya_max_tokens": non_zero_u32_to_option(request.options.surya_max_tokens)
    });
    let response = http_client.post_json(endpoint, &payload.to_string())?;
    parse_ocr_response(&response)
}

fn ensure_backend_ocr_models(
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<(), String> {
    let mut reported = false;
    let mut report_download = || {
        if !reported {
            let _ = evt_tx.send(WorkerEvent::ModelDownloadStarted);
            reported = true;
        }
    };
    ensure_backend_ocr_models_inner(engine, options, Some(&mut report_download))
}

fn ensure_backend_ocr_models_for_request(
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
) -> Result<(), String> {
    ensure_backend_ocr_models_inner(engine, options, None)
}

fn ensure_backend_ocr_models_inner(
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
    reporter: ai_models::ModelDownloadReporter<'_>,
) -> Result<(), String> {
    let models_root = config::models_dir();
    match engine {
        OcrEngine::PaddleOcr => {
            ai_models::ensure_paddle_ocr_full_with_reporter(
                &models_root,
                &options.paddle_lang,
                reporter,
            )?;
        }
        OcrEngine::MangaOcr => {
            if let Some(model) = ai_models::manga_ocr_model_from_key(&options.manga_model) {
                ai_models::ensure_manga_ocr_onnx_with_reporter(&models_root, model, reporter)?;
            }
        }
        OcrEngine::EasyOcr | OcrEngine::Surya => {}
    }
    Ok(())
}

fn crop_image_as_png(
    request: &OcrRecognizeRequest,
    page_cache: &mut PageImageCache,
) -> Result<Vec<u8>, String> {
    if let Some(image_override_png) = request.image_override_png.as_ref() {
        return Ok(image_override_png.clone());
    }
    let crop = crop_image(request, page_cache)?;
    encode_png(crop)
}

fn crop_image(
    request: &OcrRecognizeRequest,
    page_cache: &mut PageImageCache,
) -> Result<DynamicImage, String> {
    if let Some(image_override_png) = request.image_override_png.as_ref() {
        return image::load_from_memory(image_override_png)
            .map_err(|err| format!("Не удалось декодировать overlay PNG для OCR: {err}"));
    }
    let source = page_cache.get_or_load(&request.page_path)?;
    let (img_w, img_h) = source.dimensions();
    if img_w == 0 || img_h == 0 {
        return Err(format!(
            "Пустое изображение для OCR: {}",
            request.page_path.display()
        ));
    }

    let [u1, v1, u2, v2] = normalized_uv(request.uv_rect);
    let x1 = ((u1 * img_w as f32).floor() as u32).min(img_w.saturating_sub(1));
    let y1 = ((v1 * img_h as f32).floor() as u32).min(img_h.saturating_sub(1));
    let x2 = ((u2 * img_w as f32).ceil() as u32).min(img_w);
    let y2 = ((v2 * img_h as f32).ceil() as u32).min(img_h);

    if x2 <= x1 || y2 <= y1 {
        return Err("Выделение OCR слишком маленькое.".to_string());
    }
    Ok(source.crop_imm(x1, y1, x2 - x1, y2 - y1))
}

fn encode_png(image: DynamicImage) -> Result<Vec<u8>, String> {
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image.to_rgb8())
        .write_to(&mut cursor, ImageFormat::Png)
        .map_err(|err| format!("Не удалось сериализовать crop в PNG: {err}"))?;
    Ok(cursor.into_inner())
}

fn normalized_uv(uv: [f32; 4]) -> [f32; 4] {
    let left = uv[0].min(uv[2]).clamp(0.0, 1.0);
    let right = uv[0].max(uv[2]).clamp(0.0, 1.0);
    let top = uv[1].min(uv[3]).clamp(0.0, 1.0);
    let bottom = uv[1].max(uv[3]).clamp(0.0, 1.0);
    [left, top, right, bottom]
}

fn parse_backend_ok_only(response: &Value) -> Result<(), String> {
    if response.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        Ok(())
    } else {
        let error = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Неизвестная ошибка backend.");
        Err(error.to_string())
    }
}

fn non_zero_u32_to_option(value: u32) -> Option<u32> {
    if value == 0 { None } else { Some(value) }
}

fn parse_ocr_response(response: &Value) -> Result<OcrRecognizeResult, String> {
    parse_backend_ok_only(response)?;

    let mut lines = response
        .get("lines")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut text = response
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    if lines.is_empty() && !text.is_empty() {
        lines = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
    }
    if text.is_empty() && !lines.is_empty() {
        text = lines.join("\n");
    }

    Ok(OcrRecognizeResult { lines, text })
}

#[derive(Default)]
struct BackendHttpClient {
    stream: Option<TcpStream>,
}

enum PostJsonError {
    Transport(String),
    Backend(String),
}

impl BackendHttpClient {
    fn post_json(&mut self, path: &str, payload: &str) -> Result<Value, String> {
        if let Err(err) = ensure_ai_backend_healthy() {
            self.close_stream();
            return Err(err);
        }

        let mut attempt = 0usize;
        loop {
            match self.post_json_once(path, payload) {
                Ok(value) => return Ok(value),
                Err(PostJsonError::Backend(err)) => return Err(err),
                Err(PostJsonError::Transport(err)) => {
                    if attempt >= OCR_HTTP_RETRY_LIMIT {
                        return Err(err);
                    }
                    attempt += 1;
                    self.close_stream();
                }
            }
        }
    }

    fn post_json_once(&mut self, path: &str, payload: &str) -> Result<Value, PostJsonError> {
        self.ensure_connected()?;
        let Some(stream) = self.stream.as_mut() else {
            return Err(PostJsonError::Transport(
                "OCR backend stream не инициализирован.".to_string(),
            ));
        };

        let req_head = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
            ai_backend_addr_text(),
            payload.len()
        );
        if let Err(err) = stream.write_all(req_head.as_bytes()) {
            self.close_stream();
            return Err(PostJsonError::Transport(format!(
                "Не удалось отправить OCR HTTP-заголовки: {err}"
            )));
        }
        if let Err(err) = stream.write_all(payload.as_bytes()) {
            self.close_stream();
            return Err(PostJsonError::Transport(format!(
                "Не удалось отправить OCR HTTP-body: {err}"
            )));
        }

        let (status_code, body, should_close) = match read_http_response_keep_alive(stream) {
            Ok(parts) => parts,
            Err(err) => {
                self.close_stream();
                return Err(PostJsonError::Transport(err));
            }
        };
        if should_close {
            self.close_stream();
        }

        let body_text = String::from_utf8_lossy(&body).to_string();
        let json_value: Value = serde_json::from_str(&body_text).map_err(|err| {
            PostJsonError::Transport(format!("OCR backend вернул не-JSON ({status_code}): {err}"))
        })?;

        if status_code >= 400 {
            let msg = json_value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Ошибка OCR backend.");
            return Err(PostJsonError::Backend(format!(
                "OCR backend HTTP {status_code}: {msg}"
            )));
        }

        Ok(json_value)
    }

    fn ensure_connected(&mut self) -> Result<(), PostJsonError> {
        if self.stream.is_some() {
            return Ok(());
        }
        self.stream = Some(connect_backend_stream().map_err(PostJsonError::Transport)?);
        Ok(())
    }

    fn close_stream(&mut self) {
        self.stream = None;
    }
}

fn connect_backend_stream() -> Result<TcpStream, String> {
    let addr = ai_backend_addr_text();
    let socket_addr = addr
        .to_socket_addrs()
        .map_err(|err| format!("Ошибка резолва OCR backend {addr}: {err}"))?
        .next()
        .ok_or_else(|| format!("Не найден сокет OCR backend: {addr}"))?;

    let stream = TcpStream::connect_timeout(&socket_addr, OCR_BACKEND_CONNECT_TIMEOUT)
        .map_err(|err| format!("Не удалось подключиться к OCR backend: {err}"))?;
    stream
        .set_read_timeout(Some(OCR_BACKEND_READ_TIMEOUT))
        .map_err(|err| format!("Не удалось выставить read timeout OCR: {err}"))?;
    stream
        .set_write_timeout(Some(OCR_BACKEND_WRITE_TIMEOUT))
        .map_err(|err| format!("Не удалось выставить write timeout OCR: {err}"))?;
    stream
        .set_nodelay(true)
        .map_err(|err| format!("Не удалось включить TCP_NODELAY OCR: {err}"))?;
    Ok(stream)
}

fn read_http_response_keep_alive(stream: &mut TcpStream) -> Result<(u16, Vec<u8>, bool), String> {
    let mut raw = Vec::with_capacity(8 * 1024);
    let mut chunk = [0_u8; 8 * 1024];
    let mut header_end: Option<usize> = None;
    let mut expected_total_len: Option<usize> = None;

    loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|err| format!("Не удалось прочитать ответ OCR backend: {err}"))?;
        if read == 0 {
            return Err("OCR backend закрыл соединение до завершения HTTP-ответа.".to_string());
        }
        raw.extend_from_slice(&chunk[..read]);

        if header_end.is_none() {
            header_end = raw.windows(4).position(|window| window == b"\r\n\r\n");
            if let Some(end) = header_end {
                let head = String::from_utf8_lossy(&raw[..end]);
                if let Some(content_len) = parse_http_content_length(&head) {
                    expected_total_len = Some(end + 4 + content_len);
                }
            }
        }

        if let Some(total_len) = expected_total_len {
            if raw.len() >= total_len {
                break;
            }
            continue;
        }

        if header_end.is_some() {
            // Fallback: если backend не прислал Content-Length, читаем до закрытия.
            stream
                .read_to_end(&mut raw)
                .map_err(|err| format!("Не удалось дочитать ответ OCR backend: {err}"))?;
            break;
        }
    }

    let Some(end) = header_end else {
        return Err("Некорректный HTTP-ответ OCR backend (нет заголовков).".to_string());
    };
    let head = String::from_utf8_lossy(&raw[..end]);
    let status_line = head.lines().next().unwrap_or("HTTP/1.1 500");
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|part| part.parse::<u16>().ok())
        .ok_or_else(|| format!("Не удалось распарсить HTTP-статус OCR backend: {status_line}"))?;
    let body = raw[end + 4..].to_vec();
    let should_close = http_connection_should_close(&head);
    Ok((status_code, body, should_close))
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

fn http_connection_should_close(head: &str) -> bool {
    for line in head.lines() {
        let mut parts = line.splitn(2, ':');
        let Some(key) = parts.next() else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("Connection") {
            continue;
        }
        let value = parts.next().unwrap_or("").trim();
        if value.eq_ignore_ascii_case("close") {
            return true;
        }
    }
    false
}

struct CachedPageImage {
    image: DynamicImage,
    approx_bytes: usize,
}

struct PageImageCache {
    max_items: usize,
    max_bytes: usize,
    total_bytes: usize,
    entries: HashMap<PathBuf, CachedPageImage>,
    lru: VecDeque<PathBuf>,
}

impl PageImageCache {
    fn new(max_items: usize, max_bytes: usize) -> Self {
        Self {
            max_items: max_items.max(1),
            max_bytes: max_bytes.max(4 * 1024 * 1024),
            total_bytes: 0,
            entries: HashMap::new(),
            lru: VecDeque::new(),
        }
    }

    fn get_or_load(&mut self, path: &Path) -> Result<&DynamicImage, String> {
        if self.entries.contains_key(path) {
            self.touch(path);
            return self
                .entries
                .get(path)
                .map(|entry| &entry.image)
                .ok_or_else(|| "Не удалось получить изображение из OCR cache.".to_string());
        }

        let image = image::open(path).map_err(|err| {
            format!(
                "Не удалось открыть изображение для OCR ({}): {err}",
                path.display()
            )
        })?;
        let approx_bytes = approx_image_size_bytes(&image);
        let key = path.to_path_buf();

        self.total_bytes = self.total_bytes.saturating_add(approx_bytes);
        self.entries.insert(
            key.clone(),
            CachedPageImage {
                image,
                approx_bytes,
            },
        );
        self.touch(path);
        self.evict_if_needed();

        self.entries
            .get(path)
            .map(|entry| &entry.image)
            .ok_or_else(|| "Не удалось сохранить изображение в OCR cache.".to_string())
    }

    fn touch(&mut self, path: &Path) {
        self.lru.retain(|item| item.as_path() != path);
        self.lru.push_back(path.to_path_buf());
    }

    fn evict_if_needed(&mut self) {
        while self.should_evict() {
            let Some(oldest) = self.lru.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.approx_bytes);
            }
        }
    }

    fn should_evict(&self) -> bool {
        if self.entries.len() > self.max_items {
            return true;
        }
        self.total_bytes > self.max_bytes && self.entries.len() > 1
    }
}

fn approx_image_size_bytes(image: &DynamicImage) -> usize {
    let (w, h) = image.dimensions();
    (w as usize).saturating_mul(h as usize).saturating_mul(4)
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if data.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0usize;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
        i += 3;
    }

    let rem = data.len() - i;
    if rem == 1 {
        let n = (data[i] as u32) << 16;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }

    out
}
