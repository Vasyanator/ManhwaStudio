// ============================================================================
// OCR CONTROLLER (Translation tab)
// ----------------------------------------------------------------------------
// Что в файле:
// - `TranslationOcrController`: state-машина OCR
//   (NotLoaded/DownloadingModel/Loading/Ready/Error),
//   очередь команд в worker и публикация событий в UI.
// - Worker-команды: загрузка движка и OCR-запросы с возвратом результата/ошибки.
// - Runtime-оптимизации:
//   1) общий `BackendClient` (framed v2 IPC) к Python backend через AF_UNIX
//      сокет: одно соединение + фоновый reader-поток на все OCR-запросы, без
//      reconnect на каждый запрос; запрос выполняется через `CallHandle`,
//      который можно отменить по id при суперсиде,
//   2) LRU-кэш декодированных страниц для повторных crop при OCR по блокам.
//   3) advanced-recognition может передать уже скомпозитенный PNG crop с
//      пользовательским оверлеем, который worker использует вместо crop страницы.
// - AI API OCR uses `genai` multimodal chat calls from the worker thread and
//   stores provider API keys only through the OS credential store.
// - Post-OCR character substitution (`CharReplacementRule`) is applied to the
//   recognized result in the worker before it is published, so every engine
//   path and the stored last result share the same substituted text.
// - Вспомогательные функции: crop по UV, PNG-кодирование, сборка/разбор
//   framed-заголовков `BackendClient`/`CallHandle` и JSON.
// ============================================================================
use crate::backend_ipc::{self, CallError, CallHandle};
use crate::tabs::translation::backend_health::ai_backend_offline_error;
use crate::{ai_models, config};
// Native ONNX Runtime OCR path (Phase 1: MangaOCR only). Desktop-only: the native
// runtime + ORT loader depend on `ms-onnx`/`ort`, which are not part of the web build.
#[cfg(not(target_arch = "wasm32"))]
use crate::native_runtime;
#[cfg(not(target_arch = "wasm32"))]
use crate::onnx_runtime::{OrtDownloadProgress, OrtDownloadStage};
// AI API OCR (`genai`) is native-only: the crate is not compiled for wasm. The
// worker command/event enums and the controller stay target-neutral; only the
// bodies that call `genai`/`tokio`/`ureq`/`keyring` are gated below.
#[cfg(not(target_arch = "wasm32"))]
use genai::adapter::AdapterKind;
#[cfg(not(target_arch = "wasm32"))]
use genai::chat::{ChatMessage, ChatRequest, ContentPart};
#[cfg(not(target_arch = "wasm32"))]
use genai::resolver::{AuthData, AuthResolver, ProviderConfig};
#[cfg(not(target_arch = "wasm32"))]
use genai::{Client, ModelIden};
use image::{DynamicImage, GenericImageView, ImageFormat};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use ms_thread::{self as thread, JoinHandle};
use web_time::Duration;

/// Per-OCR-request timeout for the v2 framed `call`. Mirrors the previous HTTP
/// read timeout: model warmup + recognition can take a while on first use.
const OCR_BACKEND_CALL_TIMEOUT: Duration = Duration::from_secs(300);
/// Upper bound for draining the waiter's final message after a cancel. Set just
/// above `OCR_BACKEND_CALL_TIMEOUT` so that, even if a live-but-slow backend never
/// honours the cancel, the waiter's own `wait` timeout fires first and we never
/// block the OCR worker thread indefinitely.
const OCR_SUPERSEDE_DRAIN_TIMEOUT: Duration = Duration::from_secs(310);
const OCR_EVENT_POLL_BUDGET: usize = 16;
/// Bounded retry for transport-level (connection/framing) failures. `shared_client()`
/// auto-reconnects, so one retry after a dead-connection is enough before surfacing.
const OCR_BACKEND_TRANSPORT_RETRY_LIMIT: usize = 1;
const OCR_PAGE_CACHE_MAX_ITEMS: usize = 8;
const OCR_PAGE_CACHE_MAX_BYTES: usize = 256 * 1024 * 1024;

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
    PaddleVl,
    Surya,
    AiApi,
}

impl OcrEngine {
    /// The v2 framed-protocol method name for this engine, or `None` for the
    /// AI-API engine (which runs over `genai`, not the backend socket).
    fn backend_method(self) -> Option<&'static str> {
        use crate::backend_ipc::protocol;
        match self {
            OcrEngine::MangaOcr => Some(protocol::METHOD_OCR_MANGA),
            OcrEngine::EasyOcr => Some(protocol::METHOD_OCR_EASY),
            OcrEngine::PaddleOcr => Some(protocol::METHOD_OCR_PADDLE),
            OcrEngine::PaddleVl => Some(protocol::METHOD_OCR_PADDLE_VL),
            OcrEngine::Surya => Some(protocol::METHOD_OCR_SURYA),
            OcrEngine::AiApi => None,
        }
    }

    pub fn requires_backend(self) -> bool {
        !matches!(self, Self::AiApi) && self.backend_method().is_some()
    }
}

#[derive(Debug, Clone)]
pub struct OcrRuntimeOptions {
    pub manga_model: String,
    pub paddle_lang: String,
    pub paddle_vl_script: String,
    pub easy_langs: String,
    pub surya_task_name: String,
    pub surya_recognize_math: bool,
    pub surya_sort_lines: bool,
    pub surya_drop_repeated_text: bool,
    pub surya_max_sliding_window: u32,
    pub surya_max_tokens: u32,
    pub ai_api_service: AiApiService,
    pub ai_api_model: String,
    pub ai_api_system_instruction: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AiApiService {
    OpenAi,
    Anthropic,
    Gemini,
    OpenRouter,
    Groq,
    DeepSeek,
    Xai,
}

impl AiApiService {
    pub const ALL: [Self; 7] = [
        Self::OpenAi,
        Self::Anthropic,
        Self::Gemini,
        Self::OpenRouter,
        Self::Groq,
        Self::DeepSeek,
        Self::Xai,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::OpenRouter => "open_router",
            Self::Groq => "groq",
            Self::DeepSeek => "deepseek",
            Self::Xai => "xai",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Gemini",
            Self::OpenRouter => "OpenRouter",
            Self::Groq => "Groq",
            Self::DeepSeek => "DeepSeek",
            Self::Xai => "xAI",
        }
    }

    /// Maps this service to its `genai` adapter. Native-only: `genai` (and the
    /// `AdapterKind` type) is not compiled for the web build.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn adapter_kind(self) -> AdapterKind {
        match self {
            Self::OpenAi => AdapterKind::OpenAI,
            Self::Anthropic => AdapterKind::Anthropic,
            Self::Gemini => AdapterKind::Gemini,
            Self::OpenRouter => AdapterKind::OpenRouter,
            Self::Groq => AdapterKind::Groq,
            Self::DeepSeek => AdapterKind::DeepSeek,
            Self::Xai => AdapterKind::Xai,
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-4o-mini",
            Self::Anthropic => "claude-3-5-haiku-latest",
            Self::Gemini => "gemini-2.5-flash",
            Self::OpenRouter => "open_router::google/gemini-2.0-flash-001",
            Self::Groq => "groq::meta-llama/llama-4-scout-17b-16e-instruct",
            Self::DeepSeek => "deepseek-chat",
            Self::Xai => "grok-2-vision-1212",
        }
    }

    pub fn from_key(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Self::Anthropic,
            "gemini" | "google" => Self::Gemini,
            "openrouter" | "open_router" => Self::OpenRouter,
            "groq" => Self::Groq,
            "deepseek" | "deep_seek" => Self::DeepSeek,
            "xai" | "x_ai" | "grok" => Self::Xai,
            _ => Self::OpenAi,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AiApiMetadata {
    pub service: AiApiService,
    pub key_configured: bool,
    pub models: Vec<String>,
    pub account_status: String,
}

/// A single post-OCR character-substitution rule applied to recognized text.
///
/// Every occurrence of any string in `targets` is replaced by `replacement`
/// (literal, non-overlapping, left-to-right). Rules carry only already-parsed
/// targets; UI-level enable flags and quoted-list parsing live in the panel.
#[derive(Debug, Clone)]
pub struct CharReplacementRule {
    pub targets: Vec<String>,
    pub replacement: String,
}

impl CharReplacementRule {
    /// Applies this rule to `text`, replacing each non-empty target in order.
    fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for target in &self.targets {
            if target.is_empty() {
                continue;
            }
            out = out.replace(target.as_str(), &self.replacement);
        }
        out
    }
}

/// Applies all `rules` in order to both the joined `text` and each line of
/// `result`, keeping the recognized text and its line breakdown consistent.
fn apply_char_replacements(result: &mut OcrRecognizeResult, rules: &[CharReplacementRule]) {
    if rules.is_empty() {
        return;
    }
    for rule in rules {
        result.text = rule.apply(&result.text);
        for line in &mut result.lines {
            *line = rule.apply(line);
        }
    }
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
    /// Post-OCR character substitutions applied to the recognized result before
    /// it is published. Empty means no substitution.
    pub char_replacements: Vec<CharReplacementRule>,
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
    AiApiKeyStored {
        service: AiApiService,
    },
    AiApiKeyCleared {
        service: AiApiService,
    },
    AiApiMetadataLoaded(AiApiMetadata),
    AiApiMetadataFailed {
        service: AiApiService,
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
            self.last_error = Some(t!("translation.ocr.worker_unavailable_error").to_string());
            self.set_state(OcrLoadState::Error);
        }
    }

    pub fn request_recognize(&mut self, request: OcrRecognizeRequest) {
        match self.state {
            OcrLoadState::Ready => {
                if self.cmd_tx.send(WorkerCommand::Recognize(request)).is_err() {
                    self.last_error = Some(t!("translation.ocr.worker_unavailable_error").to_string());
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

    pub fn store_ai_api_key(&mut self, service: AiApiService, api_key: String) {
        if self
            .cmd_tx
            .send(WorkerCommand::StoreAiApiKey { service, api_key })
            .is_err()
        {
            self.last_error = Some(t!("translation.ocr.worker_unavailable_error").to_string());
        }
    }

    pub fn clear_ai_api_key(&mut self, service: AiApiService) {
        if self
            .cmd_tx
            .send(WorkerCommand::ClearAiApiKey { service })
            .is_err()
        {
            self.last_error = Some(t!("translation.ocr.worker_unavailable_error").to_string());
        }
    }

    pub fn refresh_ai_api_metadata(&mut self, service: AiApiService) {
        if self
            .cmd_tx
            .send(WorkerCommand::RefreshAiApiMetadata { service })
            .is_err()
        {
            self.last_error = Some(t!("translation.ocr.worker_unavailable_error").to_string());
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
                        self.last_error = Some(t!("translation.ocr.worker_unavailable_error").to_string());
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
                Ok(WorkerEvent::AiApiKeyStored { service }) => {
                    out.push(OcrControllerEvent::AiApiKeyStored { service });
                }
                Ok(WorkerEvent::AiApiKeyCleared { service }) => {
                    out.push(OcrControllerEvent::AiApiKeyCleared { service });
                }
                Ok(WorkerEvent::AiApiMetadataLoaded(metadata)) => {
                    out.push(OcrControllerEvent::AiApiMetadataLoaded(metadata));
                }
                Ok(WorkerEvent::AiApiMetadataErr { service, error }) => {
                    out.push(OcrControllerEvent::AiApiMetadataFailed { service, error });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.last_error = Some(t!("translation.ocr.worker_disconnected_error").to_string());
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
    StoreAiApiKey {
        service: AiApiService,
        api_key: String,
    },
    ClearAiApiKey {
        service: AiApiService,
    },
    RefreshAiApiMetadata {
        service: AiApiService,
    },
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
    AiApiKeyStored {
        service: AiApiService,
    },
    AiApiKeyCleared {
        service: AiApiService,
    },
    AiApiMetadataLoaded(AiApiMetadata),
    AiApiMetadataErr {
        service: AiApiService,
        error: String,
    },
}

fn worker_loop(cmd_rx: Receiver<WorkerCommand>, evt_tx: Sender<WorkerEvent>) {
    let mut page_cache = PageImageCache::new(OCR_PAGE_CACHE_MAX_ITEMS, OCR_PAGE_CACHE_MAX_BYTES);

    while let Ok(command) = cmd_rx.recv() {
        match command {
            WorkerCommand::Stop => break,
            WorkerCommand::Load { engine, options } => {
                match warmup_ocr_engine(engine, &options, &evt_tx) {
                    Ok(()) => {
                        let _ = evt_tx.send(WorkerEvent::LoadOk);
                    }
                    Err(err) => {
                        let _ = evt_tx.send(WorkerEvent::LoadErr(err));
                    }
                }
            }
            WorkerCommand::Recognize(request) => {
                if run_recognize_command(request, &mut page_cache, &cmd_rx, &evt_tx).is_break() {
                    break;
                }
            }
            WorkerCommand::StoreAiApiKey { service, api_key } => {
                match store_ai_api_key(service, &api_key) {
                    Ok(()) => {
                        let _ = evt_tx.send(WorkerEvent::AiApiKeyStored { service });
                    }
                    Err(error) => {
                        let _ = evt_tx.send(WorkerEvent::AiApiMetadataErr { service, error });
                    }
                }
            }
            WorkerCommand::ClearAiApiKey { service } => match clear_ai_api_key(service) {
                Ok(()) => {
                    let _ = evt_tx.send(WorkerEvent::AiApiKeyCleared { service });
                }
                Err(error) => {
                    let _ = evt_tx.send(WorkerEvent::AiApiMetadataErr { service, error });
                }
            },
            WorkerCommand::RefreshAiApiMetadata { service } => {
                match load_ai_api_metadata(service) {
                    Ok(metadata) => {
                        let _ = evt_tx.send(WorkerEvent::AiApiMetadataLoaded(metadata));
                    }
                    Err(error) => {
                        let _ = evt_tx.send(WorkerEvent::AiApiMetadataErr { service, error });
                    }
                }
            }
        }
    }
}

fn warmup_ocr_engine(
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<(), String> {
    if engine == OcrEngine::AiApi {
        return validate_ai_api_options(options);
    }

    // Route-aware readiness gate. When the active route is the native ONNX Runtime,
    // the Python backend is NOT required: native inference lazy-loads the runtime +
    // model on the first recognize (download progress is surfaced then). Reaching
    // `Ready` here without warming the backend is exactly what lets native OCR run
    // fully offline. A later native inference error still falls back to the backend
    // *if it is up* — that fallback path re-checks backend readiness on its own, so
    // the native->backend contract is preserved without forcing the backend up now.
    // Desktop-only: the native runtime is compiled out on the web build.
    #[cfg(not(target_arch = "wasm32"))]
    if !ocr_route_needs_backend_warmup(current_ocr_route(engine, &options.manga_model)) {
        crate::runtime_log::log_info(
            "[ocr] native AI runtime route active; skipping Python backend warmup \
             (native OCR loads lazily on first recognize).",
        );
        return Ok(());
    }

    warmup_backend_ocr_engine(engine, options, evt_tx)
}

/// Whether an OCR route requires warming the Python backend at load time.
///
/// Only [`OcrRoute::Backend`] needs the backend; the native routes lazy-load the
/// in-process ONNX Runtime on first recognize and therefore reach `Ready` without
/// the backend. Pure so the warmup decision is unit-testable.
#[cfg(not(target_arch = "wasm32"))]
fn ocr_route_needs_backend_warmup(route: OcrRoute) -> bool {
    match route {
        OcrRoute::NativeManga(_) | OcrRoute::NativePaddle => false,
        OcrRoute::Backend => true,
    }
}

fn warmup_backend_ocr_engine(
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<(), String> {
    ensure_backend_ocr_models(engine, options, evt_tx)?;
    ensure_v2_backend_ready()?;
    let _ = evt_tx.send(WorkerEvent::BackendLoadStarted);
    let method = engine
        .backend_method()
        .ok_or_else(|| t!("translation.ocr.method_not_set_error").to_string())?;
    // Warm the per-engine worker/model by recognizing a tiny throwaway image.
    // The header carries the engine params; the blob carries the raw PNG bytes.
    let header_fields = ocr_header_fields(options, true, false);
    let dummy_png = dummy_warmup_png()?;
    // A warmup that races against a cancel just returns; treat that as success
    // (the model still got touched). Errors/transport are surfaced normally.
    match ocr_backend_call(method, header_fields, &dummy_png) {
        Ok(_) => Ok(()),
        Err(CallError::Interrupted(_)) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

/// Builds the inline header fields (engine params) shared by every backend OCR
/// method. The backend reads only the fields its method needs and ignores the
/// rest, so sending the full superset keeps a single builder for all engines.
/// The image itself is NOT in the header — it travels as the request blob.
fn ocr_header_fields(
    options: &OcrRuntimeOptions,
    join_newlines: bool,
    reflect_strings: bool,
) -> Value {
    json!({
        "join_newlines": join_newlines,
        "reflect_strings": reflect_strings,
        "manga_model": options.manga_model,
        "paddle_lang": options.paddle_lang,
        "paddle_vl_script": options.paddle_vl_script,
        "easy_langs": options.easy_langs,
        "surya_task_name": options.surya_task_name,
        "surya_recognize_math": options.surya_recognize_math,
        "surya_sort_lines": options.surya_sort_lines,
        "surya_drop_repeated_text": options.surya_drop_repeated_text,
        "surya_max_sliding_window": non_zero_u32_to_option(options.surya_max_sliding_window),
        "surya_max_tokens": non_zero_u32_to_option(options.surya_max_tokens)
    })
}

/// v2 readiness gate replacing the legacy HTTP `/health` precondition. A
/// successful `shared_client()` performs the `hello` handshake, which fails fast
/// when the backend is not running; that failure is mapped to the same unified
/// "backend offline" message the HTTP path showed, preserving the UX.
fn ensure_v2_backend_ready() -> Result<(), String> {
    backend_ipc::shared_client()
        .map(|_| ())
        .map_err(|_| ai_backend_offline_error().to_string())
}

/// Issues a blocking backend OCR `call` with a bounded transport retry.
/// `shared_client()` auto-reconnects, so a transport failure is retried a small
/// number of times (re-resolving the shared client each attempt) then surfaced.
/// `Error` and `Interrupted` are returned to the caller unchanged. Used for the
/// warmup path, which does not need cancellation.
fn ocr_backend_call(
    method: &str,
    header_fields: Value,
    blob: &[u8],
) -> Result<(Value, Vec<u8>), CallError> {
    let mut attempt = 0usize;
    loop {
        let client = match backend_ipc::shared_client() {
            Ok(client) => client,
            Err(err) => {
                if attempt >= OCR_BACKEND_TRANSPORT_RETRY_LIMIT {
                    return Err(CallError::Transport(err));
                }
                attempt += 1;
                continue;
            }
        };
        match client.call(
            method,
            header_fields.clone(),
            blob,
            OCR_BACKEND_CALL_TIMEOUT,
        ) {
            Ok(ok) => return Ok(ok),
            Err(CallError::Transport(err)) => {
                if attempt >= OCR_BACKEND_TRANSPORT_RETRY_LIMIT {
                    return Err(CallError::Transport(err));
                }
                attempt += 1;
            }
            Err(other) => return Err(other),
        }
    }
}

/// Begins a cancellable backend OCR call with a bounded transport retry, returning
/// the shared client (so the caller can cancel the in-flight id) and the
/// in-flight [`CallHandle`].
fn begin_ocr_backend_call(
    method: &str,
    header_fields: Value,
    blob: &[u8],
) -> Result<(backend_ipc::BackendClient, CallHandle), CallError> {
    let mut attempt = 0usize;
    loop {
        let client = match backend_ipc::shared_client() {
            Ok(client) => client,
            Err(err) => {
                if attempt >= OCR_BACKEND_TRANSPORT_RETRY_LIMIT {
                    return Err(CallError::Transport(err));
                }
                attempt += 1;
                continue;
            }
        };
        match client.begin_call(method, header_fields.clone(), blob) {
            Ok(handle) => return Ok((client, handle)),
            Err(err) => {
                if attempt >= OCR_BACKEND_TRANSPORT_RETRY_LIMIT {
                    return Err(CallError::Transport(err));
                }
                attempt += 1;
            }
        }
    }
}

/// Publishes a recognize outcome to the controller. Returns `ControlFlow::Break`
/// if the event channel is gone (the worker should stop).
fn publish_recognize(
    request_id: u64,
    result: Result<OcrRecognizeResult, String>,
    evt_tx: &Sender<WorkerEvent>,
) -> std::ops::ControlFlow<()> {
    let event = match result {
        Ok(result) => WorkerEvent::RecognizeOk { request_id, result },
        Err(error) => WorkerEvent::RecognizeErr { request_id, error },
    };
    if evt_tx.send(event).is_err() {
        std::ops::ControlFlow::Break(())
    } else {
        std::ops::ControlFlow::Continue(())
    }
}

/// A tiny throwaway PNG used to warm an engine's model/worker without depending
/// on a real page crop. 8x8 white image, encoded once per warmup.
fn dummy_warmup_png() -> Result<Vec<u8>, String> {
    let image = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        8,
        8,
        image::Rgb([255, 255, 255]),
    ));
    encode_png(image)
}

/// Outcome of a single backend recognize, distinguishing a real result/error
/// from "superseded by a newer selection" (the interrupted/cancel outcome).
enum RecognizeOutcome {
    Done(Result<OcrRecognizeResult, String>),
    Superseded,
}

/// Where an OCR recognize request should run.
///
/// Kept target-neutral (its `NativeManga` variant carries the app-managed model
/// enum, not the native-only runtime type) so the decision helper is unit-testable
/// on every build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OcrRoute {
    /// Run MangaOCR natively via the in-process ONNX Runtime (given export).
    NativeManga(ai_models::MangaOcrOnnxModel),
    /// Run PaddleOCR natively via the in-process ONNX Runtime (language from the
    /// request options).
    NativePaddle,
    /// Run through the Python backend (the historical path).
    Backend,
}

/// Pure routing decision for an OCR recognize request.
///
/// Routes to a native variant only when the user selected the native AI runtime
/// AND the per-scope SIGILL load-guard is not `Suspect`. Under those conditions:
/// MangaOCR routes to [`OcrRoute::NativeManga`] iff `manga_model_key` maps to an
/// ONNX export (`base_onnx`/`2025_onnx`; `base_torch` has no native path);
/// PaddleOCR always routes to [`OcrRoute::NativePaddle`] (every paddle language has
/// a native path). Every other engine, and every non-native/Suspect case, routes
/// to [`OcrRoute::Backend`], so an unrecognized or torch-only selection never
/// silently takes the native path.
fn ocr_route(
    runtime: config::AiRuntime,
    engine: OcrEngine,
    manga_model_key: &str,
    guard: config::OrtLoadDecision,
) -> OcrRoute {
    // Native requires the native runtime and a non-Suspect load guard.
    if runtime != config::AiRuntime::Native || guard != config::OrtLoadDecision::Safe {
        return OcrRoute::Backend;
    }
    match engine {
        OcrEngine::MangaOcr => match ai_models::manga_ocr_model_from_key(manga_model_key) {
            // Torch-only (`base_torch`) or unknown keys map to `None` -> backend.
            Some(variant) => OcrRoute::NativeManga(variant),
            None => OcrRoute::Backend,
        },
        OcrEngine::PaddleOcr => OcrRoute::NativePaddle,
        // These engines have no native path yet; they use the Python backend.
        OcrEngine::EasyOcr
        | OcrEngine::PaddleVl
        | OcrEngine::Surya
        | OcrEngine::AiApi => OcrRoute::Backend,
    }
}

/// Route-aware decision for whether the Python backend is required to run the
/// selected OCR engine/model under `runtime` and the SIGILL load `guard`.
///
/// This is the single source of truth the UI readiness gates consult. It reuses
/// [`ocr_route`] so it can never disagree with the actual dispatch: a native
/// route (MangaOCR ONNX or PaddleOCR under the native runtime with a non-`Suspect`
/// guard) needs NO backend and returns `false`; every backend-routed engine/model
/// returns `true`. `AiApi` is special-cased to `false` because it runs over `genai`
/// and never touches the backend socket (its `ocr_route` value is `Backend` only
/// because `OcrRoute` has no AI-API variant). A `Suspect` guard falls back to the
/// backend, so it correctly reports the backend as required. Pure and testable.
pub(crate) fn ocr_requires_backend(
    engine: OcrEngine,
    manga_model_key: &str,
    runtime: config::AiRuntime,
    guard: config::OrtLoadDecision,
) -> bool {
    // Engines that never use the backend socket (AiApi over `genai`) never require
    // it, regardless of runtime/guard.
    if !engine.requires_backend() {
        return false;
    }
    match ocr_route(runtime, engine, manga_model_key, guard) {
        OcrRoute::NativeManga(_) | OcrRoute::NativePaddle => false,
        OcrRoute::Backend => true,
    }
}

/// Reads the native-routing inputs (selected AI runtime + provider-scoped SIGILL
/// load guard) fresh off disk.
///
/// Kept as one helper so [`current_ocr_route`] and the UI-side readiness gate read
/// the exact same inputs. Involves disk I/O; callers must not invoke it on the
/// GUI thread's hot path without their own caching. Desktop-only: the native
/// runtime is compiled out on the web build.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn read_ocr_route_inputs() -> (config::AiRuntime, config::OrtLoadDecision) {
    let cfg = config::load_raw_user_settings_for_startup().unwrap_or(Value::Null);
    let runtime = config::AiRuntime::from_user_settings(&cfg);
    let scope = native_runtime::native_load_scope_key();
    let decision = config::ort_load_decision(config::read_ort_load_guard(&cfg, &scope));
    (runtime, decision)
}

/// Assembles an [`OcrRecognizeResult`] from a single native MangaOCR string,
/// applying `join_newlines`/`reflect_strings` exactly like the backend path: split
/// on newlines and drop blank lines; reverse line order when `reflect_strings`;
/// join with `"\n"` (or `" "`) per `join_newlines`, then trim.
#[cfg(not(target_arch = "wasm32"))]
fn assemble_native_ocr_result(
    text: &str,
    join_newlines: bool,
    reflect_strings: bool,
) -> OcrRecognizeResult {
    let mut lines: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if reflect_strings {
        lines.reverse();
    }
    let separator = if join_newlines { "\n" } else { " " };
    let text = lines.join(separator).trim().to_string();
    OcrRecognizeResult { lines, text }
}

/// Logs the native->backend fallback once per process (for the case where the
/// native runtime is selected but the current engine/model has no native path).
#[cfg(not(target_arch = "wasm32"))]
fn log_native_fallback_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        crate::runtime_log::log_info(
            "[ocr] native AI runtime selected but the current OCR engine/model has no native path \
             (native covers MangaOCR ONNX + PaddleOCR); using the Python backend.",
        );
    }
}

/// Reads the selected AI runtime + SIGILL load guard fresh off disk (worker thread;
/// disk I/O ok) and returns the OCR route for `engine`/`manga_model_key`.
///
/// The guard is scoped to the effective native provider so the pre-check matches the
/// provider `native_runtime` will actually load. Shared by [`try_native_ocr`] (which
/// dispatches on it) and the load-time warmup (which uses it to skip the backend
/// warmup when the route is native), so both agree on the routing decision.
/// Mirrors `text_detector::current_detector_route`.
#[cfg(not(target_arch = "wasm32"))]
fn current_ocr_route(engine: OcrEngine, manga_model_key: &str) -> OcrRoute {
    let (runtime, decision) = read_ocr_route_inputs();
    ocr_route(runtime, engine, manga_model_key, decision)
}

/// Whether the native AI runtime is currently selected (independent of whether the
/// active engine/model has a native path). Worker-thread only (disk I/O). Used to
/// decide whether a `Backend` route for a native-runtime user warrants the
/// "no native path, using backend" log.
#[cfg(not(target_arch = "wasm32"))]
fn native_ai_runtime_selected() -> bool {
    let cfg = config::load_raw_user_settings_for_startup().unwrap_or(Value::Null);
    config::AiRuntime::from_user_settings(&cfg) == config::AiRuntime::Native
}

/// Outcome of an attempted native ONNX Runtime OCR recognize.
///
/// Distinguishes "this op has no native path, use the backend" from "the native
/// path was taken and failed" so the dispatcher can surface the real native error
/// when the backend cannot serve as a fallback. `Failed` carries the user-facing
/// localized message from [`native_runtime::NativeRuntimeError`]'s `Display`.
#[cfg(not(target_arch = "wasm32"))]
enum NativeOcrOutcome {
    /// Not a native route (unsupported engine, torch-only model, Suspect guard, or
    /// backend runtime): the request should use the Python backend.
    NotNative,
    /// Native recognition succeeded; publish this result.
    Ok(OcrRecognizeResult),
    /// The native route was taken but failed. The string is the user-facing error.
    Failed(String),
}

/// Attempts the native ONNX Runtime OCR path (MangaOCR or PaddleOCR) for `request`.
///
/// Returns [`NativeOcrOutcome::Ok`] on success, [`NativeOcrOutcome::NotNative`]
/// when the request has no native path (so the backend handles it), and
/// [`NativeOcrOutcome::Failed`] when the native route was taken but crop decode or
/// inference failed. The caller decides whether a `Failed` outcome falls back to
/// the backend (if it is up) or is surfaced to the user (if the backend is offline).
/// Failures are always logged with diagnostic context, never hidden. Runs on the
/// OCR worker thread (blocking download/inference).
#[cfg(not(target_arch = "wasm32"))]
fn try_native_ocr(
    request: &OcrRecognizeRequest,
    page_cache: &mut PageImageCache,
    evt_tx: &Sender<WorkerEvent>,
) -> NativeOcrOutcome {
    let route = current_ocr_route(request.engine, &request.options.manga_model);
    if route == OcrRoute::Backend {
        // Native selected but this op has no native path (unsupported engine,
        // torch-only model, or Suspect guard): use the backend and log once so
        // the fallback is visible without spamming per request.
        if native_ai_runtime_selected() {
            log_native_fallback_once();
        }
        return NativeOcrOutcome::NotNative;
    }

    // Decode the crop to RGBA for the native engine. A crop failure on a native
    // route is a real failure (the backend cannot fix a bad crop/source), so it is
    // surfaced when the backend is offline rather than masked as "backend offline".
    let rgba = match crop_image(request, page_cache) {
        Ok(image) => image.to_rgba8(),
        Err(err) => {
            crate::runtime_log::log_error(format!("[ocr] native OCR crop decode failed: {err}"));
            return NativeOcrOutcome::Failed(tf!("translation.ocr.native_prepare_image_error", err = err));
        }
    };

    // Adapt ORT dylib/model download progress to the OCR download-state event so
    // the UI shows activity while the runtime/model is fetched on first use.
    let mut reported = false;
    let mut progress = |snapshot: OrtDownloadProgress| {
        if !reported
            && matches!(
                snapshot.stage,
                OrtDownloadStage::Downloading
                    | OrtDownloadStage::Verifying
                    | OrtDownloadStage::Extracting
            )
        {
            reported = true;
            let _ = evt_tx.send(WorkerEvent::ModelDownloadStarted);
        }
    };

    // Run the native op. PaddleOCR returns lines already; join them so the shared
    // `assemble_native_ocr_result` applies the same join/reflect logic as MangaOCR.
    let native_result: Result<String, native_runtime::NativeRuntimeError> = match route {
        OcrRoute::NativeManga(variant) => {
            native_runtime::recognize_manga(variant, &rgba, &mut progress)
        }
        OcrRoute::NativePaddle => {
            native_runtime::recognize_paddle(&request.options.paddle_lang, &rgba, &mut progress)
                .map(|lines| lines.join("\n"))
        }
        // `Backend` is handled above; unreachable here but matched exhaustively.
        OcrRoute::Backend => return NativeOcrOutcome::NotNative,
    };

    match native_result {
        Ok(text) => NativeOcrOutcome::Ok(assemble_native_ocr_result(
            &text,
            request.join_newlines,
            request.reflect_strings,
        )),
        Err(err) => {
            // Log with the fallback framing for diagnostics; a Suspect guard never
            // reaches here (it routes to Backend). Log the `Debug` form (a stable
            // English variant name) so logs stay grep-able across UI languages, while
            // the user-facing `Failed` message uses the localized `Display` so an
            // offline-backend user sees the real reason instead of "backend offline".
            let engine_label = native_ocr_engine_label(route);
            crate::runtime_log::log_error(format!(
                "[ocr] native {engine_label} failed (falls back to the Python backend if it is up): {err:?}"
            ));
            NativeOcrOutcome::Failed(tf!("translation.ocr.native_error", err = err))
        }
    }
}

/// Human-readable engine label for a native OCR route, for log context.
#[cfg(not(target_arch = "wasm32"))]
fn native_ocr_engine_label(route: OcrRoute) -> &'static str {
    match route {
        OcrRoute::NativeManga(_) => "MangaOCR",
        OcrRoute::NativePaddle => "PaddleOCR",
        OcrRoute::Backend => "OCR",
    }
}

/// Pure decision: given a native recognize failure, should the worker surface the
/// native error to the user, or fall back to the Python backend?
///
/// Returns `true` (surface the native error) exactly when the backend is NOT
/// available — the native path is then the only path, so masking its failure as
/// "backend offline" would hide the real cause. When the backend IS available the
/// contract is preserved: the worker falls back to the backend (`false`). Pure so
/// the dispatch decision is unit-testable without a live backend.
#[cfg(not(target_arch = "wasm32"))]
fn native_failure_should_surface(backend_available: bool) -> bool {
    !backend_available
}

/// Handles one `Recognize` command, including real cancellation: while the
/// backend call is in flight, the worker keeps draining `cmd_rx`. If a newer
/// `Recognize` arrives it cancels the in-flight request (the backend replies
/// `status:"interrupted"`), drops the superseded request silently, and starts
/// the newer one. A `Stop` cancels and breaks the worker loop.
///
/// Returns `ControlFlow::Break` only when the worker should stop (Stop received
/// or the event channel is gone).
fn run_recognize_command(
    mut request: OcrRecognizeRequest,
    page_cache: &mut PageImageCache,
    cmd_rx: &Receiver<WorkerCommand>,
    evt_tx: &Sender<WorkerEvent>,
) -> std::ops::ControlFlow<()> {
    loop {
        let request_id = request.request_id;

        // The AI-API engine runs over `genai`, not the framed socket, so it has
        // no IPC cancel; run it synchronously like before.
        if request.engine == OcrEngine::AiApi {
            let result = run_ai_api_ocr_request(&request, page_cache).map(|mut result| {
                apply_char_replacements(&mut result, &request.char_replacements);
                result
            });
            return publish_recognize(request_id, result, evt_tx);
        }

        // Native ONNX Runtime route (MangaOCR + PaddleOCR). On success we publish
        // the native result. On a native failure we preserve the fallback contract
        // ONLY when the backend is up; when the backend is offline we surface the
        // real native error instead of falling through to the misleading
        // "backend offline" message. A non-native op falls through to the backend.
        // Desktop-only: the native runtime is compiled out on the web build.
        #[cfg(not(target_arch = "wasm32"))]
        match try_native_ocr(&request, page_cache, evt_tx) {
            NativeOcrOutcome::Ok(mut result) => {
                apply_char_replacements(&mut result, &request.char_replacements);
                return publish_recognize(request_id, Ok(result), evt_tx);
            }
            NativeOcrOutcome::Failed(native_error) => {
                // Only path is native when the backend is down: surface its error.
                if native_failure_should_surface(ensure_v2_backend_ready().is_ok()) {
                    return publish_recognize(request_id, Err(native_error), evt_tx);
                }
                // Backend is up: fall through to the backend recognize below.
            }
            NativeOcrOutcome::NotNative => {
                // No native path: fall through to the backend recognize below.
            }
        }

        match run_backend_recognize(&request, page_cache, cmd_rx) {
            BackendRecognizeFlow::Outcome(RecognizeOutcome::Done(result)) => {
                let result = result.map(|mut result| {
                    apply_char_replacements(&mut result, &request.char_replacements);
                    result
                });
                return publish_recognize(request_id, result, evt_tx);
            }
            // The in-flight request was superseded but no follow-up was captured
            // (e.g. cancel raced an empty queue): drop it silently, like the old
            // server-side "latest wins" did.
            BackendRecognizeFlow::Outcome(RecognizeOutcome::Superseded) => {
                return std::ops::ControlFlow::Continue(());
            }
            // A newer selection superseded this one: drop the old request silently
            // and loop to process the newer request next.
            BackendRecognizeFlow::Superseded(next) => {
                request = *next;
            }
            BackendRecognizeFlow::Stop => return std::ops::ControlFlow::Break(()),
        }
    }
}

/// What `run_backend_recognize` decided to do next.
enum BackendRecognizeFlow {
    /// Terminal outcome for the current request (deliver it).
    Outcome(RecognizeOutcome),
    /// A newer `Recognize` arrived and cancelled this one; process `next`.
    Superseded(Box<OcrRecognizeRequest>),
    /// A `Stop` arrived; the worker should break.
    Stop,
}

/// Runs one backend recognize with cancellation. Begins the framed call, then
/// waits for the terminal frame on a helper thread while the calling thread
/// watches `cmd_rx` for a newer `Recognize`/`Stop`, cancelling the in-flight
/// request if one arrives.
fn run_backend_recognize(
    request: &OcrRecognizeRequest,
    page_cache: &mut PageImageCache,
    cmd_rx: &Receiver<WorkerCommand>,
) -> BackendRecognizeFlow {
    if let Err(err) = ensure_backend_ocr_models_for_request(request.engine, &request.options) {
        return BackendRecognizeFlow::Outcome(RecognizeOutcome::Done(Err(err)));
    }
    if let Err(err) = ensure_v2_backend_ready() {
        return BackendRecognizeFlow::Outcome(RecognizeOutcome::Done(Err(err)));
    }
    let method = match request.engine.backend_method() {
        Some(method) => method,
        None => {
            return BackendRecognizeFlow::Outcome(RecognizeOutcome::Done(Err(
                t!("translation.ocr.method_not_set_error").to_string(),
            )));
        }
    };
    let crop_png = match crop_image_as_png(request, page_cache) {
        Ok(png) => png,
        Err(err) => return BackendRecognizeFlow::Outcome(RecognizeOutcome::Done(Err(err))),
    };
    let header_fields = ocr_header_fields(
        &request.options,
        request.join_newlines,
        request.reflect_strings,
    );

    // Begin the call (with a bounded transport retry), obtaining a cancellable
    // handle. Keep the client + id so a superseding selection can cancel by id.
    let (client, handle) = match begin_ocr_backend_call(method, header_fields, &crop_png) {
        Ok(pair) => pair,
        Err(CallError::Interrupted(_)) => {
            return BackendRecognizeFlow::Outcome(RecognizeOutcome::Superseded);
        }
        Err(err) => {
            return BackendRecognizeFlow::Outcome(RecognizeOutcome::Done(Err(err.to_string())));
        }
    };
    let in_flight_id = handle.id();

    // Drive the blocking `wait` on a helper thread so the worker thread stays free
    // to watch `cmd_rx` for a superseding command.
    let (result_tx, result_rx) = mpsc::channel::<Result<(Value, Vec<u8>), CallError>>();
    let waiter = thread::spawn(move || {
        let _ = result_tx.send(handle.wait(OCR_BACKEND_CALL_TIMEOUT));
    });

    // Poll: whichever fires first — the terminal frame or a newer command — wins.
    // A newer `Recognize`/`Stop` cancels the in-flight id; the backend then sends
    // an `interrupted` terminal which unblocks the waiter.
    let flow = watch_for_supersede(&result_rx, cmd_rx, &client, in_flight_id);
    let _ = waiter.join();
    flow
}

/// Blocks until either the in-flight call finishes (terminal frame received on
/// `result_rx`) or a superseding `Recognize`/`Stop` arrives on `cmd_rx`. In the
/// latter case the in-flight id is cancelled and the call's terminal frame
/// (the `interrupted` reply, drained here so the waiter thread can exit) is
/// discarded. Non-superseding commands (key store/clear, metadata, redundant
/// loads) are ignored while a recognize is in flight.
fn watch_for_supersede(
    result_rx: &Receiver<Result<(Value, Vec<u8>), CallError>>,
    cmd_rx: &Receiver<WorkerCommand>,
    client: &backend_ipc::BackendClient,
    in_flight_id: u64,
) -> BackendRecognizeFlow {
    loop {
        // 1) Has the call already finished? Prefer the terminal outcome.
        match result_rx.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => return BackendRecognizeFlow::Outcome(interpret_call_result(result)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return BackendRecognizeFlow::Outcome(interpret_call_result(Err(
                    CallError::Transport(t!("translation.ocr.waiter_ended_error").to_string()),
                )));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        // 2) Otherwise, check for a superseding command.
        let superseding = match cmd_rx.try_recv() {
            Ok(WorkerCommand::Recognize(next)) => Some(WorkerCommand::Recognize(next)),
            Ok(WorkerCommand::Stop) => Some(WorkerCommand::Stop),
            // Ignore non-superseding commands mid-recognize.
            Ok(_) | Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(WorkerCommand::Stop),
        };

        if let Some(command) = superseding {
            // Cancel the in-flight request and drain its (interrupted) terminal so
            // the waiter thread can exit before we return. Bound the wait: on a
            // live-but-slow-to-cancel backend the waiter's own `wait` timeout
            // (`OCR_BACKEND_CALL_TIMEOUT`) is the worst case, so wait just past it
            // rather than blocking this worker thread forever.
            let _ = client.cancel(in_flight_id);
            let _ = result_rx.recv_timeout(OCR_SUPERSEDE_DRAIN_TIMEOUT);
            return match command {
                WorkerCommand::Recognize(next) => BackendRecognizeFlow::Superseded(Box::new(next)),
                _ => BackendRecognizeFlow::Stop,
            };
        }
    }
}

/// Maps the helper-thread's call result into a `RecognizeOutcome`. `Interrupted`
/// (a cancel outcome) is reported as `Superseded`, never as an error.
fn interpret_call_result(result: Result<(Value, Vec<u8>), CallError>) -> RecognizeOutcome {
    match result {
        Ok((header, _blob)) => RecognizeOutcome::Done(parse_ocr_response(&header)),
        Err(CallError::Interrupted(_)) => RecognizeOutcome::Superseded,
        Err(err) => RecognizeOutcome::Done(Err(err.to_string())),
    }
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
        // PaddleVl (Transformers) downloads its weights through the Hugging Face
        // hub cache on first use, like EasyOCR/Surya; no app-managed model tree.
        OcrEngine::EasyOcr | OcrEngine::PaddleVl | OcrEngine::Surya | OcrEngine::AiApi => {}
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
            .map_err(|err| tf!("translation.ocr.decode_overlay_error", err = err));
    }
    let source = page_cache.get_or_load(&request.page_path)?;
    let (img_w, img_h) = source.dimensions();
    if img_w == 0 || img_h == 0 {
        return Err(tf!("translation.ocr.empty_image_error", request = request.page_path.display()));
    }

    let [u1, v1, u2, v2] = normalized_uv(request.uv_rect);
    let x1 = ((u1 * img_w as f32).floor() as u32).min(img_w.saturating_sub(1));
    let y1 = ((v1 * img_h as f32).floor() as u32).min(img_h.saturating_sub(1));
    let x2 = ((u2 * img_w as f32).ceil() as u32).min(img_w);
    let y2 = ((v2 * img_h as f32).ceil() as u32).min(img_h);

    if x2 <= x1 || y2 <= y1 {
        return Err(t!("translation.ocr.selection_too_small_error").to_string());
    }
    Ok(source.crop_imm(x1, y1, x2 - x1, y2 - y1))
}

fn encode_png(image: DynamicImage) -> Result<Vec<u8>, String> {
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image.to_rgb8())
        .write_to(&mut cursor, ImageFormat::Png)
        .map_err(|err| tf!("translation.ocr.serialize_crop_error", err = err))?;
    Ok(cursor.into_inner())
}

fn normalized_uv(uv: [f32; 4]) -> [f32; 4] {
    let left = uv[0].min(uv[2]).clamp(0.0, 1.0);
    let right = uv[0].max(uv[2]).clamp(0.0, 1.0);
    let top = uv[1].min(uv[3]).clamp(0.0, 1.0);
    let bottom = uv[1].max(uv[3]).clamp(0.0, 1.0);
    [left, top, right, bottom]
}

fn non_zero_u32_to_option(value: u32) -> Option<u32> {
    if value == 0 { None } else { Some(value) }
}

/// Parses the v2 `status:"ok"` OCR response header into lines/text. The caller
/// has already mapped `status:"error"`/`"interrupted"` via `CallError`, so this
/// only reads the `lines`/`text` result fields (the `engine`/`model`/`device`
/// metadata fields are not needed by the UI).
fn parse_ocr_response(response: &Value) -> Result<OcrRecognizeResult, String> {
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

fn validate_ai_api_options(options: &OcrRuntimeOptions) -> Result<(), String> {
    let service = options.ai_api_service;
    if read_ai_api_key(service)?.trim().is_empty() {
        return Err(tf!("translation.ocr.api_key_missing_error", service = service.label()));
    }
    if options.ai_api_model.trim().is_empty() {
        return Err(t!("translation.ocr.no_multimodal_model_error").to_string());
    }
    Ok(())
}

/// Web stub: AI-API OCR runs over `genai` + `tokio`, which are not compiled for
/// the browser build. Returns a clear error instead of a fake recognition.
#[cfg(target_arch = "wasm32")]
fn run_ai_api_ocr_request(
    _request: &OcrRecognizeRequest,
    _page_cache: &mut PageImageCache,
) -> Result<OcrRecognizeResult, String> {
    Err(t!("translation.ocr.ai_api_web_unavailable_error").to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn run_ai_api_ocr_request(
    request: &OcrRecognizeRequest,
    page_cache: &mut PageImageCache,
) -> Result<OcrRecognizeResult, String> {
    validate_ai_api_options(&request.options)?;
    let crop_png = crop_image_as_png(request, page_cache)?;
    let service = request.options.ai_api_service;
    let api_key = read_ai_api_key(service)?;
    let model = model_iden_for_ai_api_service(service, &request.options.ai_api_model);
    let system_instruction = normalized_ai_api_system_instruction(&request.options);
    let prompt = if request.reflect_strings {
        "Recognize all visible text in this manga/comic image. Read vertical columns right-to-left when the layout indicates manga reading order. Return only the recognized text, preserving line breaks when they are meaningful."
    } else {
        "Recognize all visible text in this manga/comic image. Return only the recognized text, preserving line breaks when they are meaningful."
    };
    let image_b64 = base64_encode(&crop_png);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| tf!("translation.ocr.async_runtime_error", err = err))?;

    let text = runtime.block_on(async move {
        let client = build_ai_api_client(service, api_key);
        let chat_req = ChatRequest::default()
            .with_system(system_instruction)
            .append_message(ChatMessage::user(vec![
                ContentPart::from_text(prompt),
                ContentPart::from_binary_base64(
                    "image/png",
                    image_b64,
                    Some("ocr-crop.png".to_string()),
                ),
            ]));
        let chat_res = client
            .exec_chat(model, chat_req, None)
            .await
            .map_err(|err| tf!("translation.ocr.ai_api_request_failed_error", err = err))?;
        Ok::<String, String>(chat_res.first_text().unwrap_or("").trim().to_string())
    })?;

    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();

    Ok(OcrRecognizeResult { lines, text })
}

fn normalized_ai_api_system_instruction(options: &OcrRuntimeOptions) -> String {
    let trimmed = options.ai_api_system_instruction.trim();
    if trimmed.is_empty() {
        "You are an OCR engine for manga and comics. Recognize text exactly as it is written, primarily in the following language: Korean. Pay special attention to the sounds. Do not translate, explain, describe the image, or add captions. Return only the recognized text. If a sound is particularly unclear and you are unsure, list several possible options separated by /".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn build_ai_api_client(service: AiApiService, api_key: String) -> Client {
    let api_key = Arc::new(api_key);
    let expected_adapter = service.adapter_kind();
    let auth_resolver = AuthResolver::from_resolver_fn(
        move |model_iden: ModelIden| -> Result<Option<AuthData>, genai::resolver::Error> {
            if model_iden.adapter_kind == expected_adapter {
                Ok(Some(AuthData::from_single((*api_key).clone())))
            } else {
                Ok(None)
            }
        },
    );
    Client::builder().with_auth_resolver(auth_resolver).build()
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn model_iden_for_ai_api_service(service: AiApiService, model: &str) -> ModelIden {
    let model_name = model
        .trim()
        .split_once("::")
        .map_or_else(|| model.trim(), |(_, name)| name.trim());
    ModelIden::new(service.adapter_kind(), model_name)
}

/// Web stub: model listing / account status need `genai`/`tokio`/`ureq`, absent
/// on the browser build. Surfaces a clear error rather than empty metadata.
#[cfg(target_arch = "wasm32")]
pub(crate) fn load_ai_api_metadata(_service: AiApiService) -> Result<AiApiMetadata, String> {
    Err(t!("translation.ocr.ai_api_data_web_unavailable_error").to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn load_ai_api_metadata(service: AiApiService) -> Result<AiApiMetadata, String> {
    let key = read_ai_api_key(service).unwrap_or_default();
    let key_configured = !key.trim().is_empty();
    let mut account_status = if key_configured {
        t!("translation.ocr.balance_unavailable_status").to_string()
    } else {
        t!("translation.common.api_key_not_set_status").to_string()
    };
    let mut models = Vec::new();

    if key_configured {
        models = fetch_ai_api_model_names(service, &key)?;
        if service == AiApiService::OpenRouter {
            account_status = fetch_openrouter_account_status(&key)
                .unwrap_or_else(|err| tf!("translation.ocr.openrouter_balance_error", err = err));
        }
    }

    Ok(AiApiMetadata {
        service,
        key_configured,
        models,
        account_status,
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_ai_api_model_names(service: AiApiService, api_key: &str) -> Result<Vec<String>, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| tf!("translation.ocr.models_async_runtime_error", err = err))?;
    let adapter = service.adapter_kind();
    let key = api_key.to_string();
    let models = runtime.block_on(async move {
        let client = Client::default();
        client
            .all_model_names(
                adapter,
                ProviderConfig::from_auth(AuthData::from_single(key)),
            )
            .await
            .map_err(|err| {
                tf!("translation.ocr.fetch_models_error", service = service.label(), err = err)
            })
    })?;
    let mut filtered = models
        .into_iter()
        .filter(|model| is_likely_multimodal_model(model))
        .map(|model| model_name_for_ai_api_ui(service, &model))
        .collect::<Vec<_>>();
    filtered.sort();
    filtered.dedup();
    if filtered.is_empty() {
        filtered.push(service.default_model().to_string());
    }
    Ok(filtered)
}

#[cfg(not(target_arch = "wasm32"))]
fn model_name_for_ai_api_ui(service: AiApiService, model: &str) -> String {
    if service == AiApiService::OpenRouter && !model.contains("::") {
        format!("open_router::{model}")
    } else if service == AiApiService::Groq && !model.contains("::") {
        format!("groq::{model}")
    } else {
        model.to_string()
    }
}

pub(crate) fn is_likely_multimodal_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("vision")
        || model.contains("vl")
        || model.contains("omni")
        || model.contains("multimodal")
        || model.contains("gpt-4o")
        || model.contains("gpt-4.1")
        || model.contains("gpt-5")
        || model.contains("claude-3")
        || model.contains("claude-4")
        || model.contains("gemini")
        || model.contains("grok-2")
        || model.contains("grok-3")
        || model.contains("llama-4")
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_openrouter_account_status(api_key: &str) -> Result<String, String> {
    let response = ureq::get("https://openrouter.ai/api/v1/key")
        .set("Authorization", &format!("Bearer {api_key}"))
        .call()
        .map_err(|err| tf!("translation.ocr.openrouter_key_request_error", err = err))?;
    let value: Value = response
        .into_json()
        .map_err(|err| tf!("translation.ocr.openrouter_non_json_error", err = err))?;
    let data = value.get("data").unwrap_or(&value);
    let usage = data.get("usage").and_then(Value::as_f64);
    let limit = data.get("limit").and_then(Value::as_f64);
    let remaining = data.get("limit_remaining").and_then(Value::as_f64);
    let rate = data.get("rate_limit").and_then(Value::as_object);

    let mut parts = Vec::new();
    if let Some(usage) = usage {
        parts.push(tf!(
            "translation.ocr.openrouter_usage",
            usage = format!("{usage:.2}")
        ));
    }
    match (limit, remaining) {
        (Some(limit), Some(remaining)) => {
            parts.push(tf!(
                "translation.ocr.openrouter_limit_remaining",
                limit = format!("{limit:.2}"),
                remaining = format!("{remaining:.2}")
            ));
        }
        (None, Some(remaining)) => {
            parts.push(tf!(
                "translation.ocr.openrouter_remaining",
                remaining = format!("{remaining:.2}")
            ));
        }
        (None, None) => {
            parts.push(t!("translation.ocr.limit_not_set_status").to_string());
        }
        (Some(limit), None) => {
            parts.push(tf!(
                "translation.ocr.openrouter_limit",
                limit = format!("{limit:.2}")
            ));
        }
    }
    if let Some(rate) = rate {
        let requests = rate.get("requests").and_then(Value::as_u64);
        let interval = rate.get("interval").and_then(Value::as_str);
        if let (Some(requests), Some(interval)) = (requests, interval) {
            parts.push(format!("{requests} req/{interval}"));
        }
    }
    Ok(format!("OpenRouter: {}", parts.join(", ")))
}

/// Web stub: the OS credential store (`keyring`) does not exist in the browser,
/// so storing an API key is rejected with a clear error.
#[cfg(target_arch = "wasm32")]
pub(crate) fn store_ai_api_key(_service: AiApiService, _api_key: &str) -> Result<(), String> {
    Err(t!("translation.ocr.keystore_web_unavailable_error").to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn store_ai_api_key(service: AiApiService, api_key: &str) -> Result<(), String> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err(t!("translation.ocr.api_key_empty_error").to_string());
    }
    ai_api_keyring_entry(service)?
        .set_password(trimmed)
        .map_err(|err| tf!("translation.ocr.store_api_key_error", service = service.label(), err = err))
}

/// Web stub: no OS credential store on the browser build, so clearing a key is
/// rejected with a clear error.
#[cfg(target_arch = "wasm32")]
pub(crate) fn clear_ai_api_key(_service: AiApiService) -> Result<(), String> {
    Err(t!("translation.ocr.keystore_web_unavailable_error").to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn clear_ai_api_key(service: AiApiService) -> Result<(), String> {
    match ai_api_keyring_entry(service)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(tf!("translation.ocr.delete_api_key_error", service = service.label(), err = err)),
    }
}

/// Web stub: no OS credential store on the browser build. Returns a clear error
/// so callers (OCR warmup, MT run, metadata) surface "unavailable on web" rather
/// than treating a missing key as an empty one.
#[cfg(target_arch = "wasm32")]
pub(crate) fn read_ai_api_key(_service: AiApiService) -> Result<String, String> {
    Err(t!("translation.ocr.keystore_web_unavailable_error").to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn read_ai_api_key(service: AiApiService) -> Result<String, String> {
    match ai_api_keyring_entry(service)?.get_password() {
        Ok(key) => Ok(key),
        Err(keyring::Error::NoEntry) => Ok(String::new()),
        Err(err) => Err(tf!("translation.ocr.read_api_key_error", service = service.label(), err = err)),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn ai_api_keyring_entry(service: AiApiService) -> Result<keyring::Entry, String> {
    keyring::Entry::new("ManhwaStudio AI API OCR", service.key())
        .map_err(|err| tf!("translation.ocr.keyring_unavailable_error", err = err))
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
                .ok_or_else(|| t!("translation.ocr.cache_get_error").to_string());
        }

        let image = image::open(path).map_err(|err| {
            tf!("translation.ocr.open_image_error", path = path.display(), err = err)
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
            .ok_or_else(|| t!("translation.ocr.cache_store_error").to_string())
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

#[cfg(test)]
mod tests {
    use super::{
        AiApiService, CallError, CharReplacementRule, OcrEngine, OcrRecognizeResult,
        OcrRoute, OcrRuntimeOptions, RecognizeOutcome, apply_char_replacements,
        assemble_native_ocr_result, interpret_call_result, is_likely_multimodal_model,
        model_iden_for_ai_api_service, native_failure_should_surface, ocr_header_fields,
        ocr_requires_backend, ocr_route, ocr_route_needs_backend_warmup, parse_ocr_response,
    };
    use crate::ai_models::MangaOcrOnnxModel;
    use crate::backend_ipc::protocol;
    use crate::config::{AiRuntime, OrtLoadDecision};
    use genai::adapter::AdapterKind;
    use serde_json::{Value, json};

    fn sample_options() -> OcrRuntimeOptions {
        OcrRuntimeOptions {
            manga_model: "base".to_string(),
            paddle_lang: "korean_v5".to_string(),
            paddle_vl_script: "korean".to_string(),
            easy_langs: "ko".to_string(),
            surya_task_name: "ocr_without_boxes".to_string(),
            surya_recognize_math: false,
            surya_sort_lines: true,
            surya_drop_repeated_text: false,
            surya_max_sliding_window: 0,
            surya_max_tokens: 128,
            ai_api_service: AiApiService::OpenAi,
            ai_api_model: "gpt-4o-mini".to_string(),
            ai_api_system_instruction: String::new(),
        }
    }

    #[test]
    fn backend_method_maps_each_engine_to_protocol_const() {
        assert_eq!(
            OcrEngine::MangaOcr.backend_method(),
            Some(protocol::METHOD_OCR_MANGA)
        );
        assert_eq!(
            OcrEngine::EasyOcr.backend_method(),
            Some(protocol::METHOD_OCR_EASY)
        );
        assert_eq!(
            OcrEngine::PaddleOcr.backend_method(),
            Some(protocol::METHOD_OCR_PADDLE)
        );
        assert_eq!(
            OcrEngine::PaddleVl.backend_method(),
            Some(protocol::METHOD_OCR_PADDLE_VL)
        );
        assert_eq!(
            OcrEngine::Surya.backend_method(),
            Some(protocol::METHOD_OCR_SURYA)
        );
        assert_eq!(OcrEngine::AiApi.backend_method(), None);
        assert!(OcrEngine::Surya.requires_backend());
        assert!(!OcrEngine::AiApi.requires_backend());
    }

    #[test]
    fn ocr_header_fields_carry_params_not_image() {
        let header = ocr_header_fields(&sample_options(), false, true);
        // Engine params live inline in the header (the image is NOT here — it
        // travels as the request blob).
        assert_eq!(header["join_newlines"], json!(false));
        assert_eq!(header["reflect_strings"], json!(true));
        assert_eq!(header["manga_model"], json!("base"));
        assert_eq!(header["paddle_lang"], json!("korean_v5"));
        assert_eq!(header["paddle_vl_script"], json!("korean"));
        assert_eq!(header["easy_langs"], json!("ko"));
        assert_eq!(header["surya_task_name"], json!("ocr_without_boxes"));
        assert_eq!(header["surya_sort_lines"], json!(true));
        // Zero -> null (omitted), positive -> the value.
        assert_eq!(header["surya_max_sliding_window"], Value::Null);
        assert_eq!(header["surya_max_tokens"], json!(128));
        // No base64 image field is ever placed in the header.
        assert!(header.get("image_base64").is_none());
        assert!(header.get("image").is_none());
    }

    #[test]
    fn request_header_uses_engine_method_and_keeps_params() {
        // Mirror what begin_call/request_header produce on the wire: method +
        // reserved fields plus the inline engine params.
        let header_fields = ocr_header_fields(&sample_options(), true, false);
        let wire = protocol::request_header(
            7,
            OcrEngine::EasyOcr.backend_method().unwrap(),
            &header_fields,
        );
        assert_eq!(
            wire[protocol::HEADER_METHOD],
            json!(protocol::METHOD_OCR_EASY)
        );
        assert_eq!(wire[protocol::HEADER_KIND], json!(protocol::KIND_REQUEST));
        assert_eq!(wire["easy_langs"], json!("ko"));
        assert_eq!(wire["join_newlines"], json!(true));
    }

    #[test]
    fn parses_lines_and_text_from_ok_response_header() {
        let header = json!({
            "engine": "suryaocr",
            "lines": ["one", "two"],
            "text": "one\ntwo"
        });
        let result = parse_ocr_response(&header).expect("parse ok");
        assert_eq!(result.lines, vec!["one".to_string(), "two".to_string()]);
        assert_eq!(result.text, "one\ntwo");
    }

    #[test]
    fn parse_response_backfills_missing_lines_from_text() {
        let header = json!({ "engine": "mangaocr", "text": "alpha\n\nbeta " });
        let result = parse_ocr_response(&header).expect("parse ok");
        assert_eq!(result.lines, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(result.text, "alpha\n\nbeta");
    }

    #[test]
    fn parse_response_handles_paddle_onnx_extra_fields() {
        // paddle_onnx adds model/device metadata; lines/text still parse and the
        // extra fields are simply ignored by the UI-facing result.
        let header = json!({
            "engine": "paddleocr_onnx",
            "model": "korean_v5",
            "device": "cpu",
            "lines": ["x"],
            "text": "x"
        });
        let result = parse_ocr_response(&header).expect("parse ok");
        assert_eq!(result.text, "x");
    }

    #[test]
    fn interrupted_call_maps_to_superseded_not_error() {
        // A cancel outcome (`status:"interrupted"`) must be treated as "superseded
        // by a newer selection", never surfaced as a recognize error.
        let outcome = interpret_call_result(Err(CallError::Interrupted("cancelled".to_string())));
        assert!(matches!(outcome, RecognizeOutcome::Superseded));
    }

    #[test]
    fn backend_error_maps_to_failed_recognize() {
        let outcome = interpret_call_result(Err(CallError::Error("boom".to_string())));
        match outcome {
            RecognizeOutcome::Done(Err(msg)) => assert!(msg.contains("boom")),
            other => panic!(
                "expected Done(Err), got a different outcome (superseded={})",
                matches!(other, RecognizeOutcome::Superseded)
            ),
        }
    }

    #[test]
    fn transport_error_maps_to_failed_recognize() {
        let outcome = interpret_call_result(Err(CallError::Transport("eof".to_string())));
        assert!(matches!(outcome, RecognizeOutcome::Done(Err(_))));
    }

    #[test]
    fn ok_call_parses_into_done_result() {
        let header = json!({ "engine": "mangaocr", "lines": ["hi"], "text": "hi" });
        let outcome = interpret_call_result(Ok((header, Vec::new())));
        match outcome {
            RecognizeOutcome::Done(Ok(result)) => assert_eq!(result.text, "hi"),
            _ => panic!("expected Done(Ok(..))"),
        }
    }

    #[test]
    fn applies_char_replacements_to_text_and_lines() {
        let mut result = OcrRecognizeResult {
            lines: vec!["a·b".to_string(), "c…".to_string()],
            text: "a·b\nc…".to_string(),
        };
        let rules = vec![
            CharReplacementRule {
                targets: vec!["·".to_string(), "・".to_string()],
                replacement: ".".to_string(),
            },
            CharReplacementRule {
                targets: vec!["…".to_string()],
                replacement: "...".to_string(),
            },
        ];
        apply_char_replacements(&mut result, &rules);
        assert_eq!(result.lines, vec!["a.b".to_string(), "c...".to_string()]);
        assert_eq!(result.text, "a.b\nc...");
    }

    #[test]
    fn empty_char_replacements_leave_result_unchanged() {
        let mut result = OcrRecognizeResult {
            lines: vec!["a·b".to_string()],
            text: "a·b".to_string(),
        };
        apply_char_replacements(&mut result, &[]);
        assert_eq!(result.lines, vec!["a·b".to_string()]);
        assert_eq!(result.text, "a·b");
    }

    #[test]
    fn parses_ai_api_service_keys() {
        assert_eq!(
            AiApiService::from_key("open_router"),
            AiApiService::OpenRouter
        );
        assert_eq!(AiApiService::from_key("grok"), AiApiService::Xai);
        assert_eq!(AiApiService::from_key("unknown"), AiApiService::OpenAi);
    }

    #[test]
    fn strips_ui_namespace_for_explicit_model_identity() {
        let model = model_iden_for_ai_api_service(
            AiApiService::OpenRouter,
            "open_router::google/gemini-2.0-flash-001",
        );
        assert_eq!(model.adapter_kind, AdapterKind::OpenRouter);
        assert_eq!(model.model_name.to_string(), "google/gemini-2.0-flash-001");
    }

    #[test]
    fn detects_common_multimodal_model_names() {
        assert!(is_likely_multimodal_model("gpt-4o-mini"));
        assert!(is_likely_multimodal_model("claude-3-5-haiku-latest"));
        assert!(is_likely_multimodal_model("google/gemini-2.0-flash-001"));
        assert!(!is_likely_multimodal_model("text-embedding-3-small"));
    }

    #[test]
    fn ocr_route_native_manga_for_manga_onnx_safe_guard() {
        // Native runtime + MangaOCR + ONNX variant + Safe guard -> NativeManga.
        assert_eq!(
            ocr_route(
                AiRuntime::Native,
                OcrEngine::MangaOcr,
                "base_onnx",
                OrtLoadDecision::Safe
            ),
            OcrRoute::NativeManga(MangaOcrOnnxModel::Base)
        );
        assert_eq!(
            ocr_route(
                AiRuntime::Native,
                OcrEngine::MangaOcr,
                "2025_onnx",
                OrtLoadDecision::Safe
            ),
            OcrRoute::NativeManga(MangaOcrOnnxModel::Model2025)
        );
    }

    #[test]
    fn ocr_route_native_paddle_for_paddle_engine_safe_guard() {
        // Native runtime + PaddleOCR + Safe guard -> NativePaddle, for any language.
        // The manga_model_key is irrelevant for the Paddle route.
        assert_eq!(
            ocr_route(
                AiRuntime::Native,
                OcrEngine::PaddleOcr,
                "base_onnx",
                OrtLoadDecision::Safe
            ),
            OcrRoute::NativePaddle
        );
        assert_eq!(
            ocr_route(
                AiRuntime::Native,
                OcrEngine::PaddleOcr,
                "",
                OrtLoadDecision::Safe
            ),
            OcrRoute::NativePaddle
        );
    }

    #[test]
    fn ocr_route_backend_when_paddle_guard_suspect() {
        // A Suspect guard disables the native Paddle path.
        assert_eq!(
            ocr_route(
                AiRuntime::Native,
                OcrEngine::PaddleOcr,
                "korean_v5",
                OrtLoadDecision::Suspect
            ),
            OcrRoute::Backend
        );
    }

    #[test]
    fn ocr_route_backend_when_model_has_no_native_path() {
        // Torch-only MangaOCR model -> backend even with native runtime + Safe guard.
        assert_eq!(
            ocr_route(
                AiRuntime::Native,
                OcrEngine::MangaOcr,
                "base_torch",
                OrtLoadDecision::Safe
            ),
            OcrRoute::Backend
        );
        // Unknown model key -> backend.
        assert_eq!(
            ocr_route(
                AiRuntime::Native,
                OcrEngine::MangaOcr,
                "definitely-not-a-model",
                OrtLoadDecision::Safe
            ),
            OcrRoute::Backend
        );
    }

    #[test]
    fn ocr_route_backend_when_guard_suspect() {
        // A Suspect guard disables the native path even for an ONNX MangaOCR model.
        assert_eq!(
            ocr_route(
                AiRuntime::Native,
                OcrEngine::MangaOcr,
                "base_onnx",
                OrtLoadDecision::Suspect
            ),
            OcrRoute::Backend
        );
    }

    #[test]
    fn ocr_route_backend_for_engines_without_native_path() {
        // Native runtime + Safe guard, but these engines have no native path
        // (MangaOCR and PaddleOCR are the natively-supported OCR engines).
        for engine in [
            OcrEngine::EasyOcr,
            OcrEngine::PaddleVl,
            OcrEngine::Surya,
            OcrEngine::AiApi,
        ] {
            assert_eq!(
                ocr_route(AiRuntime::Native, engine, "base_onnx", OrtLoadDecision::Safe),
                OcrRoute::Backend,
                "engine {engine:?} must route to backend"
            );
        }
    }

    #[test]
    fn ocr_route_backend_when_runtime_is_backend() {
        // Backend runtime always routes to the backend regardless of engine/model/guard.
        for guard in [OrtLoadDecision::Safe, OrtLoadDecision::Suspect] {
            assert_eq!(
                ocr_route(AiRuntime::Backend, OcrEngine::MangaOcr, "base_onnx", guard),
                OcrRoute::Backend
            );
            assert_eq!(
                ocr_route(AiRuntime::Backend, OcrEngine::MangaOcr, "2025_onnx", guard),
                OcrRoute::Backend
            );
        }
    }

    #[test]
    fn native_routes_skip_backend_warmup_backend_route_needs_it() {
        // The native routes lazy-load in-process on first recognize, so warmup must
        // NOT require the Python backend for them (this is what makes native OCR work
        // offline). Only the Backend route warms the backend.
        assert!(!ocr_route_needs_backend_warmup(OcrRoute::NativeManga(
            MangaOcrOnnxModel::Base
        )));
        assert!(!ocr_route_needs_backend_warmup(OcrRoute::NativeManga(
            MangaOcrOnnxModel::Model2025
        )));
        assert!(!ocr_route_needs_backend_warmup(OcrRoute::NativePaddle));
        assert!(ocr_route_needs_backend_warmup(OcrRoute::Backend));
    }

    #[test]
    fn ocr_requires_backend_backend_runtime_matches_engine_contract() {
        // With the (default) backend runtime, the decision is byte-identical to the
        // historical `engine.requires_backend()`: every backend engine requires it,
        // AiApi never does — for either guard state.
        for guard in [OrtLoadDecision::Safe, OrtLoadDecision::Suspect] {
            for engine in [
                OcrEngine::MangaOcr,
                OcrEngine::EasyOcr,
                OcrEngine::PaddleOcr,
                OcrEngine::PaddleVl,
                OcrEngine::Surya,
                OcrEngine::AiApi,
            ] {
                assert_eq!(
                    ocr_requires_backend(engine, "base_onnx", AiRuntime::Backend, guard),
                    engine.requires_backend(),
                    "engine {engine:?} guard {guard:?}"
                );
            }
        }
    }

    #[test]
    fn ocr_requires_backend_native_manga_onnx_safe_guard_is_false() {
        // Native runtime + MangaOCR ONNX export + Safe guard -> in-process, no backend.
        for model in ["base_onnx", "2025_onnx"] {
            assert!(!ocr_requires_backend(
                OcrEngine::MangaOcr,
                model,
                AiRuntime::Native,
                OrtLoadDecision::Safe
            ));
        }
    }

    #[test]
    fn ocr_requires_backend_native_paddle_safe_guard_is_false() {
        assert!(!ocr_requires_backend(
            OcrEngine::PaddleOcr,
            "base_onnx",
            AiRuntime::Native,
            OrtLoadDecision::Safe
        ));
    }

    #[test]
    fn ocr_requires_backend_native_torch_model_is_true() {
        // `base_torch` has no native export, so even under the native runtime the op
        // routes to the backend and therefore still requires it.
        assert!(ocr_requires_backend(
            OcrEngine::MangaOcr,
            "base_torch",
            AiRuntime::Native,
            OrtLoadDecision::Safe
        ));
    }

    #[test]
    fn ocr_requires_backend_native_guard_suspect_is_true() {
        // A Suspect guard disables the native path -> the op falls back to the backend,
        // so the backend IS required (otherwise the user would be stuck with no path).
        assert!(ocr_requires_backend(
            OcrEngine::MangaOcr,
            "base_onnx",
            AiRuntime::Native,
            OrtLoadDecision::Suspect
        ));
        assert!(ocr_requires_backend(
            OcrEngine::PaddleOcr,
            "base_onnx",
            AiRuntime::Native,
            OrtLoadDecision::Suspect
        ));
    }

    #[test]
    fn ocr_requires_backend_native_engines_without_native_path_are_true() {
        // EasyOCR/PaddleVL/Surya have no native path; they require the backend even
        // under the native runtime with a Safe guard.
        for engine in [OcrEngine::EasyOcr, OcrEngine::PaddleVl, OcrEngine::Surya] {
            assert!(
                ocr_requires_backend(engine, "base_onnx", AiRuntime::Native, OrtLoadDecision::Safe),
                "engine {engine:?}"
            );
        }
    }

    #[test]
    fn ocr_requires_backend_ai_api_never_requires_backend() {
        // AiApi runs over `genai`; it never requires the Python backend, on any runtime.
        for runtime in [AiRuntime::Backend, AiRuntime::Native] {
            assert!(!ocr_requires_backend(
                OcrEngine::AiApi,
                "base_onnx",
                runtime,
                OrtLoadDecision::Safe
            ));
        }
    }

    #[test]
    fn native_failure_surfaces_only_when_backend_offline() {
        // Native failed + backend up  -> fall back to backend (do not surface).
        assert!(!native_failure_should_surface(true));
        // Native failed + backend down -> surface the real native error to the user.
        assert!(native_failure_should_surface(false));
    }

    #[test]
    fn assemble_native_result_single_line_is_identity() {
        // A one-line MangaOCR string: lines=[text], text=trimmed, join/reflect no-op.
        for join in [true, false] {
            for reflect in [true, false] {
                let result = assemble_native_ocr_result("  こんにちは  ", join, reflect);
                assert_eq!(result.lines, vec!["こんにちは".to_string()]);
                assert_eq!(result.text, "こんにちは");
            }
        }
    }

    #[test]
    fn assemble_native_result_splits_and_joins_lines() {
        // Blank lines are stripped; join controls the separator.
        let joined = assemble_native_ocr_result("a\n\nb\n", true, false);
        assert_eq!(joined.lines, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(joined.text, "a\nb");

        let spaced = assemble_native_ocr_result("a\n\nb\n", false, false);
        assert_eq!(spaced.lines, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(spaced.text, "a b");
    }

    #[test]
    fn assemble_native_result_reflect_reverses_line_order() {
        let result = assemble_native_ocr_result("first\nsecond\nthird", true, true);
        assert_eq!(
            result.lines,
            vec!["third".to_string(), "second".to_string(), "first".to_string()]
        );
        assert_eq!(result.text, "third\nsecond\nfirst");
    }
}
