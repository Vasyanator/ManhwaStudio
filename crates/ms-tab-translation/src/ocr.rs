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
// - AI API OCR builds `genai` multimodal chat calls (through `ms_ai_api`: target with the
//   base URL of a compatible service, client, credential-store API keys (optional for a
//   compatible service), async bridge) from the worker thread and runs them through the
//   validated streaming executor `ms_ai_api::exec_chat_validated`: the controller's
//   `GenerationTracker` shows the live phase / received characters in the panel, and
//   "Stop" (`cancel_generation`) or dropping the controller aborts the HTTP request; a
//   stopped request is published as `RecognizeCancelled`, never as an error. The answer is
//   cleaned (`<think>` blocks, a code fence wrapping the whole answer) before line
//   splitting; an answer left empty by that is asked for ONCE more (`ai_api_ocr_validation`).
//   Line split + "keep line breaks" join: the shared `assemble_line_ocr_result`. The default
//   system instruction has one owner (`default_ai_api_ocr_instruction`, worded for the OCR
//   input language); an untouched stored default follows the language
//   (`effective_ai_api_ocr_instruction`), a user edit is sent as typed.
// - Post-OCR processing (`apply_post_ocr_processing`) runs in the worker before
//   the result is published, so every engine path and the stored last result
//   share the same text: character substitution (`CharReplacementRule`) first,
//   then the optional ALL-CAPS -> sentence-case fix (`ocr_case_fix`).
// - Вспомогательные функции: crop по UV, PNG-кодирование, сборка/разбор
//   framed-заголовков `BackendClient`/`CallHandle` и JSON.
// - External models (Baberu OCR, PaddleOCR-VL variants, the EasyOCR model of the selected
//   languages' script, the two Surya checkpoints): `external_model_specs` names the pinned
//   spec bundle an engine needs; `gate_external_models` is the ONE pre-dispatch gate
//   (load warmup and every non-AI-API recognize) that turns "not downloaded" into a
//   localized error naming the panel's download button (an EasyOCR language list easyocr
//   cannot read into its own localized error first), and reports the missing bundle
//   (`OcrControllerEvent::ExternalModelMissing`) so the tab re-probes the panel's state and
//   the button the error names is the one drawn. The resolved directories travel in the
//   request header (`ocr_header_fields`, protocol v12 keys). EasyOCR languages are sent
//   canonical (`ms_sysprobe::ai_models::easyocr::normalize_easyocr_langs`, the one owner of
//   the code table and the script choice). Nothing here downloads them; the
//   download controller is `ocr_model_download.rs`. Baberu routes like MangaOCR/PaddleOCR:
//   native ONNX (vision on the selected EP, decoders on CPU) under the native runtime + Safe
//   guard, else backend `ocr.baberu`.
// - Route availability (UI gate): `ocr_route_kind` maps a selection onto
//   `local_ai::AiRouteKind` (AI API = cloud, native ONNX routes, backend routes) and
//   `ocr_route_availability` checks it against the process's `LocalAiAvailability`
//   (`--no-ai` / platform) through the one rule `AiRouteKind::availability`. The worker reads
//   the runtime through `local_ai::effective_ai_runtime` (`read_ocr_route_inputs`).
// ============================================================================
use ms_backend_ipc::{self as backend_ipc, CallError, CallHandle};
use ms_local_ai::backend_health::ai_backend_offline_error;
#[cfg(not(target_arch = "wasm32"))]
use ms_local_ai::local_ai::effective_ai_runtime;
use ms_local_ai::local_ai::{AiRouteKind, LocalAiAvailability, LocalAiOff};
use crate::ocr_case_fix;
use ms_config as config;
use ms_sysprobe::ai_models;
use ms_sysprobe::ai_models::{EnsureCtx, ModelDownloadEvent, ModelEnsureError};
use ms_sysprobe::ai_models::easyocr::{EasyOcrLangError, EasyOcrScript, easyocr_script_for, easyocr_spec, normalize_easyocr_langs};
use ms_sysprobe::ai_models::external::{ExternalModelSpec, ExternalModelStatus, external_model_status};
use ms_sysprobe::ai_models::external_catalog::{
    BABERU_OCR, BaberuFiles, PaddleVlVariant, SURYA_DETECTION_2025_05_07, SURYA_RECOGNITION_2025_09_23, baberu_files,
};
// Native ONNX Runtime OCR path (Phase 1: MangaOCR only). Desktop-only: the native
// runtime + ORT loader depend on `ms-onnx`/`ort`, which are not part of the web build.
#[cfg(not(target_arch = "wasm32"))]
use ms_native_runtime as native_runtime;
#[cfg(not(target_arch = "wasm32"))]
use ms_onnx_runtime::OrtDownloadStage;
// AI API OCR (`genai`, reached through `ms_ai_api`) is native-only: `genai` is not
// compiled for wasm. The worker command/event enums and the controller stay
// target-neutral; only the bodies that build `genai` requests are gated below.
use ms_ai_api::{AiApiService, AiApiTarget, GenerationSnapshot, GenerationTracker};
#[cfg(not(target_arch = "wasm32"))]
use ms_ai_api::{AiApiError, RepairRequest, RetryReason, TrackedAnswer, Validation};
#[cfg(not(target_arch = "wasm32"))]
use ms_ai_api::structured::{clean_plain_text, strip_think_blocks};
#[cfg(not(target_arch = "wasm32"))]
use ms_ai_api::genai::chat::{ChatMessage, ChatRequest, ContentPart};
use image::{DynamicImage, GenericImageView, ImageFormat};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Baberu OCR: native ONNX (vision on the selected EP, decoders on CPU) with a backend
    /// `ocr.baberu` fallback; ja/zh/en only.
    Baberu,
}

impl OcrEngine {
    /// The v2 framed-protocol method name for this engine, or `None` for the
    /// AI-API engine (which runs over `genai`, not the backend socket).
    fn backend_method(self) -> Option<&'static str> {
        use ms_backend_ipc::protocol;
        match self {
            OcrEngine::MangaOcr => Some(protocol::METHOD_OCR_MANGA),
            OcrEngine::EasyOcr => Some(protocol::METHOD_OCR_EASY),
            OcrEngine::PaddleOcr => Some(protocol::METHOD_OCR_PADDLE),
            OcrEngine::PaddleVl => Some(protocol::METHOD_OCR_PADDLE_VL),
            OcrEngine::Surya => Some(protocol::METHOD_OCR_SURYA),
            OcrEngine::Baberu => Some(protocol::METHOD_OCR_BABERU),
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
    /// Selected PaddleOCR-VL weights (`paddle_vl_model` on the wire, `key()` form).
    pub paddle_vl_model: PaddleVlVariant,
    pub easy_langs: String,
    pub surya_task_name: String,
    pub surya_recognize_math: bool,
    pub surya_sort_lines: bool,
    pub surya_drop_repeated_text: bool,
    pub surya_max_sliding_window: u32,
    pub surya_max_tokens: u32,
    pub ai_api_service: AiApiService,
    /// Server address of a compatible AI API service, as typed (validated per request).
    pub ai_api_base_url: String,
    pub ai_api_model: String,
    pub ai_api_system_instruction: String,
    /// OCR input language for the AI API default instruction (the MT source-language code;
    /// `auto` names none), see `effective_ai_api_ocr_instruction`.
    pub ai_api_source_lang: String,
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

/// Applies every post-OCR transformation the request asks for, in the one order
/// all engines must share.
///
/// The caps-lock DECISION and the caps-lock REWRITE straddle the substitutions on
/// purpose. Every recognize route (AI API, native ONNX, backend IPC) must call this
/// helper — engine parity of post-processing is a module contract.
fn apply_post_ocr_processing(result: &mut OcrRecognizeResult, request: &OcrRecognizeRequest) {
    // Decide on the RAW engine output, before any substitution. A rule that repairs
    // a misread character with a lowercase letter (`"0" -> "o"`, a common fix) would
    // otherwise make an all-caps result look mixed-case and silently veto the fix,
    // publishing "HELLo WORLD" instead of "Hello world".
    let fix_caps_lock = request.fix_caps_lock && ocr_case_fix::looks_like_caps_lock(result);
    // Substitutions run next, so the rewrite below sees the FINAL punctuation: a
    // rule may turn an engine artifact into a real sentence terminator (the default
    // `…` -> `...` rule does exactly that) and thereby open a new sentence.
    apply_char_replacements(result, &request.char_replacements);
    if fix_caps_lock {
        ocr_case_fix::apply_caps_lock_fix(result);
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
    /// When `true`, a recognized result that is entirely uppercase Latin/Cyrillic
    /// text is lowered to sentence case (see `ocr_case_fix`): only the first letter
    /// of the text and the first letter after `.`/`!`/`?`/`…` stay uppercase, with
    /// the sole exception of the English pronoun `I` and its contractions
    /// (`I'm`, `I'll`, `I've`, `I'd`), which stay capitalized anywhere.
    ///
    /// The decision is taken on the raw engine output; a result holding any
    /// lowercase Latin/Cyrillic letter is left alone. Characters outside those two
    /// scripts never take part in the decision and are copied through unchanged, so
    /// a result is byte-identical only when it has no Latin/Cyrillic letters at all
    /// (or the detector rejected it) — `"HELLO 日本"` still becomes `"Hello 日本"`.
    pub fix_caps_lock: bool,
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
    /// The user stopped the request (AI API "Stop"); a neutral outcome, not a failure.
    RecognizeCancelled {
        request_id: u64,
    },
    /// The pre-dispatch gate found this external model bundle not installed (the load or
    /// recognize also fails with the "not downloaded" error): the tab re-probes its
    /// download state so the panel shows the button that error names.
    ExternalModelMissing(Vec<&'static ExternalModelSpec>),
    /// Progress of an app-managed model download of the running load or recognize (thinned
    /// by `ai_models`); the state is `DownloadingModel` meanwhile. Retries are progress.
    ModelDownloadProgress(ModelDownloadEvent),
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
    /// Live state and Stop switch of the AI API request the worker runs (shared with it).
    generation: GenerationTracker,
    /// Cancel token of the most recently sent Load / Recognize: set when a newer command
    /// supersedes it and on drop, so a model download it runs stops (`OcrFailure::Cancelled`).
    command_cancel: Arc<AtomicBool>,
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
        let generation = GenerationTracker::new();
        let worker_generation = generation.clone();
        let worker_thread = thread::spawn(move || worker_loop(cmd_rx, evt_tx, &worker_generation));
        Self {
            state: OcrLoadState::NotLoaded,
            last_error: None,
            last_result: None,
            pending_request: None,
            cmd_tx,
            evt_rx,
            worker_thread: Some(worker_thread),
            generation,
            command_cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Supersedes the previous command (its model download, if any, stops) and returns the
    /// fresh cancel token of the command about to be sent.
    fn next_command_cancel(&mut self) -> Arc<AtomicBool> {
        self.command_cancel.store(true, Ordering::Relaxed);
        self.command_cancel = Arc::new(AtomicBool::new(false));
        Arc::clone(&self.command_cancel)
    }

    /// The OCR panel's Cancel of a running model download: stops the download of the newest
    /// Load / Recognize (older ones were already superseded). A no-op unless the state is
    /// `DownloadingModel`. The command then ends as a cancelled load (`StateChanged(NotLoaded)`)
    /// or `RecognizeCancelled`; the staged bytes are kept for the next attempt.
    pub fn cancel_model_download(&self) {
        if self.state == OcrLoadState::DownloadingModel {
            self.command_cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Live state of the AI API OCR request in flight, for the panel's generation widget.
    pub fn generation_snapshot(&self) -> GenerationSnapshot {
        self.generation.snapshot()
    }

    /// Stops AI API generation `run_id` (the `run_id` of the snapshot the "Stop" was clicked on)
    /// if it is still in flight: the HTTP request is dropped and the request ends with
    /// `OcrControllerEvent::RecognizeCancelled`. A generation that already finished is left
    /// alone, so a stale click never stops a later queued request. Queued requests still run.
    pub fn cancel_generation(&self, run_id: u64) {
        self.generation.cancel_run(run_id);
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
        let cancel = self.next_command_cancel();
        if self
            .cmd_tx
            .send(WorkerCommand::Load { engine, options, cancel })
            .is_err()
        {
            self.last_error = Some(t!("translation.ocr.worker_unavailable_error").to_string());
            self.set_state(OcrLoadState::Error);
        }
    }

    pub fn request_recognize(&mut self, request: OcrRecognizeRequest) {
        match self.state {
            OcrLoadState::Ready => {
                let cancel = self.next_command_cancel();
                if self.cmd_tx.send(WorkerCommand::Recognize { request, cancel }).is_err() {
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

    pub fn poll_events(&mut self) -> Vec<OcrControllerEvent> {
        let mut out = Vec::new();
        for _ in 0..OCR_EVENT_POLL_BUDGET {
            match self.evt_rx.try_recv() {
                Ok(WorkerEvent::ExternalModelMissing(specs)) => {
                    out.push(OcrControllerEvent::ExternalModelMissing(specs));
                }
                Ok(WorkerEvent::ModelDownloadStarted) => {
                    if self.state != OcrLoadState::DownloadingModel {
                        self.set_state(OcrLoadState::DownloadingModel);
                        out.push(OcrControllerEvent::StateChanged(
                            OcrLoadState::DownloadingModel,
                        ));
                    }
                }
                Ok(WorkerEvent::ModelDownloadProgress(event)) => {
                    if self.state != OcrLoadState::DownloadingModel {
                        self.set_state(OcrLoadState::DownloadingModel);
                        out.push(OcrControllerEvent::StateChanged(
                            OcrLoadState::DownloadingModel,
                        ));
                    }
                    out.push(OcrControllerEvent::ModelDownloadProgress(event));
                }
                Ok(WorkerEvent::LoadCancelled) => {
                    // A cancelled model download is not an engine error: back to NotLoaded
                    // (the next request loads again and resumes the staged bytes), and the
                    // request that waited for this load ends as cancelled.
                    let dropped_request_id = self.pending_request.take().map(|req| req.request_id);
                    self.set_state(OcrLoadState::NotLoaded);
                    out.push(OcrControllerEvent::StateChanged(OcrLoadState::NotLoaded));
                    if let Some(request_id) = dropped_request_id {
                        out.push(OcrControllerEvent::RecognizeCancelled { request_id });
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
                    if let Some(request) = self.pending_request.take() {
                        let cancel = self.next_command_cancel();
                        if self.cmd_tx.send(WorkerCommand::Recognize { request, cancel }).is_err() {
                            self.last_error = Some(t!("translation.ocr.worker_unavailable_error").to_string());
                            self.set_state(OcrLoadState::Error);
                            out.push(OcrControllerEvent::StateChanged(OcrLoadState::Error));
                        }
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
                Ok(WorkerEvent::RecognizeCancelled { request_id }) => {
                    // The engine itself is fine: a stop is not an error, so any earlier error
                    // text is cleared like after a successful recognize.
                    self.last_error = None;
                    if self.state != OcrLoadState::Ready {
                        self.set_state(OcrLoadState::Ready);
                        out.push(OcrControllerEvent::StateChanged(OcrLoadState::Ready));
                    }
                    out.push(OcrControllerEvent::RecognizeCancelled { request_id });
                }
                Ok(WorkerEvent::RecognizeErr { request_id, error }) => {
                    self.last_error = Some(error.clone());
                    self.set_state(OcrLoadState::Error);
                    out.push(OcrControllerEvent::StateChanged(OcrLoadState::Error));
                    out.push(OcrControllerEvent::RecognizeFailed { request_id, error });
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
        // Abort the AI API request in flight first, and make every queued AI API request return
        // before its keyring read (`run_ai_api_ocr_request` checks `is_closed`): the join below
        // would otherwise block the GUI thread until the provider answers. Queued non-AI
        // (backend / native) requests still run before the worker sees `Stop`, but none of
        // them downloads a model (their tokens are set).
        self.generation.close();
        // Stop the model download the newest command may run; older queued commands were
        // already superseded, so none of them downloads either.
        self.command_cancel.store(true, Ordering::Relaxed);
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
        /// Stops this load's model download (see `TranslationOcrController::command_cancel`).
        cancel: Arc<AtomicBool>,
    },
    Recognize {
        request: OcrRecognizeRequest,
        /// Stops this request's model download.
        cancel: Arc<AtomicBool>,
    },
    Stop,
}

#[derive(Debug)]
enum WorkerEvent {
    /// Sent by the external-model gate right before the error it causes (the whole bundle).
    ExternalModelMissing(Vec<&'static ExternalModelSpec>),
    ModelDownloadStarted,
    /// One (thinned) app-managed model download event.
    ModelDownloadProgress(ModelDownloadEvent),
    BackendLoadStarted,
    LoadOk,
    LoadErr(String),
    /// The load's model download was cancelled (superseded or controller dropped).
    LoadCancelled,
    RecognizeOk {
        request_id: u64,
        result: OcrRecognizeResult,
    },
    RecognizeErr {
        request_id: u64,
        error: String,
    },
    RecognizeCancelled {
        request_id: u64,
    },
}

fn worker_loop(cmd_rx: Receiver<WorkerCommand>, evt_tx: Sender<WorkerEvent>, generation: &GenerationTracker) {
    let mut page_cache = PageImageCache::new(OCR_PAGE_CACHE_MAX_ITEMS, OCR_PAGE_CACHE_MAX_BYTES);

    while let Ok(command) = cmd_rx.recv() {
        match command {
            WorkerCommand::Stop => break,
            WorkerCommand::Load { engine, options, cancel } => {
                let event = match warmup_ocr_engine(engine, &options, &cancel, &evt_tx) {
                    Ok(()) => WorkerEvent::LoadOk,
                    Err(OcrFailure::Cancelled) => WorkerEvent::LoadCancelled,
                    Err(OcrFailure::Failed(err)) => WorkerEvent::LoadErr(err),
                };
                let _ = evt_tx.send(event);
            }
            WorkerCommand::Recognize { request, cancel } => {
                let job = QueuedRecognize { request, cancel };
                if run_recognize_command(job, &mut page_cache, &cmd_rx, &evt_tx, generation).is_break() {
                    break;
                }
            }
        }
    }
}

/// A recognize command as the worker runs it: the request plus its cancel token.
#[derive(Debug)]
struct QueuedRecognize {
    request: OcrRecognizeRequest,
    cancel: Arc<AtomicBool>,
}

/// Runs an `ai_models::ensure_*` call for the OCR worker: `cancel` is the command's token and
/// every (thinned) download event goes to the controller as `ModelDownloadProgress`.
fn ensure_with_events<T>(
    cancel: &AtomicBool,
    evt_tx: &Sender<WorkerEvent>,
    ensure: impl FnOnce(&mut EnsureCtx<'_>) -> Result<T, ModelEnsureError>,
) -> Result<T, ModelEnsureError> {
    let mut forward = |event: &ModelDownloadEvent| {
        // A gone controller is noticed by the caller's next send; progress is best effort.
        let _ = evt_tx.send(WorkerEvent::ModelDownloadProgress(event.clone()));
    };
    ensure(&mut EnsureCtx { cancel, progress: &mut forward })
}

fn warmup_ocr_engine(
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
    cancel: &AtomicBool,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<(), OcrFailure> {
    if engine == OcrEngine::AiApi {
        return validate_ai_api_options(options).map(|_target| ()).map_err(OcrFailure::from);
    }
    // External models are never downloaded implicitly: a missing Baberu / PaddleOCR-VL /
    // EasyOCR / Surya model fails the load here, on every route, with the error naming the
    // panel button.
    let model_dirs = gate_external_models(engine, options, evt_tx)?;

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
        ms_log::runtime_log::log_info(
            "[ocr] native AI runtime route active; skipping Python backend warmup \
             (native OCR loads lazily on first recognize).",
        );
        return Ok(());
    }

    warmup_backend_ocr_engine(engine, options, &model_dirs, cancel, evt_tx)
}

/// Whether an OCR route requires warming the Python backend at load time.
///
/// Only [`OcrRoute::Backend`] needs the backend; the native routes lazy-load the
/// in-process ONNX Runtime on first recognize and therefore reach `Ready` without
/// the backend. Pure so the warmup decision is unit-testable.
#[cfg(not(target_arch = "wasm32"))]
fn ocr_route_needs_backend_warmup(route: OcrRoute) -> bool {
    match route {
        OcrRoute::NativeManga(_) | OcrRoute::NativePaddle | OcrRoute::NativeBaberu => false,
        OcrRoute::Backend => true,
    }
}

fn warmup_backend_ocr_engine(
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
    model_dirs: &ExternalModelDirs,
    cancel: &AtomicBool,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<(), OcrFailure> {
    ensure_backend_ocr_models(engine, options, cancel, evt_tx)?;
    ensure_v2_backend_ready()?;
    let _ = evt_tx.send(WorkerEvent::BackendLoadStarted);
    let method = engine
        .backend_method()
        .ok_or_else(|| t!("translation.ocr.method_not_set_error").to_string())?;
    // Warm the per-engine worker/model by recognizing a tiny throwaway image.
    // The header carries the engine params; the blob carries the raw PNG bytes.
    let header_fields = ocr_header_fields(options, model_dirs, true, false)?;
    let dummy_png = dummy_warmup_png()?;
    // A warmup that races against a cancel just returns; treat that as success
    // (the model still got touched). Errors/transport are surfaced normally.
    match ocr_backend_call(method, header_fields, &dummy_png) {
        Ok(_) => Ok(()),
        Err(CallError::Interrupted(_)) => Ok(()),
        Err(err) => Err(OcrFailure::Failed(err.to_string())),
    }
}

/// Builds the inline header fields (engine params) shared by every backend OCR
/// method. The backend reads only the fields its method needs and ignores the
/// rest, so sending the full superset keeps a single builder for all engines.
/// The image itself is NOT in the header — it travels as the request blob.
///
/// `model_dirs` carries the resolved external models: `paddle_vl_model_dir` (absolute
/// checkpoint directory, `ocr.paddle_vl` requires it), `baberu_model_files`
/// (`{vision, prefill, step, vocab}` absolute paths, `ocr.baberu` requires it),
/// `easyocr_model_dir` (`ocr.easy`, v12) and `surya_recognition_dir` + `surya_detection_dir`
/// (`ocr.surya`, v12); each is `null` when not resolved for this request. `easy_langs` is
/// the CANONICAL list (`normalize_easyocr_langs`, comma-joined), which the backend uses as is.
///
/// # Errors
/// A model path that is not valid UTF-8 (it could not be sent without being mangled, so
/// it is rejected instead of lossily converted).
fn ocr_header_fields(
    options: &OcrRuntimeOptions,
    model_dirs: &ExternalModelDirs,
    join_newlines: bool,
    reflect_strings: bool,
) -> Result<Value, String> {
    let paddle_vl_model_dir = model_dirs.paddle_vl.as_deref().map(wire_path).transpose()?;
    let easyocr_model_dir = model_dirs.easyocr.as_deref().map(wire_path).transpose()?;
    let surya_recognition_dir = model_dirs.surya_recognition.as_deref().map(wire_path).transpose()?;
    let surya_detection_dir = model_dirs.surya_detection.as_deref().map(wire_path).transpose()?;
    let baberu_model_files = model_dirs
        .baberu
        .as_ref()
        .map(|files| -> Result<Value, String> {
            Ok(json!({
                "vision": wire_path(&files.vision)?,
                "prefill": wire_path(&files.prefill)?,
                "step": wire_path(&files.step)?,
                "vocab": wire_path(&files.vocab)?,
            }))
        })
        .transpose()?;
    let mut fields = json!({
        "join_newlines": join_newlines,
        "reflect_strings": reflect_strings,
        "manga_model": options.manga_model,
        "paddle_lang": options.paddle_lang,
        "paddle_vl_script": options.paddle_vl_script,
        "paddle_vl_model": options.paddle_vl_model.key(),
        "paddle_vl_model_dir": paddle_vl_model_dir,
        "baberu_model_files": baberu_model_files,
        "surya_task_name": options.surya_task_name,
        "surya_recognize_math": options.surya_recognize_math,
        "surya_sort_lines": options.surya_sort_lines,
        "surya_drop_repeated_text": options.surya_drop_repeated_text,
        "surya_max_sliding_window": non_zero_u32_to_option(options.surya_max_sliding_window),
        "surya_max_tokens": non_zero_u32_to_option(options.surya_max_tokens)
    });
    // The v12 keys come from the protocol mirror (one spelling for both sides).
    if let Value::Object(map) = &mut fields {
        map.insert(backend_ipc::protocol::HEADER_EASY_LANGS.to_owned(), json!(normalize_easyocr_langs(&options.easy_langs).join(",")));
        map.insert(backend_ipc::protocol::HEADER_EASYOCR_MODEL_DIR.to_owned(), json!(easyocr_model_dir));
        map.insert(backend_ipc::protocol::HEADER_SURYA_RECOGNITION_DIR.to_owned(), json!(surya_recognition_dir));
        map.insert(backend_ipc::protocol::HEADER_SURYA_DETECTION_DIR.to_owned(), json!(surya_detection_dir));
    }
    Ok(fields)
}

/// A model path as a wire string; never lossy.
fn wire_path(path: &Path) -> Result<&str, String> {
    path.to_str().ok_or_else(|| tf!("translation.ocr.model_path_not_utf8_error", path = path.display()))
}

/// The resolved on-disk locations of the external models one OCR request needs. Empty
/// (`Default`) for engines without an external model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ExternalModelDirs {
    /// Installed directory of the selected PaddleOCR-VL variant.
    pub paddle_vl: Option<PathBuf>,
    /// Installed Baberu OCR files.
    pub baberu: Option<BaberuFiles>,
    /// The shared EasyOCR directory (detector + the selected script's recognizer installed).
    pub easyocr: Option<PathBuf>,
    /// Installed Surya `text_recognition` checkpoint.
    pub surya_recognition: Option<PathBuf>,
    /// Installed Surya `text_detection` checkpoint.
    pub surya_detection: Option<PathBuf>,
}

/// The Surya OCR bundle: the detection checkpoint first (shared with the Surya text
/// detector), then recognition. `ocr.surya` requires both directories.
pub(crate) const SURYA_OCR_SPECS: [&ExternalModelSpec; 2] = [&SURYA_DETECTION_2025_05_07, &SURYA_RECOGNITION_2025_09_23];

/// The pinned external model bundle `engine` needs: `paddle_vl` picks the PaddleOCR-VL
/// variant, `easy_langs` (the raw setting) the EasyOCR script. Empty for engines whose models
/// are app-managed elsewhere, and for an EasyOCR language list easyocr cannot read
/// ([`easyocr_selection`] explains why). Single owner of the engine -> external spec mapping
/// (gate, panel download block, tab status probes).
pub(crate) fn external_model_specs(engine: OcrEngine, paddle_vl: PaddleVlVariant, easy_langs: &str) -> Vec<&'static ExternalModelSpec> {
    match engine {
        OcrEngine::Baberu => vec![&BABERU_OCR],
        OcrEngine::PaddleVl => vec![paddle_vl.spec()],
        OcrEngine::EasyOcr => easyocr_selection(easy_langs).map(|(_, script)| vec![easyocr_spec(script)]).unwrap_or_default(),
        OcrEngine::Surya => SURYA_OCR_SPECS.to_vec(),
        OcrEngine::MangaOcr | OcrEngine::PaddleOcr | OcrEngine::AiApi => Vec::new(),
    }
}

/// The canonical EasyOCR language list of the raw setting and the recognizer script easyocr
/// selects for it (both owned by `ms_sysprobe::ai_models::easyocr`).
///
/// # Errors
/// [`EasyOcrLangError`] exactly where easyocr would refuse the list.
pub(crate) fn easyocr_selection(easy_langs: &str) -> Result<(Vec<String>, EasyOcrScript), EasyOcrLangError> {
    let langs = normalize_easyocr_langs(easy_langs);
    let script = easyocr_script_for(&langs)?;
    Ok((langs, script))
}

/// Localized user-facing text for an EasyOCR language-list error (the error's `Display` is
/// English diagnostic text for logs). Only language-neutral facts are substituted: language
/// codes and the recognizer file name.
pub(crate) fn easyocr_lang_error_message(err: &EasyOcrLangError) -> String {
    match err {
        EasyOcrLangError::Empty => t!("translation.ocr.easyocr_no_language_error").to_string(),
        EasyOcrLangError::Unsupported { codes } => tf!("translation.ocr.easyocr_unsupported_language_error", codes = codes.join(", ")),
        EasyOcrLangError::Incompatible { script, compatible } => tf!(
            "translation.ocr.easyocr_incompatible_languages_error",
            model = script.recognizer_file(),
            codes = compatible.join(", ")
        ),
    }
}

/// The pre-dispatch gate under the real `side_models` root. Worker thread only (stats files).
///
/// # Errors
/// See [`gate_external_models_in`].
fn gate_external_models(
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<ExternalModelDirs, String> {
    gate_external_models_in(&config::side_models_dir(), engine, options, evt_tx)
}

/// Why the gate refused a request: the localized message, and the bundle that is not
/// installed (empty when the refusal is not about a download, e.g. an EasyOCR language list).
#[derive(Debug, Clone, PartialEq, Eq)]
struct GateRefusal {
    message: String,
    missing: Vec<&'static ExternalModelSpec>,
}

/// [`resolve_external_model_dirs_in`] plus the miss report: when the bundle is not
/// installed, `WorkerEvent::ExternalModelMissing(bundle)` is sent BEFORE the caller publishes
/// the error, so the tab re-probes the panel state in the same poll.
///
/// # Errors
/// The refusal's localized message (see [`resolve_external_model_dirs_in`]).
fn gate_external_models_in(
    side_models_root: &Path,
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<ExternalModelDirs, String> {
    resolve_external_model_dirs_in(side_models_root, engine, options).map_err(|refusal| {
        if !refusal.missing.is_empty() {
            let key = crate::ocr_model_download::bundle_key(&refusal.missing);
            if evt_tx.send(WorkerEvent::ExternalModelMissing(refusal.missing)).is_err() {
                ms_log::runtime_log::log_info(format!("[ocr] controller gone; cannot report missing model '{key}'"));
            }
        }
        refusal.message
    })
}

/// Resolves the installed external model bundle of `engine` under `side_models_root`
/// (stat-only, never downloads). Engines without an external model get
/// `ExternalModelDirs::default()`.
///
/// # Errors
/// For EasyOCR first the localized language-list error (`missing` empty); then, when any spec
/// of the bundle is not installed, the localized "model is not downloaded" message naming the
/// exact download / resume button the OCR panel shows for the bundle's current state.
fn resolve_external_model_dirs_in(
    side_models_root: &Path,
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
) -> Result<ExternalModelDirs, GateRefusal> {
    if engine == OcrEngine::EasyOcr
        && let Err(err) = easyocr_selection(&options.easy_langs)
    {
        ms_log::runtime_log::log_info(format!("[ocr] EasyOCR language list '{}' refused: {err}", options.easy_langs));
        return Err(GateRefusal { message: easyocr_lang_error_message(&err), missing: Vec::new() });
    }
    let specs = external_model_specs(engine, options.paddle_vl_model, &options.easy_langs);
    let statuses: Vec<(&'static ExternalModelSpec, ExternalModelStatus)> =
        specs.iter().map(|spec| (*spec, external_model_status(side_models_root, spec))).collect();
    if let Some((spec, status)) = statuses.iter().find(|(_, status)| *status != ExternalModelStatus::Installed) {
        ms_log::runtime_log::log_info(format!(
            "[ocr] external model '{}' is not installed ({status:?}); asking the user to download it",
            spec.id
        ));
        return Err(GateRefusal { message: model_not_downloaded_error(&statuses), missing: specs });
    }
    let dir_of = |index: usize| specs.get(index).map(|spec| spec.local_dir(side_models_root));
    Ok(match engine {
        OcrEngine::Baberu => ExternalModelDirs { baberu: dir_of(0).map(|dir| baberu_files(&dir)), ..ExternalModelDirs::default() },
        OcrEngine::PaddleVl => ExternalModelDirs { paddle_vl: dir_of(0), ..ExternalModelDirs::default() },
        // Every script spec lives in the one shared EasyOCR directory.
        OcrEngine::EasyOcr => ExternalModelDirs { easyocr: dir_of(0), ..ExternalModelDirs::default() },
        // `SURYA_OCR_SPECS` order: detection, recognition.
        OcrEngine::Surya => ExternalModelDirs { surya_detection: dir_of(0), surya_recognition: dir_of(1), ..ExternalModelDirs::default() },
        // `external_model_specs` is empty for these: nothing to resolve.
        OcrEngine::MangaOcr | OcrEngine::PaddleOcr | OcrEngine::AiApi => ExternalModelDirs::default(),
    })
}

/// The localized "not downloaded" error for a bundle with these probe answers, naming the
/// button the panel shows for that state ("Download (size)" or "Resume download (done of
/// total)"). The caption comes from `ocr_model_download::download_label_for_statuses`, the
/// panel's own owner of the visible caption; the tab re-probes the panel on the accompanying
/// `ExternalModelMissing`, so both read the same disk state.
fn model_not_downloaded_error(statuses: &[(&'static ExternalModelSpec, ExternalModelStatus)]) -> String {
    let button = crate::ocr_model_download::download_label_for_statuses(statuses);
    tf!("translation.ocr.model_not_downloaded_error", button = button)
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
    publish_worker_event(event, evt_tx)
}

/// Sends `event` to the controller. Returns `ControlFlow::Break` if the event
/// channel is gone (the worker should stop).
fn publish_worker_event(event: WorkerEvent, evt_tx: &Sender<WorkerEvent>) -> std::ops::ControlFlow<()> {
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
    /// Run Baberu OCR natively (vision on the selected EP, decoders on CPU) on the files
    /// resolved by the gate.
    NativeBaberu,
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
/// a native path) and Baberu OCR to [`OcrRoute::NativeBaberu`]. Every other engine, and every non-native/Suspect case, routes
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
        OcrEngine::Baberu => OcrRoute::NativeBaberu,
        // These engines have no native path yet; they use the Python backend.
        OcrEngine::EasyOcr
        | OcrEngine::PaddleVl
        | OcrEngine::Surya
        | OcrEngine::AiApi => OcrRoute::Backend,
    }
}

/// What the selected OCR engine/model runs on under `runtime` and the SIGILL load `guard`.
///
/// Reuses [`ocr_route`] so it can never disagree with the actual dispatch: a native route
/// (MangaOCR ONNX, PaddleOCR or Baberu OCR under the native runtime with a non-`Suspect`
/// guard) is [`AiRouteKind::Native`], every other local engine/model is
/// [`AiRouteKind::Backend`]. `AiApi` is [`AiRouteKind::Cloud`]: it runs over `genai` and never
/// touches the backend socket (its `ocr_route` value is `Backend` only because `OcrRoute` has
/// no AI-API variant). A `Suspect` guard falls back to the backend. Pure and testable.
pub(crate) fn ocr_route_kind(
    engine: OcrEngine,
    manga_model_key: &str,
    runtime: config::AiRuntime,
    guard: config::OrtLoadDecision,
) -> AiRouteKind {
    // Engines that never use the backend socket (AiApi over `genai`) are cloud routes,
    // regardless of runtime/guard.
    if !engine.requires_backend() {
        return AiRouteKind::Cloud;
    }
    match ocr_route(runtime, engine, manga_model_key, guard) {
        OcrRoute::NativeManga(_) | OcrRoute::NativePaddle | OcrRoute::NativeBaberu => AiRouteKind::Native,
        OcrRoute::Backend => AiRouteKind::Backend,
    }
}

/// The single source of truth every OCR trigger, the load button and the panel notice
/// consult: the route kind of the selected engine/model ([`ocr_route_kind`]) checked against
/// the process's local-AI availability by the one rule, [`AiRouteKind::availability`]. An
/// `Ok(AiRouteKind::Backend)` still needs a connected backend (the caller's health check).
///
/// # Errors
/// The [`LocalAiOff`] of the runtime the route needs (`--no-ai`, or a platform without it).
pub(crate) fn ocr_route_availability(
    engine: OcrEngine,
    manga_model_key: &str,
    runtime: config::AiRuntime,
    guard: config::OrtLoadDecision,
    local_ai: &LocalAiAvailability,
) -> Result<AiRouteKind, LocalAiOff> {
    let kind = ocr_route_kind(engine, manga_model_key, runtime, guard);
    kind.availability(local_ai).map(|()| kind)
}

/// Reads the native-routing inputs (selected AI runtime + provider-scoped SIGILL
/// load guard) fresh off disk.
///
/// Kept as one helper so [`current_ocr_route`] and the UI-side readiness gate read
/// the exact same inputs. Disk I/O (and, on the process's first call, the native
/// selection's hardware probes): worker threads only — the tab reads it through the
/// background refresh in `tab/route_inputs.rs`. Desktop-only: the native runtime is
/// compiled out on the web build.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn read_ocr_route_inputs() -> (config::AiRuntime, config::OrtLoadDecision) {
    let cfg = load_user_settings_for_routing();
    let runtime = effective_ai_runtime(&cfg);
    let scope = native_runtime::native_load_scope_key();
    let decision = config::ort_load_decision(config::read_ort_load_guard(&cfg, &scope));
    (runtime, decision)
}

/// Whether the last [`load_user_settings_for_routing`] failed; only edges are logged, so
/// the tab's background refresh (every `OCR_ROUTE_INPUTS_CACHE_TTL`) does not spam the log.
#[cfg(not(target_arch = "wasm32"))]
static ROUTING_SETTINGS_READ_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Records `failed` in `flag` and returns whether that changed the recorded state (the
/// edge that is worth one log line).
#[cfg(not(target_arch = "wasm32"))]
fn failure_state_changed(flag: &std::sync::atomic::AtomicBool, failed: bool) -> bool {
    flag.swap(failed, std::sync::atomic::Ordering::Relaxed) != failed
}

/// Reads `user_config` as stored for the OCR routing decision; worker threads only (disk I/O).
///
/// A failed read or parse keeps the routing fallback `Value::Null` (its defaults: the
/// effective runtime of an empty config, no SIGILL guard entry) and is logged with the
/// error (which names the document path) when the state changes (first failure, recovery), not on
/// every repeat.
#[cfg(not(target_arch = "wasm32"))]
fn load_user_settings_for_routing() -> Value {
    match config::load_raw_user_settings_for_startup() {
        Ok(cfg) => {
            if failure_state_changed(&ROUTING_SETTINGS_READ_FAILED, false) {
                ms_log::runtime_log::log_info("[ocr] user_config readable again for OCR routing");
            }
            cfg
        }
        Err(err) => {
            if failure_state_changed(&ROUTING_SETTINGS_READ_FAILED, true) {
                ms_log::runtime_log::log_warn(format!(
                    "[ocr] failed to read user_config for OCR routing; routing with default \
                     settings (empty config) until it is readable again.\nError (names the \
                     document path): {err:#}\nPossible cause: the document is unreadable or malformed"
                ));
            }
            Value::Null
        }
    }
}

/// Assembles an [`OcrRecognizeResult`] from one recognized string (native ONNX engines and
/// the AI API answer), applying `join_newlines`/`reflect_strings` exactly like the backend
/// path: split on newlines and drop blank lines; reverse line order when `reflect_strings`;
/// join with `"\n"` (or `" "`) per `join_newlines`, then trim. The one owner of the
/// Rust-side line join, so every route honours "keep line breaks" the same way.
#[cfg(not(target_arch = "wasm32"))]
fn assemble_line_ocr_result(
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
        ms_log::runtime_log::log_info(
            "[ocr] native AI runtime selected but the current OCR engine/model has no native path \
             (native covers MangaOCR ONNX + PaddleOCR + Baberu OCR); using the Python backend.",
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
/// Mirrors `text_detector::native::current_detector_route`.
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
    let cfg = load_user_settings_for_routing();
    effective_ai_runtime(&cfg) == config::AiRuntime::Native
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
    /// The request's cancel token stopped a model download: published as
    /// `RecognizeCancelled`, never retried on the backend.
    Cancelled,
}

/// Attempts the native ONNX Runtime OCR path (MangaOCR, PaddleOCR or Baberu OCR) for
/// `request`; `model_dirs` is the gate's resolution (Baberu reads its files from it).
///
/// Returns [`NativeOcrOutcome::Ok`] on success, [`NativeOcrOutcome::NotNative`]
/// when the request has no native path (so the backend handles it), and
/// [`NativeOcrOutcome::Failed`] when the native route was taken but crop decode or
/// inference failed, and [`NativeOcrOutcome::Cancelled`] when `cancel` stopped a model
/// download (model events go to the controller as `ModelDownloadProgress`). The caller decides whether a `Failed` outcome falls back to
/// the backend (if it is up) or is surfaced to the user (if the backend is offline).
/// Failures are always logged with diagnostic context, never hidden. Runs on the
/// OCR worker thread (blocking download/inference).
#[cfg(not(target_arch = "wasm32"))]
fn try_native_ocr(
    request: &OcrRecognizeRequest,
    model_dirs: &ExternalModelDirs,
    page_cache: &mut PageImageCache,
    cancel: &AtomicBool,
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
            ms_log::runtime_log::log_error(format!("[ocr] native OCR crop decode failed: {err}"));
            return NativeOcrOutcome::Failed(tf!("translation.ocr.native_prepare_image_error", err = err));
        }
    };

    // Adapt ORT dylib progress to the OCR download-state event (once) and forward model
    // download events, so the UI shows activity while the runtime/model is fetched on first
    // use. Sends are best effort: a gone controller is noticed by the publish that follows.
    let mut reported = false;
    let mut progress = |update: native_runtime::NativeProgress<'_>| match update {
        native_runtime::NativeProgress::Runtime(snapshot) => {
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
        }
        native_runtime::NativeProgress::Model(event) => {
            let _ = evt_tx.send(WorkerEvent::ModelDownloadProgress(event.clone()));
        }
    };

    // Run the native op. PaddleOCR returns lines already; join them so the shared
    // `assemble_line_ocr_result` applies the same join/reflect logic as MangaOCR.
    let native_result: Result<String, native_runtime::NativeRuntimeError> = match route {
        OcrRoute::NativeManga(variant) => {
            native_runtime::recognize_manga(variant, &rgba, cancel, &mut progress)
        }
        OcrRoute::NativePaddle => {
            native_runtime::recognize_paddle(&request.options.paddle_lang, &rgba, cancel, &mut progress)
                .map(|lines| lines.join("\n"))
        }
        OcrRoute::NativeBaberu => match model_dirs.baberu.as_ref() {
            Some(files) => native_runtime::recognize_baberu(&baberu_model_paths(files), &rgba, cancel, &mut progress),
            // The gate resolves Baberu files before any Baberu dispatch; without them the
            // model is simply not installed.
            None => return NativeOcrOutcome::Failed(model_not_downloaded_error(&[(&BABERU_OCR, ExternalModelStatus::Missing)])),
        },
        // `Backend` is handled above; unreachable here but matched exhaustively.
        OcrRoute::Backend => return NativeOcrOutcome::NotNative,
    };

    match native_result {
        Ok(text) => NativeOcrOutcome::Ok(assemble_line_ocr_result(
            &text,
            request.join_newlines,
            request.reflect_strings,
        )),
        Err(native_runtime::NativeRuntimeError::Cancelled) => {
            ms_log::runtime_log::log_info(format!("[ocr] native {} request cancelled during a model download", native_ocr_engine_label(route)));
            NativeOcrOutcome::Cancelled
        }
        Err(err) => {
            // Log with the fallback framing for diagnostics; a Suspect guard never
            // reaches here (it routes to Backend). Log the `Debug` form (a stable
            // English variant name) so logs stay grep-able across UI languages, while
            // the user-facing `Failed` message uses the localized `Display` so an
            // offline-backend user sees the real reason instead of "backend offline".
            let engine_label = native_ocr_engine_label(route);
            ms_log::runtime_log::log_error(format!(
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
        OcrRoute::NativeBaberu => "Baberu OCR",
        OcrRoute::Backend => "OCR",
    }
}

/// The catalog's resolved Baberu files as the native runtime's path set (the runtime owns
/// no layout; the catalog does).
#[cfg(not(target_arch = "wasm32"))]
fn baberu_model_paths(files: &BaberuFiles) -> native_runtime::BaberuModelPaths {
    native_runtime::BaberuModelPaths {
        vision: files.vision.clone(),
        prefill: files.prefill.clone(),
        step: files.step.clone(),
        vocab: files.vocab.clone(),
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
    mut job: QueuedRecognize,
    page_cache: &mut PageImageCache,
    cmd_rx: &Receiver<WorkerCommand>,
    evt_tx: &Sender<WorkerEvent>,
    generation: &GenerationTracker,
) -> std::ops::ControlFlow<()> {
    loop {
        let request = &job.request;
        let request_id = request.request_id;

        // The AI-API engine runs over `genai`, not the framed socket, so it has
        // no IPC cancel; it runs synchronously and is stopped through the
        // generation tracker instead (the panel's "Stop", or the controller drop).
        if request.engine == OcrEngine::AiApi {
            let result = match run_ai_api_ocr_request(request, page_cache, generation) {
                Ok(mut result) => {
                    apply_post_ocr_processing(&mut result, request);
                    Ok(result)
                }
                Err(OcrFailure::Cancelled) => {
                    return publish_worker_event(WorkerEvent::RecognizeCancelled { request_id }, evt_tx);
                }
                Err(OcrFailure::Failed(error)) => Err(error),
            };
            return publish_recognize(request_id, result, evt_tx);
        }

        // The external-model gate runs before the route decision, so a missing Baberu /
        // PaddleOCR-VL model fails the same way on the native and the backend route.
        let model_dirs = match gate_external_models(request.engine, &request.options, evt_tx) {
            Ok(dirs) => dirs,
            Err(error) => return publish_recognize(request_id, Err(error), evt_tx),
        };

        // Native ONNX Runtime route (MangaOCR + PaddleOCR + Baberu OCR). On success we publish
        // the native result. On a native failure we preserve the fallback contract
        // ONLY when the backend is up; when the backend is offline we surface the
        // real native error instead of falling through to the misleading
        // "backend offline" message. A non-native op falls through to the backend.
        // Desktop-only: the native runtime is compiled out on the web build.
        #[cfg(not(target_arch = "wasm32"))]
        match try_native_ocr(request, &model_dirs, page_cache, &job.cancel, evt_tx) {
            NativeOcrOutcome::Ok(mut result) => {
                apply_post_ocr_processing(&mut result, request);
                return publish_recognize(request_id, Ok(result), evt_tx);
            }
            NativeOcrOutcome::Cancelled => {
                return publish_worker_event(WorkerEvent::RecognizeCancelled { request_id }, evt_tx);
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

        match run_backend_recognize(request, &model_dirs, page_cache, &job.cancel, cmd_rx, evt_tx) {
            BackendRecognizeFlow::Outcome(RecognizeOutcome::Done(result)) => {
                let result = result.map(|mut result| {
                    apply_post_ocr_processing(&mut result, request);
                    result
                });
                return publish_recognize(request_id, result, evt_tx);
            }
            BackendRecognizeFlow::Cancelled => {
                return publish_worker_event(WorkerEvent::RecognizeCancelled { request_id }, evt_tx);
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
                job = *next;
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
    Superseded(Box<QueuedRecognize>),
    /// The request's cancel token stopped its model download (published as cancelled).
    Cancelled,
    /// A `Stop` arrived; the worker should break.
    Stop,
}

/// Runs one backend recognize with cancellation. Begins the framed call, then
/// waits for the terminal frame on a helper thread while the calling thread
/// watches `cmd_rx` for a newer `Recognize`/`Stop`, cancelling the in-flight
/// request if one arrives.
fn run_backend_recognize(
    request: &OcrRecognizeRequest,
    model_dirs: &ExternalModelDirs,
    page_cache: &mut PageImageCache,
    cancel: &AtomicBool,
    cmd_rx: &Receiver<WorkerCommand>,
    evt_tx: &Sender<WorkerEvent>,
) -> BackendRecognizeFlow {
    match ensure_backend_ocr_models(request.engine, &request.options, cancel, evt_tx) {
        Ok(()) => {}
        Err(ModelEnsureError::Cancelled) => return BackendRecognizeFlow::Cancelled,
        Err(ModelEnsureError::Failed(err)) => return BackendRecognizeFlow::Outcome(RecognizeOutcome::Done(Err(err))),
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
    let header_fields = match ocr_header_fields(&request.options, model_dirs, request.join_newlines, request.reflect_strings) {
        Ok(fields) => fields,
        Err(err) => return BackendRecognizeFlow::Outcome(RecognizeOutcome::Done(Err(err))),
    };

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
            Ok(WorkerCommand::Recognize { request, cancel }) => Some(WorkerCommand::Recognize { request, cancel }),
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
                WorkerCommand::Recognize { request, cancel } => BackendRecognizeFlow::Superseded(Box::new(QueuedRecognize { request, cancel })),
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

/// Ensures the app-managed model files a backend OCR `engine` needs (PaddleOCR, MangaOCR
/// ONNX); download events go to the controller and `cancel` stops a download. External
/// models (PaddleOCR-VL, Baberu, EasyOCR, Surya) are only gated (`gate_external_models`), never
/// fetched here; AI API has no local model.
fn ensure_backend_ocr_models(
    engine: OcrEngine,
    options: &OcrRuntimeOptions,
    cancel: &AtomicBool,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<(), ModelEnsureError> {
    let models_root = config::models_dir();
    match engine {
        OcrEngine::PaddleOcr => {
            ensure_with_events(cancel, evt_tx, |ctx| ai_models::ensure_paddle_ocr_full(&models_root, &options.paddle_lang, ctx))?;
        }
        OcrEngine::MangaOcr => {
            if let Some(model) = ai_models::manga_ocr_model_from_key(&options.manga_model) {
                ensure_with_events(cancel, evt_tx, |ctx| ai_models::ensure_manga_ocr_onnx(&models_root, model, ctx))?;
            }
        }
        OcrEngine::EasyOcr | OcrEngine::PaddleVl | OcrEngine::Surya | OcrEngine::Baberu | OcrEngine::AiApi => {}
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

/// Checks that an AI API OCR request can be sent and returns its target: a usable base URL for a
/// compatible service, a stored key for a service that requires one, and a model id.
fn validate_ai_api_options(options: &OcrRuntimeOptions) -> Result<AiApiTarget, String> {
    let service = options.ai_api_service;
    let target = AiApiTarget::new(service, &options.ai_api_base_url).map_err(|err| err.to_string())?;
    if service.requires_key() && ms_ai_api::keys::read_api_key(&target).map_err(|err| err.to_string())?.trim().is_empty() {
        return Err(tf!("translation.ocr.api_key_missing_error", service = service.label()));
    }
    if options.ai_api_model.trim().is_empty() {
        return Err(t!("translation.ocr.no_multimodal_model_error").to_string());
    }
    Ok(target)
}

/// Why a load or recognize step produced no result (AI API request, model download).
#[derive(Debug)]
enum OcrFailure {
    /// The user stopped the request, a newer command superseded it, or the controller was
    /// dropped: published as `RecognizeCancelled` / a cancelled load, never as an error.
    Cancelled,
    /// A real failure; the text is the user-facing error.
    Failed(String),
}

impl From<String> for OcrFailure {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

impl From<ModelEnsureError> for OcrFailure {
    fn from(error: ModelEnsureError) -> Self {
        match error {
            ModelEnsureError::Cancelled => Self::Cancelled,
            ModelEnsureError::Failed(text) => Self::Failed(text),
        }
    }
}

/// Web stub: AI-API OCR runs over `genai` + `tokio`, which are not compiled for
/// the browser build. Returns a clear error instead of a fake recognition.
#[cfg(target_arch = "wasm32")]
fn run_ai_api_ocr_request(
    _request: &OcrRecognizeRequest,
    _page_cache: &mut PageImageCache,
    _generation: &GenerationTracker,
) -> Result<OcrRecognizeResult, OcrFailure> {
    Err(OcrFailure::Failed(t!("translation.ocr.ai_api_web_unavailable_error").to_string()))
}

/// Recognizes the request's crop with the selected AI API model through
/// `ms_ai_api::exec_chat_validated` (tracked streaming: `generation` shows its progress and can
/// stop it; one retry when the cleaned answer is empty, see `ai_api_ocr_validation`).
#[cfg(not(target_arch = "wasm32"))]
fn run_ai_api_ocr_request(
    request: &OcrRecognizeRequest,
    page_cache: &mut PageImageCache,
    generation: &GenerationTracker,
) -> Result<OcrRecognizeResult, OcrFailure> {
    // The controller is gone: skip the crop and the (possibly blocking) keyring read.
    if generation.is_closed() {
        return Err(OcrFailure::Cancelled);
    }
    let target = validate_ai_api_options(&request.options)?;
    let crop_png = crop_image_as_png(request, page_cache)?;
    let service = target.service();
    // Blank for a compatible service without a key stored for this exact base URL: sent as an
    // empty key.
    let api_key = ms_ai_api::keys::read_api_key(&target).map_err(|err| err.to_string())?;
    let model = ms_ai_api::model_id::model_iden(service, &request.options.ai_api_model).map_err(|err| err.to_string())?;
    let system_instruction = normalized_ai_api_system_instruction(&request.options);
    let prompt = if request.reflect_strings {
        "Recognize all visible text in this manga/comic image. Read vertical columns right-to-left when the layout indicates manga reading order. Return only the recognized text, preserving line breaks when they are meaningful."
    } else {
        "Recognize all visible text in this manga/comic image. Return only the recognized text, preserving line breaks when they are meaningful."
    };
    let image_b64 = ms_ai_api::base64_encode(&crop_png);

    let text = ms_ai_api::client::block_on(async move {
        let client = ms_ai_api::client::build_client(&target, api_key);
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
        // Tagged with the request id, so a "Stop" can tell which request it was shown for.
        ms_ai_api::exec_chat_validated(&client, model, chat_req, None, generation, request.request_id, ai_api_ocr_validation).await
    })
    .map_err(|err| tf!("translation.ocr.async_runtime_error", err = err))?;
    let text = match text {
        Ok(validated) => validated.value,
        Err(AiApiError::Cancelled) => return Err(OcrFailure::Cancelled),
        Err(err) => return Err(OcrFailure::Failed(tf!("translation.ocr.ai_api_request_failed_error", err = err.chat_failure_detail()))),
    };

    Ok(ai_api_ocr_result(&text, request.join_newlines))
}

/// Turns a cleaned AI API OCR answer into the published result through the shared
/// `assemble_line_ocr_result`, honouring `join_newlines`. `reflect_strings` is NOT applied
/// here: for this route it is already part of the prompt (the model is asked to read columns
/// right-to-left), so reversing the answer's lines again would undo that order.
#[cfg(not(target_arch = "wasm32"))]
fn ai_api_ocr_result(answer: &str, join_newlines: bool) -> OcrRecognizeResult {
    assemble_line_ocr_result(answer, join_newlines, false)
}

/// Cleans an AI API OCR answer (`strip_think_blocks`, then `clean_plain_text`) and asks for ONE
/// retry when nothing is left. Plain text has no structure to validate, so an empty answer is
/// the only case worth a second paid request: a user-selected crop almost always holds text, and
/// an empty answer usually means the model spent its output on reasoning or wrapped nothing in a
/// fence. If the retry is empty too, the empty result is published as before.
#[cfg(not(target_arch = "wasm32"))]
fn ai_api_ocr_validation(answer: &TrackedAnswer, _attempt: u8) -> Validation<String> {
    let text = clean_plain_text(&strip_think_blocks(&answer.text));
    let repair = text.is_empty().then(|| RepairRequest {
        reason: RetryReason::Empty,
        message: "Your previous answer contained no recognized text. Return only the text visible in the image, without reasoning, markdown or commentary.".to_string(),
    });
    Validation { value: text, repair }
}

/// The instruction actually sent for an AI API OCR request (see
/// `effective_ai_api_ocr_instruction`).
fn normalized_ai_api_system_instruction(options: &OcrRuntimeOptions) -> String {
    effective_ai_api_ocr_instruction(&options.ai_api_system_instruction, &options.ai_api_source_lang)
}

// The default AI API OCR system instruction, in three parts: the language clause sits between
// HEAD and TAIL. These constants are the ONE owner of that text (the OCR panel's initial value
// and the request-time fallback both come from `default_ai_api_ocr_instruction`). Editing them
// changes what `is_default_ai_api_ocr_instruction` recognizes: a stored default written with
// the old text then counts as a user edit and stops following the language.
const AI_API_OCR_INSTRUCTION_HEAD: &str = "You are an OCR engine for manga and comics. Recognize text exactly as it is written";
const AI_API_OCR_INSTRUCTION_LANGUAGE_PREFIX: &str = ", primarily in the following language: ";
const AI_API_OCR_INSTRUCTION_TAIL: &str = ". Pay special attention to the sounds. Do not translate, explain, describe the image, or add captions. Return only the recognized text. If a sound is particularly unclear and you are unsure, list several possible options separated by /";
/// Longest language slot `is_default_ai_api_ocr_instruction` accepts (longer = user prose).
const AI_API_OCR_INSTRUCTION_MAX_LANGUAGE_CHARS: usize = 40;

/// The default AI API OCR system instruction for the OCR input language `source_lang` (the MT
/// panel's source-language code, e.g. `ko`, `ja`, `zh-cn`; the tab's documented OCR input
/// language). `auto` or a blank code names no language: the language clause is left out.
pub(crate) fn default_ai_api_ocr_instruction(source_lang: &str) -> String {
    let lang = source_lang.trim();
    if lang.is_empty() || lang.eq_ignore_ascii_case("auto") {
        format!("{AI_API_OCR_INSTRUCTION_HEAD}{AI_API_OCR_INSTRUCTION_TAIL}")
    } else {
        format!("{AI_API_OCR_INSTRUCTION_HEAD}{AI_API_OCR_INSTRUCTION_LANGUAGE_PREFIX}{lang}{AI_API_OCR_INSTRUCTION_TAIL}")
    }
}

/// Whether `instruction` is an untouched default rather than a user edit: blank, the
/// language-less default, or the default with any short single-line language slot (any
/// language, including the legacy hard-coded `Korean` text older settings stored). Whitespace
/// around the text is ignored.
pub(crate) fn is_default_ai_api_ocr_instruction(instruction: &str) -> bool {
    let text = instruction.trim();
    if text.is_empty() {
        return true;
    }
    let Some(rest) = text.strip_prefix(AI_API_OCR_INSTRUCTION_HEAD) else {
        return false;
    };
    let Some(slot) = rest.strip_suffix(AI_API_OCR_INSTRUCTION_TAIL) else {
        return false;
    };
    if slot.is_empty() {
        return true;
    }
    let Some(language) = slot.strip_prefix(AI_API_OCR_INSTRUCTION_LANGUAGE_PREFIX) else {
        return false;
    };
    let language = language.trim();
    !language.is_empty()
        && language.chars().count() <= AI_API_OCR_INSTRUCTION_MAX_LANGUAGE_CHARS
        && !language.contains(['.', '\n'])
}

/// The instruction to use for the stored (persisted, user-editable) `instruction`: an untouched
/// default (`is_default_ai_api_ocr_instruction`) follows the OCR input language `source_lang`;
/// a user edit is kept as typed (trimmed).
pub(crate) fn effective_ai_api_ocr_instruction(instruction: &str, source_lang: &str) -> String {
    if is_default_ai_api_ocr_instruction(instruction) {
        default_ai_api_ocr_instruction(source_lang)
    } else {
        instruction.trim().to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::{
        failure_state_changed,
        AiApiService, CallError, CharReplacementRule, OcrEngine, OcrRecognizeResult,
        OcrRecognizeRequest, OcrRoute, OcrRuntimeOptions, RecognizeOutcome, apply_char_replacements,
        apply_post_ocr_processing, assemble_line_ocr_result, interpret_call_result,
        ExternalModelDirs, ExternalModelSpec, native_failure_should_surface, ocr_header_fields,
        ocr_route, ocr_route_availability, ocr_route_kind, ocr_route_needs_backend_warmup,
        parse_ocr_response,
    };
    use ms_local_ai::local_ai::{AiRouteKind, LocalAiAvailability, LocalAiOffReason, LocalRuntime};
    use ms_platform::{PlatformFeature, Unavailable, UnavailableReason};
    use ms_sysprobe::ai_models::MangaOcrOnnxModel;
    use ms_backend_ipc::protocol;
    use ms_config::{AiRuntime, OrtLoadDecision};
    use serde_json::{Value, json};

    fn sample_options() -> OcrRuntimeOptions {
        OcrRuntimeOptions {
            manga_model: "base".to_string(),
            paddle_lang: "korean_v5".to_string(),
            paddle_vl_script: "korean".to_string(),
            paddle_vl_model: super::PaddleVlVariant::MangaJa,
            easy_langs: "ko".to_string(),
            surya_task_name: "ocr_without_boxes".to_string(),
            surya_recognize_math: false,
            surya_sort_lines: true,
            surya_drop_repeated_text: false,
            surya_max_sliding_window: 0,
            surya_max_tokens: 128,
            ai_api_service: AiApiService::OpenAi,
            ai_api_base_url: String::new(),
            ai_api_model: "gpt-4o-mini".to_string(),
            ai_api_system_instruction: String::new(),
            ai_api_source_lang: "auto".to_string(),
        }
    }

    /// Builds a minimal post-processing request: only the two post-OCR fields
    /// matter, the transport fields are never read by the helper under test.
    fn sample_post_processing_request(
        char_replacements: Vec<CharReplacementRule>,
        fix_caps_lock: bool,
    ) -> OcrRecognizeRequest {
        OcrRecognizeRequest {
            request_id: 1,
            engine: OcrEngine::MangaOcr,
            options: sample_options(),
            page_path: std::path::PathBuf::from("page.png"),
            uv_rect: [0.0, 0.0, 1.0, 1.0],
            image_override_png: None,
            join_newlines: true,
            reflect_strings: false,
            char_replacements,
            fix_caps_lock,
        }
    }

    #[test]
    fn caps_lock_decision_ignores_lowercase_introduced_by_substitution() {
        // A common OCR repair rule turns a misread zero into a letter. Deciding
        // on the SUBSTITUTED text would see that lowercase `o` and abandon the
        // caps fix, publishing "HELLo WORLD".
        let mut result = OcrRecognizeResult {
            lines: vec!["HELL0 WORLD".to_string()],
            text: "HELL0 WORLD".to_string(),
        };
        let request = sample_post_processing_request(
            vec![CharReplacementRule {
                targets: vec!["0".to_string()],
                replacement: "o".to_string(),
            }],
            true,
        );
        apply_post_ocr_processing(&mut result, &request);
        assert_eq!(result.text, "Hello world");
        assert_eq!(result.lines, vec!["Hello world".to_string()]);
    }

    #[test]
    fn post_processing_lets_substitutions_create_sentence_boundaries() {
        // The rewrite must run AFTER substitutions so a rule that produces real
        // punctuation (the default `…` -> `...`) opens a new sentence.
        let mut result = OcrRecognizeResult {
            lines: vec!["ONE… TWO".to_string()],
            text: "ONE… TWO".to_string(),
        };
        let request = sample_post_processing_request(
            vec![CharReplacementRule {
                targets: vec!["…".to_string()],
                replacement: "...".to_string(),
            }],
            true,
        );
        apply_post_ocr_processing(&mut result, &request);
        assert_eq!(result.text, "One... Two");
    }

    #[test]
    fn post_processing_skips_caps_fix_when_disabled() {
        let mut result = OcrRecognizeResult {
            lines: vec!["HELLO WORLD".to_string()],
            text: "HELLO WORLD".to_string(),
        };
        let request = sample_post_processing_request(Vec::new(), false);
        apply_post_ocr_processing(&mut result, &request);
        assert_eq!(result.text, "HELLO WORLD");
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
        assert_eq!(
            OcrEngine::Baberu.backend_method(),
            Some(protocol::METHOD_OCR_BABERU)
        );
        assert_eq!(OcrEngine::AiApi.backend_method(), None);
        assert!(OcrEngine::Surya.requires_backend());
        assert!(OcrEngine::Baberu.requires_backend());
        assert!(!OcrEngine::AiApi.requires_backend());
    }

    #[test]
    fn ocr_header_fields_carry_params_not_image() {
        let header = ocr_header_fields(&sample_options(), &ExternalModelDirs::default(), false, true)
            .expect("no model paths to convert");
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
        let header_fields = ocr_header_fields(&sample_options(), &ExternalModelDirs::default(), true, false)
            .expect("no model paths to convert");
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
        assert!(!ocr_route_needs_backend_warmup(OcrRoute::NativeBaberu));
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
                OcrEngine::Baberu,
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

    /// Whether the route kind is the backend (the contract the older `ocr_requires_backend`
    /// tests below pin, now read through [`ocr_route_kind`]).
    fn ocr_requires_backend(
        engine: OcrEngine,
        manga_model_key: &str,
        runtime: AiRuntime,
        guard: OrtLoadDecision,
    ) -> bool {
        ocr_route_kind(engine, manga_model_key, runtime, guard) == AiRouteKind::Backend
    }

    /// Only the first failure and the recovery are edges; repeats of either state are not.
    #[test]
    fn failure_state_changes_only_on_edges() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        assert!(!failure_state_changed(&flag, false));
        assert!(failure_state_changed(&flag, true));
        assert!(!failure_state_changed(&flag, true));
        assert!(failure_state_changed(&flag, false));
        assert!(!failure_state_changed(&flag, false));
    }

    const ALL_ENGINES: [OcrEngine; 7] = [
        OcrEngine::MangaOcr,
        OcrEngine::EasyOcr,
        OcrEngine::PaddleOcr,
        OcrEngine::PaddleVl,
        OcrEngine::Surya,
        OcrEngine::AiApi,
        OcrEngine::Baberu,
    ];

    fn platform(on: bool, feature: PlatformFeature) -> Result<(), Unavailable> {
        if on { Ok(()) } else { Err(Unavailable::new(feature, UnavailableReason::NotOnThisPlatform)) }
    }

    /// Every combination of `--no-ai` x backend platform x native platform x engine x
    /// runtime x guard: AI API always runs; a native route needs native allowed, a backend
    /// route needs the backend allowed, and the refusal names the runtime the route needs.
    #[test]
    fn ocr_route_availability_all_combinations() {
        for no_ai in [false, true] {
            for backend_platform in [false, true] {
                for native_platform in [false, true] {
                    let local = LocalAiAvailability::resolve(
                        no_ai,
                        platform(backend_platform, PlatformFeature::PythonBackend),
                        platform(native_platform, PlatformFeature::NativeOnnxRuntime),
                    );
                    for engine in ALL_ENGINES {
                        for model in ["base_onnx", "base_torch"] {
                            for runtime in [AiRuntime::Backend, AiRuntime::Native] {
                                for guard in [OrtLoadDecision::Safe, OrtLoadDecision::Suspect] {
                                    let case = format!(
                                        "no_ai={no_ai} backend={backend_platform} native={native_platform} \
                                         {engine:?} {model} {runtime:?} {guard:?}"
                                    );
                                    let kind = ocr_route_kind(engine, model, runtime, guard);
                                    let result = ocr_route_availability(engine, model, runtime, guard, &local);
                                    match kind {
                                        AiRouteKind::Cloud => {
                                            assert_eq!(engine, OcrEngine::AiApi, "{case}");
                                            assert_eq!(result, Ok(AiRouteKind::Cloud), "{case}");
                                        }
                                        AiRouteKind::Native => {
                                            let allowed = native_platform && !no_ai;
                                            assert_eq!(result.is_ok(), allowed, "{case}");
                                            if let Err(off) = result {
                                                assert_eq!(off.runtime, LocalRuntime::NativeOnnx, "{case}");
                                            }
                                        }
                                        AiRouteKind::Backend => {
                                            let allowed = backend_platform && !no_ai;
                                            assert_eq!(result.is_ok(), allowed, "{case}");
                                            if let Err(off) = result {
                                                assert_eq!(off.runtime, LocalRuntime::PythonBackend, "{case}");
                                                // The platform reason wins over the flag.
                                                let expected = if backend_platform {
                                                    LocalAiOffReason::DisabledByFlag
                                                } else {
                                                    LocalAiOffReason::UnavailableOnPlatform(
                                                        UnavailableReason::NotOnThisPlatform,
                                                    )
                                                };
                                                assert_eq!(off.reason, expected, "{case}");
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// The user's report: under `--no-ai` (and on a platform with neither local runtime)
    /// the AI API OCR still runs.
    #[test]
    fn ai_api_ocr_runs_with_every_local_runtime_off() {
        for local in [
            LocalAiAvailability::resolve(true, Ok(()), Ok(())),
            LocalAiAvailability::resolve(
                false,
                platform(false, PlatformFeature::PythonBackend),
                platform(false, PlatformFeature::NativeOnnxRuntime),
            ),
        ] {
            assert_eq!(
                ocr_route_availability(OcrEngine::AiApi, "", AiRuntime::Native, OrtLoadDecision::Safe, &local),
                Ok(AiRouteKind::Cloud)
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
                let result = assemble_line_ocr_result("  こんにちは  ", join, reflect);
                assert_eq!(result.lines, vec!["こんにちは".to_string()]);
                assert_eq!(result.text, "こんにちは");
            }
        }
    }

    #[test]
    fn assemble_native_result_splits_and_joins_lines() {
        // Blank lines are stripped; join controls the separator.
        let joined = assemble_line_ocr_result("a\n\nb\n", true, false);
        assert_eq!(joined.lines, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(joined.text, "a\nb");

        let spaced = assemble_line_ocr_result("a\n\nb\n", false, false);
        assert_eq!(spaced.lines, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(spaced.text, "a b");
    }

    /// The AI API route honours "keep line breaks" through the shared join (S-006) and never
    /// reverses the answer's lines: right-to-left order is already asked of the model.
    #[test]
    fn ai_api_result_honours_join_newlines_without_reflecting() {
        let kept = super::ai_api_ocr_result("  first\n\n second \nthird\n", true);
        assert_eq!(kept.lines, vec!["first".to_string(), "second".to_string(), "third".to_string()]);
        assert_eq!(kept.text, "first\nsecond\nthird");

        let joined = super::ai_api_ocr_result("  first\n\n second \nthird\n", false);
        assert_eq!(joined.lines, kept.lines);
        assert_eq!(joined.text, "first second third");
    }

    /// The legacy hard-coded instruction older settings stored (S-008): it must still count as
    /// an untouched default and therefore follow the OCR input language.
    const LEGACY_KOREAN_INSTRUCTION: &str = "You are an OCR engine for manga and comics. Recognize text exactly as it is written, primarily in the following language: Korean. Pay special attention to the sounds. Do not translate, explain, describe the image, or add captions. Return only the recognized text. If a sound is particularly unclear and you are unsure, list several possible options separated by /";

    #[test]
    fn default_ai_api_instruction_names_the_source_language_or_none() {
        let ja = super::default_ai_api_ocr_instruction(" ja ");
        assert!(ja.contains("primarily in the following language: ja. Pay"), "{ja}");
        assert_eq!(super::default_ai_api_ocr_instruction("ko"), LEGACY_KOREAN_INSTRUCTION.replace("Korean", "ko"));
        for auto in ["auto", "AUTO", "  "] {
            let text = super::default_ai_api_ocr_instruction(auto);
            assert!(!text.contains("primarily"), "{text}");
            assert!(text.starts_with("You are an OCR engine") && text.ends_with("separated by /"), "{text}");
        }
    }

    #[test]
    fn untouched_ai_api_instruction_follows_the_language_and_a_user_edit_is_kept() {
        // Untouched forms: blank, the legacy Korean text, any generated default.
        for stored in ["", "  \n", LEGACY_KOREAN_INSTRUCTION, &super::default_ai_api_ocr_instruction("auto"), &super::default_ai_api_ocr_instruction("zh-cn")] {
            assert!(super::is_default_ai_api_ocr_instruction(stored), "{stored}");
            assert_eq!(super::effective_ai_api_ocr_instruction(stored, "ja"), super::default_ai_api_ocr_instruction("ja"));
        }
        // User edits: other prose, an appended sentence, a long language slot.
        let appended = format!("{LEGACY_KOREAN_INSTRUCTION} Keep the honorifics.");
        let long_slot = LEGACY_KOREAN_INSTRUCTION.replace("Korean", "Korean, but also read the tiny side notes in the margins");
        for stored in ["Read Japanese text only.", appended.as_str(), long_slot.as_str()] {
            assert!(!super::is_default_ai_api_ocr_instruction(stored), "{stored}");
            assert_eq!(super::effective_ai_api_ocr_instruction(&format!(" {stored} "), "ja"), stored);
        }
    }

    /// The request-time instruction reads the stored text and the runtime's source language.
    #[test]
    fn request_instruction_resolves_from_runtime_options() {
        let options = OcrRuntimeOptions { ai_api_source_lang: "ja".to_string(), ..sample_options() };
        assert_eq!(super::normalized_ai_api_system_instruction(&options), super::default_ai_api_ocr_instruction("ja"));
        let edited = OcrRuntimeOptions { ai_api_system_instruction: "Custom.".to_string(), ..options };
        assert_eq!(super::normalized_ai_api_system_instruction(&edited), "Custom.");
    }

    #[test]
    fn assemble_native_result_reflect_reverses_line_order() {
        let result = assemble_line_ocr_result("first\nsecond\nthird", true, true);
        assert_eq!(
            result.lines,
            vec!["third".to_string(), "second".to_string(), "first".to_string()]
        );
        assert_eq!(result.text, "third\nsecond\nfirst");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn ai_api_ocr_answers_are_cleaned_and_only_an_empty_one_is_retried() {
        use ms_ai_api::{RetryReason, TrackedAnswer};
        let answer = |text: &str| TrackedAnswer { text: text.to_string(), truncated: false };
        let cleaned = super::ai_api_ocr_validation(&answer("<think>reading</think>\n```text\n안녕\n쾅 / 쿵\n```"), 0);
        assert_eq!(cleaned.value, "안녕\n쾅 / 쿵");
        assert_eq!(cleaned.repair, None);
        let empty = super::ai_api_ocr_validation(&answer("<think>the crop is blurry, maybe"), 0);
        assert_eq!(empty.value, "");
        assert_eq!(empty.repair.map(|repair| repair.reason), Some(RetryReason::Empty));
    }

    /// Every engine, spelled through an exhaustive match so a new variant fails to compile
    /// here until the routing matrix below covers it.
    fn all_engines() -> [OcrEngine; 7] {
        let all = [
            OcrEngine::MangaOcr,
            OcrEngine::EasyOcr,
            OcrEngine::PaddleOcr,
            OcrEngine::PaddleVl,
            OcrEngine::Surya,
            OcrEngine::AiApi,
            OcrEngine::Baberu,
        ];
        for engine in all {
            match engine {
                OcrEngine::MangaOcr
                | OcrEngine::EasyOcr
                | OcrEngine::PaddleOcr
                | OcrEngine::PaddleVl
                | OcrEngine::Surya
                | OcrEngine::AiApi
                | OcrEngine::Baberu => {}
            }
        }
        all
    }

    /// The full runtime x guard x engine matrix (MangaOCR with an ONNX export): native only
    /// for the native runtime with a Safe guard, and only for the three native engines.
    #[test]
    fn routing_matrix_covers_every_runtime_guard_and_engine() {
        for runtime in [AiRuntime::Native, AiRuntime::Backend] {
            for guard in [OrtLoadDecision::Safe, OrtLoadDecision::Suspect] {
                for engine in all_engines() {
                    let native = runtime == AiRuntime::Native && guard == OrtLoadDecision::Safe;
                    let expected = match engine {
                        OcrEngine::MangaOcr if native => OcrRoute::NativeManga(MangaOcrOnnxModel::Base),
                        OcrEngine::PaddleOcr if native => OcrRoute::NativePaddle,
                        OcrEngine::Baberu if native => OcrRoute::NativeBaberu,
                        OcrEngine::MangaOcr
                        | OcrEngine::PaddleOcr
                        | OcrEngine::Baberu
                        | OcrEngine::EasyOcr
                        | OcrEngine::PaddleVl
                        | OcrEngine::Surya
                        | OcrEngine::AiApi => OcrRoute::Backend,
                    };
                    assert_eq!(
                        ocr_route(runtime, engine, "base_onnx", guard),
                        expected,
                        "runtime {runtime:?} guard {guard:?} engine {engine:?}"
                    );
                }
            }
        }
    }

    /// Baberu on the native route needs no backend (its AiButton gating must not wait on
    /// backend health); on any backend route it does.
    #[test]
    fn baberu_requires_backend_only_off_the_native_route() {
        assert!(!ocr_requires_backend(OcrEngine::Baberu, "", AiRuntime::Native, OrtLoadDecision::Safe));
        assert!(ocr_requires_backend(OcrEngine::Baberu, "", AiRuntime::Native, OrtLoadDecision::Suspect));
        assert!(ocr_requires_backend(OcrEngine::Baberu, "", AiRuntime::Backend, OrtLoadDecision::Safe));
    }

    #[test]
    fn external_model_specs_map_each_engine_to_its_bundle() {
        use ms_sysprobe::ai_models::easyocr::{EasyOcrScript, easyocr_spec};
        use ms_sysprobe::ai_models::external_catalog::{BABERU_OCR, PaddleVlVariant, SURYA_DETECTION_2025_05_07, SURYA_RECOGNITION_2025_09_23};
        let ids = |specs: Vec<&'static ExternalModelSpec>| specs.iter().map(|spec| spec.id).collect::<Vec<_>>();
        for engine in all_engines() {
            for variant in PaddleVlVariant::ALL {
                let specs = ids(super::external_model_specs(engine, variant, "ko"));
                match engine {
                    OcrEngine::Baberu => assert_eq!(specs, [BABERU_OCR.id]),
                    OcrEngine::PaddleVl => assert_eq!(specs, [variant.spec().id]),
                    OcrEngine::EasyOcr => assert_eq!(specs, [easyocr_spec(EasyOcrScript::Korean).id]),
                    OcrEngine::Surya => assert_eq!(specs, [SURYA_DETECTION_2025_05_07.id, SURYA_RECOGNITION_2025_09_23.id]),
                    OcrEngine::MangaOcr | OcrEngine::PaddleOcr | OcrEngine::AiApi => assert!(specs.is_empty(), "{engine:?}"),
                }
            }
        }
        // The EasyOCR spec follows the script of the (normalized) language list.
        let easy = |langs: &str| ids(super::external_model_specs(OcrEngine::EasyOcr, PaddleVlVariant::MangaJa, langs));
        assert_eq!(easy("en"), [easyocr_spec(EasyOcrScript::English).id]);
        assert_eq!(easy("ja, en"), [easyocr_spec(EasyOcrScript::Japanese).id]);
        assert_eq!(easy("zh"), [easyocr_spec(EasyOcrScript::ChineseSim).id]);
        assert_eq!(easy("de, fr"), [easyocr_spec(EasyOcrScript::Latin).id]);
        // A list easyocr refuses has no spec (the gate reports the language error instead).
        assert!(easy("ko, ja").is_empty());
        assert!(easy("xx").is_empty());
    }

    /// Every EasyOCR language-list error maps to its own localized message, carrying only
    /// codes and the recognizer file name.
    #[test]
    fn easyocr_language_errors_are_localized_per_variant() {
        use ms_sysprobe::ai_models::easyocr::{EasyOcrLangError, EasyOcrScript};
        assert_eq!(super::easyocr_lang_error_message(&EasyOcrLangError::Empty), t!("translation.ocr.easyocr_no_language_error"));
        let unsupported = EasyOcrLangError::Unsupported { codes: vec!["xx".to_owned(), "yy".to_owned()] };
        assert_eq!(
            super::easyocr_lang_error_message(&unsupported),
            tf!("translation.ocr.easyocr_unsupported_language_error", codes = "xx, yy")
        );
        let incompatible = super::easyocr_selection("ko, ja").expect_err("korean + japanese is refused by easyocr");
        assert!(matches!(incompatible, EasyOcrLangError::Incompatible { script: EasyOcrScript::Japanese, .. }), "{incompatible:?}");
        assert_eq!(
            super::easyocr_lang_error_message(&incompatible),
            tf!("translation.ocr.easyocr_incompatible_languages_error", model = EasyOcrScript::Japanese.recognizer_file(), codes = "ja, en")
        );
        assert_eq!(super::easyocr_selection(" KOR , en,ko"), Ok((vec!["ko".to_owned(), "en".to_owned()], EasyOcrScript::Korean)));
    }

    #[test]
    fn header_carries_resolved_model_paths_and_variant_key() {
        use ms_sysprobe::ai_models::external_catalog::baberu_files;
        let dirs = ExternalModelDirs {
            paddle_vl: Some(std::path::PathBuf::from("/models/side_models/PaddleOCR-VL/manga_ja")),
            baberu: Some(baberu_files(std::path::Path::new("/models/side_models/BaberuOCR"))),
            easyocr: Some(std::path::PathBuf::from("/models/side_models/EasyOCR")),
            surya_recognition: Some(std::path::PathBuf::from("/models/side_models/Surya/text_recognition/2025_09_23")),
            surya_detection: Some(std::path::PathBuf::from("/models/side_models/Surya/text_detection/2025_05_07")),
        };
        let header = ocr_header_fields(&sample_options(), &dirs, true, false).expect("utf-8 paths");
        assert_eq!(header["paddle_vl_model"], json!("manga_ja"));
        assert_eq!(header["paddle_vl_model_dir"], json!("/models/side_models/PaddleOCR-VL/manga_ja"));
        let files = &header["baberu_model_files"];
        let expected = baberu_files(std::path::Path::new("/models/side_models/BaberuOCR"));
        assert_eq!(files["vision"], json!(expected.vision.to_str().expect("utf-8")));
        assert_eq!(files["prefill"], json!(expected.prefill.to_str().expect("utf-8")));
        assert_eq!(files["step"], json!(expected.step.to_str().expect("utf-8")));
        assert_eq!(files["vocab"], json!(expected.vocab.to_str().expect("utf-8")));
        // The v12 directories under the protocol mirror's key names.
        assert_eq!(header[protocol::HEADER_EASYOCR_MODEL_DIR], json!("/models/side_models/EasyOCR"));
        assert_eq!(header[protocol::HEADER_SURYA_RECOGNITION_DIR], json!("/models/side_models/Surya/text_recognition/2025_09_23"));
        assert_eq!(header[protocol::HEADER_SURYA_DETECTION_DIR], json!("/models/side_models/Surya/text_detection/2025_05_07"));

        // Without resolved models every field is an explicit null (the backend ignores them
        // for other methods; the methods requiring them never get here unresolved).
        let bare = ocr_header_fields(&sample_options(), &ExternalModelDirs::default(), true, false).expect("no paths");
        for key in [
            "paddle_vl_model_dir",
            "baberu_model_files",
            protocol::HEADER_EASYOCR_MODEL_DIR,
            protocol::HEADER_SURYA_RECOGNITION_DIR,
            protocol::HEADER_SURYA_DETECTION_DIR,
        ] {
            assert_eq!(bare[key], Value::Null, "{key}");
        }
        assert_eq!(bare["paddle_vl_model"], json!("manga_ja"));
    }

    /// `easy_langs` goes out canonical (aliases mapped, trimmed, deduplicated), because the
    /// backend no longer normalizes it (v12).
    #[test]
    fn header_sends_canonical_easyocr_languages() {
        let options = super::OcrRuntimeOptions { easy_langs: " Kor, EN , jp, ko".to_string(), ..sample_options() };
        let header = ocr_header_fields(&options, &ExternalModelDirs::default(), true, false).expect("no paths");
        assert_eq!(header[protocol::HEADER_EASY_LANGS], json!("ko,en,ja"));
        let empty = super::OcrRuntimeOptions { easy_langs: " , ".to_string(), ..sample_options() };
        let header = ocr_header_fields(&empty, &ExternalModelDirs::default(), true, false).expect("no paths");
        assert_eq!(header[protocol::HEADER_EASY_LANGS], json!("ko"));
    }

    /// A model path that is not UTF-8 must be rejected, never sent lossily.
    #[cfg(unix)]
    #[test]
    fn non_utf8_model_path_is_a_header_error() {
        use std::os::unix::ffi::OsStrExt;
        let bad = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"/models/\xff\xfe"));
        let dirs = ExternalModelDirs { paddle_vl: Some(bad.clone()), ..ExternalModelDirs::default() };
        assert!(ocr_header_fields(&sample_options(), &dirs, true, false).is_err());
        let dirs = ExternalModelDirs { surya_detection: Some(bad), ..ExternalModelDirs::default() };
        assert!(ocr_header_fields(&sample_options(), &dirs, true, false).is_err());
    }

    /// The "not downloaded" refusal the gate builds for `specs`, all missing.
    fn not_downloaded(specs: &[&'static ExternalModelSpec]) -> super::GateRefusal {
        let statuses: Vec<(&ExternalModelSpec, super::ExternalModelStatus)> =
            specs.iter().map(|spec| (*spec, super::ExternalModelStatus::Missing)).collect();
        let button = crate::ocr_model_download::download_label_for_statuses(&statuses);
        super::GateRefusal { message: tf!("translation.ocr.model_not_downloaded_error", button = button), missing: specs.to_vec() }
    }

    /// The gate on a fresh (empty) `side_models` root: every external-model engine (Baberu,
    /// each PaddleOCR-VL variant, EasyOCR, Surya) fails with the localized "not downloaded"
    /// error naming the download button of its whole bundle; engines without an external
    /// model pass with empty dirs; an EasyOCR list easyocr refuses fails with the language
    /// error and no bundle. Stat-only, nothing is written.
    #[test]
    fn gate_reports_not_downloaded_on_an_empty_root() {
        use ms_sysprobe::ai_models::easyocr::{EasyOcrScript, easyocr_spec};
        use ms_sysprobe::ai_models::external_catalog::{BABERU_OCR, PaddleVlVariant};
        let root = std::env::temp_dir().join(format!("ms_tab_translation_gate_{}_absent", std::process::id()));
        assert!(!root.exists(), "the probe root must not exist: {}", root.display());
        let resolve = |engine, options: &super::OcrRuntimeOptions| super::resolve_external_model_dirs_in(&root, engine, options);
        assert_eq!(resolve(OcrEngine::Baberu, &sample_options()), Err(not_downloaded(&[&BABERU_OCR])));
        for variant in PaddleVlVariant::ALL {
            let options = super::OcrRuntimeOptions { paddle_vl_model: variant, ..sample_options() };
            assert_eq!(resolve(OcrEngine::PaddleVl, &options), Err(not_downloaded(&[variant.spec()])), "{variant:?}");
        }
        assert_eq!(resolve(OcrEngine::EasyOcr, &sample_options()), Err(not_downloaded(&[easyocr_spec(EasyOcrScript::Korean)])));
        assert_eq!(resolve(OcrEngine::Surya, &sample_options()), Err(not_downloaded(&super::SURYA_OCR_SPECS)));
        let bad_langs = super::OcrRuntimeOptions { easy_langs: "ko, ja".to_string(), ..sample_options() };
        let refusal = resolve(OcrEngine::EasyOcr, &bad_langs).expect_err("korean + japanese is refused");
        assert!(refusal.missing.is_empty());
        let lang_err = super::easyocr_selection("ko, ja").expect_err("refused");
        assert_eq!(refusal.message, super::easyocr_lang_error_message(&lang_err));
        for engine in [OcrEngine::MangaOcr, OcrEngine::PaddleOcr, OcrEngine::AiApi] {
            assert_eq!(resolve(engine, &sample_options()), Ok(ExternalModelDirs::default()), "{engine:?}");
        }
        assert!(!root.exists(), "the gate must not create anything");
    }

    /// M1: a gate miss reports WHICH bundle is missing (so the tab re-probes the panel) before
    /// the caller publishes the error; a pass, an engine without an external model, or a
    /// language-list refusal reports nothing.
    #[test]
    fn gate_miss_reports_the_missing_spec() {
        use ms_sysprobe::ai_models::external_catalog::{BABERU_OCR, PaddleVlVariant};
        let root = std::env::temp_dir().join(format!("ms_tab_translation_gate_report_{}_absent", std::process::id()));
        assert!(!root.exists(), "the probe root must not exist: {}", root.display());
        let (tx, rx) = std::sync::mpsc::channel();
        let reported = |rx: &std::sync::mpsc::Receiver<super::WorkerEvent>| match rx.try_recv() {
            Ok(super::WorkerEvent::ExternalModelMissing(specs)) => Some(crate::ocr_model_download::bundle_key(&specs)),
            Ok(_) | Err(_) => None,
        };
        assert!(super::gate_external_models_in(&root, OcrEngine::Baberu, &sample_options(), &tx).is_err());
        assert_eq!(reported(&rx), Some(BABERU_OCR.id.to_owned()));
        let options = super::OcrRuntimeOptions { paddle_vl_model: PaddleVlVariant::MangaJa, ..sample_options() };
        assert!(super::gate_external_models_in(&root, OcrEngine::PaddleVl, &options, &tx).is_err());
        assert_eq!(reported(&rx), Some(PaddleVlVariant::MangaJa.spec().id.to_owned()));
        assert!(super::gate_external_models_in(&root, OcrEngine::Surya, &sample_options(), &tx).is_err());
        assert_eq!(reported(&rx), Some(crate::ocr_model_download::bundle_key(&super::SURYA_OCR_SPECS)));
        let bad_langs = super::OcrRuntimeOptions { easy_langs: "xx".to_string(), ..sample_options() };
        assert!(super::gate_external_models_in(&root, OcrEngine::EasyOcr, &bad_langs, &tx).is_err());
        assert!(rx.try_recv().is_err(), "a language refusal reports no bundle");
        assert!(super::gate_external_models_in(&root, OcrEngine::MangaOcr, &sample_options(), &tx).is_ok());
        assert!(rx.try_recv().is_err(), "a pass reports nothing");
        assert!(!root.exists(), "the gate must not create anything");
    }

    /// A controller over in-test channels instead of a worker thread: the test plays the
    /// worker (reads the commands, sends the events).
    fn controller_with_fake_worker() -> (super::TranslationOcrController, std::sync::mpsc::Sender<super::WorkerEvent>, std::sync::mpsc::Receiver<super::WorkerCommand>) {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let controller = super::TranslationOcrController {
            state: super::OcrLoadState::NotLoaded,
            last_error: None,
            last_result: None,
            pending_request: None,
            cmd_tx,
            evt_rx,
            worker_thread: None,
            generation: super::GenerationTracker::new(),
            command_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        (controller, evt_tx, cmd_rx)
    }

    fn download_event() -> ms_sysprobe::ai_models::ModelDownloadEvent {
        ms_sysprobe::ai_models::ModelDownloadEvent {
            file: "ONNX/MangaOCR/base/encoder_model.onnx".to_string(),
            file_index: 0,
            file_count: 6,
            phase: ms_sysprobe::ai_models::ModelDownloadPhase::Downloading,
            staged: 1024,
            total: Some(4096),
        }
    }

    /// The panel's Cancel reaches the running command's token only while a model download
    /// runs; in any other state it is a no-op.
    #[test]
    fn cancel_model_download_sets_the_running_commands_token_only_while_downloading() {
        let (mut controller, evt_tx, cmd_rx) = controller_with_fake_worker();
        controller.request_recognize(sample_post_processing_request(Vec::new(), false));
        let Ok(super::WorkerCommand::Load { cancel, .. }) = cmd_rx.try_recv() else {
            panic!("the request must start a load");
        };
        controller.cancel_model_download();
        assert!(!cancel.load(std::sync::atomic::Ordering::Relaxed), "a plain load is not cancelled");
        evt_tx.send(super::WorkerEvent::ModelDownloadProgress(download_event())).expect("the controller listens");
        let _ = controller.poll_events();
        controller.cancel_model_download();
        assert!(cancel.load(std::sync::atomic::Ordering::Relaxed), "the download of the load must stop");
    }

    /// A load whose model download is cancelled ends neutrally: back to `NotLoaded`, and the
    /// request that waited for it is reported as cancelled — never as an error. Download events
    /// move the state to `DownloadingModel` and reach the tab.
    #[test]
    fn a_cancelled_model_download_during_load_ends_as_cancelled_not_as_an_error() {
        let (mut controller, evt_tx, cmd_rx) = controller_with_fake_worker();
        controller.request_recognize(sample_post_processing_request(Vec::new(), false));
        assert!(matches!(cmd_rx.try_recv(), Ok(super::WorkerCommand::Load { .. })));

        evt_tx.send(super::WorkerEvent::ModelDownloadProgress(download_event())).expect("the controller listens");
        let events = controller.poll_events();
        assert!(matches!(events.first(), Some(super::OcrControllerEvent::StateChanged(super::OcrLoadState::DownloadingModel))));
        assert!(matches!(events.get(1), Some(super::OcrControllerEvent::ModelDownloadProgress(event)) if event.staged == 1024));

        evt_tx.send(super::WorkerEvent::LoadCancelled).expect("the controller listens");
        let events = controller.poll_events();
        assert!(matches!(events.first(), Some(super::OcrControllerEvent::StateChanged(super::OcrLoadState::NotLoaded))));
        assert!(matches!(events.get(1), Some(super::OcrControllerEvent::RecognizeCancelled { request_id: 1 })));
        assert_eq!(events.len(), 2);
        assert_eq!(controller.state(), super::OcrLoadState::NotLoaded);
        assert!(controller.last_error().is_none());
    }

    /// Every Load / Recognize carries its own cancel token: a newer command sets the previous
    /// one (its model download stops) and dropping the controller sets the newest.
    #[test]
    fn a_newer_command_and_the_drop_cancel_the_previous_download() {
        let (mut controller, _evt_tx, cmd_rx) = controller_with_fake_worker();
        controller.state = super::OcrLoadState::Ready;
        controller.request_recognize(sample_post_processing_request(Vec::new(), false));
        controller.request_recognize(sample_post_processing_request(Vec::new(), false));
        let token = |command| match command {
            Ok(super::WorkerCommand::Recognize { cancel, .. }) => cancel,
            other => panic!("expected a Recognize, got {other:?}"),
        };
        let first = token(cmd_rx.try_recv());
        let second = token(cmd_rx.try_recv());
        assert!(first.load(std::sync::atomic::Ordering::Relaxed), "superseded");
        assert!(!second.load(std::sync::atomic::Ordering::Relaxed));
        drop(controller);
        assert!(second.load(std::sync::atomic::Ordering::Relaxed), "the drop stops the newest download");
    }
}
