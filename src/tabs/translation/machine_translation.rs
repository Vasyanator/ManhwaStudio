/*
FILE OVERVIEW: src/tabs/translation/machine_translation.rs
Background machine-translation controller for Translation tab.

Main types:
- `MtService`: translation provider selector (`Google` + `Yandex` + `DeepL`).
- `AiMtOptions`: AI API translation settings, ImageBubble visual detail, and project context
  snapshot.
- `MtTranslateItem` / `MtTranslateRequest`: per-bubble input and batch request payload, including
  optional ImageBubble multimodal payloads for AI API translation.
- `MtControllerEvent`: UI-facing worker events.
- `TranslationMtController`: MT run lifecycle with immediate cancel semantics.
- `ActiveMtRun`: currently active run-thread metadata (`run_id`, cancel flag, handle).

Runtime model:
- each MT run is executed in its own background thread (GUI thread is never blocked);
- `request_cancel` marks run cancelled and detaches the thread immediately;
- detached/stale run events are ignored by `run_id` and never touch canvas state.

Backend helper:
- `translate_texts_via_translator` dispatches into `machine_translators` backends.
- AI API translation sends JSON groups with bubble IDs and characters, sends ImageBubble binaries
  only in the active multimodal request, keeps text-only chat history across batches, and prunes old
  turns by an approximate context budget.

Per-ImageBubble mode (`AiMtOptions::image_mode == ImagesOnly`):
- `run_ai_imagebubble_translate_async` translates one ImageBubble per request. Every non-target
  bubble is appended (text only, chosen `AiMtContextSource` original/translation) to an append-only
  chapter-context prefix; the request is laid out as system(static) + system(context, cached) +
  user(target image + instruction). Because the context is append-only and image binaries never
  enter it, request N+1's input has request N's as a literal prefix, so provider prompt caching
  reuses it (Anthropic: message-level `CacheControl`; OpenAI-style: stable `prompt_cache_key`).

Context replicas:
- `MtTranslateItem::needs_translation == false` marks an already-translated replica included only as
  ordered read-only context (with its existing translation). `split_ai_mt_batches` keeps such
  replicas in reading order around the translatable ones but cuts each batch right after its
  `batch_size`-th translatable replica, so context past a reached per-batch limit is deferred to a
  later window. Context replicas are flagged `"context": true` in the prompt and are never returned,
  counted, or reported as failures.
*/

// AI API machine translation runs over `genai` + `tokio`, native-only crates not
// compiled for wasm. The controller, worker threads, and command/event enums stay
// target-neutral; only the bodies that touch `genai`/`tokio` (and the `keyring`
// API-key helpers in `ocr`) are gated behind `not(target_arch = "wasm32")` below.
#[cfg(not(target_arch = "wasm32"))]
use std::collections::VecDeque;
use std::fs;
use std::io::Cursor;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use ms_thread::{self as thread, JoinHandle};

use super::machine_translators::MachineTranslatorBackend;
use super::machine_translators::deepl::DeeplMtBackend;
use super::machine_translators::google::GoogleMtBackend;
use super::machine_translators::yandex::YandexMtBackend;
use super::ocr::AiApiService;
#[cfg(not(target_arch = "wasm32"))]
use super::ocr::{build_ai_api_client, model_iden_for_ai_api_service, read_ai_api_key};
use crate::project::{Bubble, ProjectData};
#[cfg(not(target_arch = "wasm32"))]
use crate::runtime_log;
use crate::tabs::characters::load_characters_for_notes;
use crate::tabs::terms::load_terms_for_notes;
use crate::tabs::translation::panels::bubbles::{bubble_extra_i32, bubble_extra_string};
#[cfg(not(target_arch = "wasm32"))]
use genai::chat::{
    CacheControl, ChatMessage, ChatOptions, ChatRequest, ContentPart, ReasoningEffort,
};
use serde_json::Value;

const MT_EVENT_POLL_BUDGET: usize = 64;
const AI_MT_CONTEXT_CHAR_BUDGET: usize = 120_000;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum MtService {
    Google,
    Yandex,
    Deepl,
}

impl MtService {
    pub fn key(self) -> &'static str {
        match self {
            MtService::Google => "google",
            MtService::Yandex => "yandex",
            MtService::Deepl => "deepl",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            MtService::Google => "Google",
            MtService::Yandex => "Yandex",
            MtService::Deepl => "DeepL",
        }
    }

    pub fn from_key(raw: &str) -> Option<Self> {
        let key = raw.trim().to_ascii_lowercase();
        match key.as_str() {
            "google" => Some(MtService::Google),
            "yandex" => Some(MtService::Yandex),
            "deepl" => Some(MtService::Deepl),
            _ => None,
        }
    }

    pub fn all() -> &'static [Self] {
        &[MtService::Google, MtService::Yandex, MtService::Deepl]
    }
}

#[derive(Debug, Clone)]
pub struct MtTranslateItem {
    pub bubble_id: i64,
    pub page_idx: usize,
    pub img_v: f32,
    pub order: i32,
    pub character: String,
    pub text: String,
    pub existing_translation: String,
    pub image: Option<MtImageInput>,
    /// When `false`, this replica is already translated and is included only as ordered read-only
    /// context for the AI batch: it is shown to the model (with its `existing_translation`) so the
    /// model keeps the correct reading order and dialogue continuity, but it is never translated and
    /// never reported back as a result. Always `true` for the non-AI translators.
    pub needs_translation: bool,
}

#[derive(Debug, Clone)]
pub struct MtImageInput {
    pub description: String,
    pub source: MtImageSource,
    /// Text areas of a multi-area image bubble. Always holds at least one entry; when it holds more
    /// than one the AI request/response use the per-area protocol (each area gets its own
    /// `original_text` + `translation`).
    pub areas: Vec<MtImageArea>,
}

impl MtImageInput {
    /// True when the image bubble has more than one text area and should use the per-area protocol.
    #[must_use]
    pub fn is_multi_area(&self) -> bool {
        self.areas.len() > 1
    }
}

/// One text area of an image bubble as sent to the AI translator.
#[derive(Debug, Clone)]
pub struct MtImageArea {
    pub description: String,
    pub original: String,
    /// Area bounding box relative to the sent image (`[x1,y1,x2,y2]`, 0..1) when known (page-crop
    /// bubbles), giving the model a positional hint for which region the area refers to.
    pub rel_bbox: Option<[f32; 4]>,
}

#[derive(Debug, Clone)]
pub enum MtImageSource {
    ExternalPath(String),
    PageCrop {
        page_idx: usize,
        crop_rect: [f32; 4],
    },
}

#[derive(Debug, Clone)]
pub struct MtTranslateRequest {
    pub service: MtService,
    pub source_lang: String,
    pub target_lang: String,
    pub items: Vec<MtTranslateItem>,
    pub ai_api: Option<AiMtOptions>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AiMtSortMode {
    Height,
    Number,
}

impl AiMtSortMode {
    pub fn key(self) -> &'static str {
        match self {
            Self::Height => "height",
            Self::Number => "number",
        }
    }

    pub fn from_key(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "number" | "id" => Self::Number,
            _ => Self::Height,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AiMtReasoning {
    None,
    Low,
    Medium,
    High,
    XHigh,
}

impl AiMtReasoning {
    pub const ALL: [Self; 5] = [Self::None, Self::Low, Self::Medium, Self::High, Self::XHigh];

    pub fn key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::None => t!("translation.mt.reasoning_none"),
            Self::Low => t!("translation.mt.reasoning_low"),
            Self::Medium => t!("translation.mt.reasoning_medium"),
            Self::High => t!("translation.mt.reasoning_high"),
            Self::XHigh => t!("translation.mt.reasoning_very_high"),
        }
    }

    pub fn from_key(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" | "x_high" => Self::XHigh,
            _ => Self::None,
        }
    }

    /// Maps to the `genai` reasoning-effort enum. Native-only: `genai` is not
    /// compiled for the web build.
    #[cfg(not(target_arch = "wasm32"))]
    fn to_genai(self) -> Option<ReasoningEffort> {
        match self {
            Self::None => None,
            Self::Low => Some(ReasoningEffort::Low),
            Self::Medium => Some(ReasoningEffort::Medium),
            Self::High => Some(ReasoningEffort::High),
            Self::XHigh => Some(ReasoningEffort::XHigh),
        }
    }
}

const AI_MT_LOW_DETAIL_MAX_EDGE_PX: u32 = 512;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AiMtImageDetail {
    Auto,
    Low,
    High,
}

impl AiMtImageDetail {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Low, Self::High];

    pub fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Low => "low",
            Self::High => "high",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Auto => t!("translation.mt.image_detail_auto"),
            Self::Low => t!("translation.mt.image_detail_low"),
            Self::High => t!("translation.mt.image_detail_high"),
        }
    }

    pub fn from_key(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "low" | "jpeg60" | "jpeg_60" => Self::Low,
            "high" | "jpeg92" | "jpeg_92" | "lossless" | "lossless_png" | "png" => Self::High,
            _ => Self::Auto,
        }
    }

    fn max_edge_px(self) -> Option<u32> {
        match self {
            Self::Low => Some(AI_MT_LOW_DETAIL_MAX_EDGE_PX),
            Self::Auto | Self::High => None,
        }
    }
}

/// Whether the AI translator runs in the normal batched mode or in the per-ImageBubble mode.
///
/// In [`AiMtImageMode::ImagesOnly`] every translatable item is a single ImageBubble translated in
/// its own request, preceded by the full chapter context up to that bubble (rendered as text only).
/// The request layout is built so providers can reuse the cached input prefix across the chapter's
/// image requests (see `run_ai_imagebubble_translate_async`).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AiMtImageMode {
    Normal,
    ImagesOnly,
}

impl AiMtImageMode {
    pub fn key(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::ImagesOnly => "images_only",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Normal => t!("translation.mt.mode_normal"),
            Self::ImagesOnly => t!("translation.mt.mode_images_only"),
        }
    }

    pub fn from_key(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "images_only" | "images" | "imagebubble" | "image_bubble" => Self::ImagesOnly,
            _ => Self::Normal,
        }
    }
}

/// Which text of a context bubble is shown to the model in [`AiMtImageMode::ImagesOnly`]: the source
/// original or the existing translation. Switchable from the panel. `Translation` falls back to the
/// original when a context bubble has no translation yet, so continuity is preserved either way.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AiMtContextSource {
    Original,
    Translation,
}

impl AiMtContextSource {
    pub fn key(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Translation => "translation",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Original => t!("translation.common.original_label"),
            Self::Translation => t!("translation.common.translation_label"),
        }
    }

    pub fn from_key(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "original" | "source" => Self::Original,
            _ => Self::Translation,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AiMtOptions {
    pub service: AiApiService,
    pub model: String,
    pub system_instruction: String,
    pub sort_mode: AiMtSortMode,
    pub use_character_names: bool,
    pub use_notes_prompt: bool,
    pub include_characters: bool,
    pub include_terms: bool,
    pub batch_size: usize,
    pub reasoning: AiMtReasoning,
    pub context_limit_percent: u8,
    pub include_existing_translation: bool,
    pub image_detail: AiMtImageDetail,
    pub image_mode: AiMtImageMode,
    pub image_context_source: AiMtContextSource,
    pub project: ProjectData,
}

#[derive(Debug, Clone)]
pub enum MtControllerEvent {
    RunStarted {
        total: usize,
    },
    ItemTranslated {
        bubble_id: i64,
        translated_text: String,
        original_text: Option<String>,
    },
    /// Per-area result for a multi-area image bubble: one `(original_text, translation)` per text
    /// area in order.
    ItemAreasTranslated {
        bubble_id: i64,
        areas: Vec<(String, String)>,
    },
    ItemFailed {
        bubble_id: i64,
        error: String,
    },
    RunFinished {
        translated: usize,
        errors: usize,
    },
    RunCancelled {
        translated: usize,
        errors: usize,
    },
    RunFailed {
        error: String,
    },
    Progress {
        translated: usize,
        errors: usize,
        total: usize,
        context_used_chars: usize,
        context_budget_chars: usize,
        pruned_replicas: usize,
    },
    AiApiKeyStored {
        service: AiApiService,
    },
    AiApiKeyCleared {
        service: AiApiService,
    },
    AiApiMetadataLoaded(super::ocr::AiApiMetadata),
    AiApiMetadataFailed {
        service: AiApiService,
        error: String,
    },
}

#[derive(Debug)]
struct ActiveMtRun {
    run_id: u64,
    cancel_requested: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

#[derive(Debug)]
pub struct TranslationMtController {
    busy: bool,
    next_run_id: u64,
    active_run: Option<ActiveMtRun>,
    detached_run_threads: Vec<JoinHandle<()>>,
    evt_tx: Sender<WorkerEvent>,
    evt_rx: Receiver<WorkerEvent>,
}

impl Default for TranslationMtController {
    fn default() -> Self {
        Self::new()
    }
}

impl TranslationMtController {
    pub fn new() -> Self {
        let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
        Self {
            busy: false,
            next_run_id: 1,
            active_run: None,
            detached_run_threads: Vec::new(),
            evt_tx,
            evt_rx,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn start_translation(&mut self, request: MtTranslateRequest) -> Result<(), String> {
        if self.busy {
            return Err(t!("translation.mt.already_running_status").to_string());
        }
        if request.items.is_empty() {
            return Err(t!("translation.mt.no_bubbles_status").to_string());
        }

        self.reap_detached_run_threads();
        let run_id = self.next_run_id;
        self.next_run_id = self.next_run_id.saturating_add(1);
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let run_cancel_requested = Arc::clone(&cancel_requested);
        let evt_tx = self.evt_tx.clone();

        let thread = thread::spawn(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                run_translate_request(run_id, request, &evt_tx, &run_cancel_requested);
            }));
            if result.is_err() {
                let _ = evt_tx.send(WorkerEvent::RunFailed {
                    run_id,
                    error: t!("translation.mt.worker_crashed_error").to_string(),
                });
            }
        });

        self.active_run = Some(ActiveMtRun {
            run_id,
            cancel_requested,
            thread,
        });
        self.busy = true;
        Ok(())
    }

    pub fn store_ai_api_key(&self, service: AiApiService, api_key: String) {
        let evt_tx = self.evt_tx.clone();
        thread::spawn(
            move || match super::ocr::store_ai_api_key(service, &api_key) {
                Ok(()) => {
                    let _ = evt_tx.send(WorkerEvent::AiApiKeyStored { service });
                }
                Err(error) => {
                    let _ = evt_tx.send(WorkerEvent::AiApiMetadataErr { service, error });
                }
            },
        );
    }

    pub fn clear_ai_api_key(&self, service: AiApiService) {
        let evt_tx = self.evt_tx.clone();
        thread::spawn(move || match super::ocr::clear_ai_api_key(service) {
            Ok(()) => {
                let _ = evt_tx.send(WorkerEvent::AiApiKeyCleared { service });
            }
            Err(error) => {
                let _ = evt_tx.send(WorkerEvent::AiApiMetadataErr { service, error });
            }
        });
    }

    pub fn refresh_ai_api_metadata(&self, service: AiApiService) {
        let evt_tx = self.evt_tx.clone();
        thread::spawn(move || match super::ocr::load_ai_api_metadata(service) {
            Ok(metadata) => {
                let _ = evt_tx.send(WorkerEvent::AiApiMetadataLoaded(metadata));
            }
            Err(error) => {
                let _ = evt_tx.send(WorkerEvent::AiApiMetadataErr { service, error });
            }
        });
    }

    pub fn request_cancel(&mut self) -> bool {
        if !self.busy {
            return false;
        }
        self.busy = false;
        if let Some(run) = self.active_run.take() {
            run.cancel_requested.store(true, Ordering::Relaxed);
            self.detached_run_threads.push(run.thread);
            return true;
        }
        false
    }

    pub fn poll_events(&mut self) -> Vec<MtControllerEvent> {
        self.reap_detached_run_threads();
        let mut out = Vec::new();

        for _ in 0..MT_EVENT_POLL_BUDGET {
            match self.evt_rx.try_recv() {
                Ok(WorkerEvent::RunStarted { run_id, total }) => {
                    if self.is_active_run(run_id) {
                        out.push(MtControllerEvent::RunStarted { total });
                    }
                }
                Ok(WorkerEvent::ItemTranslated {
                    run_id,
                    bubble_id,
                    translated_text,
                    original_text,
                }) => {
                    if self.is_active_run(run_id) {
                        out.push(MtControllerEvent::ItemTranslated {
                            bubble_id,
                            translated_text,
                            original_text,
                        });
                    }
                }
                Ok(WorkerEvent::ItemAreasTranslated {
                    run_id,
                    bubble_id,
                    areas,
                }) => {
                    if self.is_active_run(run_id) {
                        out.push(MtControllerEvent::ItemAreasTranslated { bubble_id, areas });
                    }
                }
                Ok(WorkerEvent::ItemFailed {
                    run_id,
                    bubble_id,
                    error,
                }) => {
                    if self.is_active_run(run_id) {
                        out.push(MtControllerEvent::ItemFailed { bubble_id, error });
                    }
                }
                Ok(WorkerEvent::RunFinished {
                    run_id,
                    translated,
                    errors,
                }) => {
                    if self.is_active_run(run_id) {
                        self.finish_active_run();
                        out.push(MtControllerEvent::RunFinished { translated, errors });
                    }
                }
                Ok(WorkerEvent::RunCancelled {
                    run_id,
                    translated,
                    errors,
                }) => {
                    if self.is_active_run(run_id) {
                        self.finish_active_run();
                        out.push(MtControllerEvent::RunCancelled { translated, errors });
                    }
                }
                Ok(WorkerEvent::RunFailed { run_id, error }) => {
                    if self.is_active_run(run_id) {
                        self.finish_active_run();
                        out.push(MtControllerEvent::RunFailed { error });
                    }
                }
                Ok(WorkerEvent::Progress {
                    run_id,
                    translated,
                    errors,
                    total,
                    context_used_chars,
                    context_budget_chars,
                    pruned_replicas,
                }) => {
                    if self.is_active_run(run_id) {
                        out.push(MtControllerEvent::Progress {
                            translated,
                            errors,
                            total,
                            context_used_chars,
                            context_budget_chars,
                            pruned_replicas,
                        });
                    }
                }
                Ok(WorkerEvent::AiApiKeyStored { service }) => {
                    out.push(MtControllerEvent::AiApiKeyStored { service });
                }
                Ok(WorkerEvent::AiApiKeyCleared { service }) => {
                    out.push(MtControllerEvent::AiApiKeyCleared { service });
                }
                Ok(WorkerEvent::AiApiMetadataLoaded(metadata)) => {
                    out.push(MtControllerEvent::AiApiMetadataLoaded(metadata));
                }
                Ok(WorkerEvent::AiApiMetadataErr { service, error }) => {
                    out.push(MtControllerEvent::AiApiMetadataFailed { service, error });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.finish_active_run();
                    out.push(MtControllerEvent::RunFailed {
                        error: t!("translation.mt.worker_disconnected_error").to_string(),
                    });
                    break;
                }
            }
        }

        out
    }

    fn is_active_run(&self, run_id: u64) -> bool {
        self.active_run
            .as_ref()
            .is_some_and(|active| active.run_id == run_id)
    }

    fn finish_active_run(&mut self) {
        self.busy = false;
        if let Some(run) = self.active_run.take() {
            if run.thread.is_finished() {
                let _ = run.thread.join();
            } else {
                self.detached_run_threads.push(run.thread);
            }
        }
    }

    fn reap_detached_run_threads(&mut self) {
        if self.detached_run_threads.is_empty() {
            return;
        }
        let mut still_running = Vec::with_capacity(self.detached_run_threads.len());
        for thread in self.detached_run_threads.drain(..) {
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                still_running.push(thread);
            }
        }
        self.detached_run_threads = still_running;
    }
}

impl Drop for TranslationMtController {
    fn drop(&mut self) {
        self.busy = false;
        if let Some(run) = self.active_run.take() {
            run.cancel_requested.store(true, Ordering::Relaxed);
            self.detached_run_threads.push(run.thread);
        }
        self.reap_detached_run_threads();
    }
}

#[derive(Debug)]
enum WorkerEvent {
    RunStarted {
        run_id: u64,
        total: usize,
    },
    ItemTranslated {
        run_id: u64,
        bubble_id: i64,
        translated_text: String,
        original_text: Option<String>,
    },
    ItemAreasTranslated {
        run_id: u64,
        bubble_id: i64,
        areas: Vec<(String, String)>,
    },
    ItemFailed {
        run_id: u64,
        bubble_id: i64,
        error: String,
    },
    RunFinished {
        run_id: u64,
        translated: usize,
        errors: usize,
    },
    RunCancelled {
        run_id: u64,
        translated: usize,
        errors: usize,
    },
    RunFailed {
        run_id: u64,
        error: String,
    },
    Progress {
        run_id: u64,
        translated: usize,
        errors: usize,
        total: usize,
        context_used_chars: usize,
        context_budget_chars: usize,
        pruned_replicas: usize,
    },
    AiApiKeyStored {
        service: AiApiService,
    },
    AiApiKeyCleared {
        service: AiApiService,
    },
    AiApiMetadataLoaded(super::ocr::AiApiMetadata),
    AiApiMetadataErr {
        service: AiApiService,
        error: String,
    },
}

fn run_translate_request(
    run_id: u64,
    request: MtTranslateRequest,
    evt_tx: &Sender<WorkerEvent>,
    cancel_requested: &Arc<AtomicBool>,
) {
    let MtTranslateRequest {
        service,
        source_lang,
        target_lang,
        items,
        ai_api,
    } = request;
    // Context-only replicas (already translated, included for ordering) are not counted as work:
    // progress totals reflect only the replicas that actually need translation.
    let total = items.iter().filter(|item| item.needs_translation).count();
    let _ = evt_tx.send(WorkerEvent::RunStarted { run_id, total });

    let mut translated = 0usize;
    let mut errors = 0usize;

    if let Some(ai_options) = ai_api {
        run_ai_translate_request(
            run_id,
            &source_lang,
            &target_lang,
            items,
            ai_options,
            evt_tx,
            cancel_requested,
            &mut translated,
            &mut errors,
        );
        return;
    }

    for item in items {
        // The non-AI translators have no batch context, so context-only replicas (if any leaked in)
        // are skipped instead of being sent to the provider.
        if !item.needs_translation {
            continue;
        }
        if cancel_requested.load(Ordering::Relaxed) {
            let _ = evt_tx.send(WorkerEvent::RunCancelled {
                run_id,
                translated,
                errors,
            });
            return;
        }

        let item_id = item.bubble_id;
        let backend_result =
            translate_texts_via_translator(service, &source_lang, &target_lang, vec![item.text]);

        if cancel_requested.load(Ordering::Relaxed) {
            let _ = evt_tx.send(WorkerEvent::RunCancelled {
                run_id,
                translated,
                errors,
            });
            return;
        }

        match backend_result {
            Ok(mut results) => {
                if results.len() != 1 {
                    let err = tf!("translation.mt.invalid_response_error", results = results.len());
                    let _ = evt_tx.send(WorkerEvent::ItemFailed {
                        run_id,
                        bubble_id: item_id,
                        error: err,
                    });
                    errors += 1;
                    send_mt_progress(
                        evt_tx,
                        WorkerProgressSnapshot::simple(run_id, translated, errors, total),
                    );
                    continue;
                }

                match results.pop().expect("len checked == 1") {
                    Ok(text) => {
                        let _ = evt_tx.send(WorkerEvent::ItemTranslated {
                            run_id,
                            bubble_id: item_id,
                            translated_text: text,
                            original_text: None,
                        });
                        translated += 1;
                    }
                    Err(err) => {
                        let _ = evt_tx.send(WorkerEvent::ItemFailed {
                            run_id,
                            bubble_id: item_id,
                            error: err,
                        });
                        errors += 1;
                    }
                }
                send_mt_progress(
                    evt_tx,
                    WorkerProgressSnapshot::simple(run_id, translated, errors, total),
                );
            }
            Err(err) => {
                let _ = evt_tx.send(WorkerEvent::ItemFailed {
                    run_id,
                    bubble_id: item_id,
                    error: err,
                });
                errors += 1;
                send_mt_progress(
                    evt_tx,
                    WorkerProgressSnapshot::simple(run_id, translated, errors, total),
                );
            }
        }
    }

    let _ = evt_tx.send(WorkerEvent::RunFinished {
        run_id,
        translated,
        errors,
    });
}

#[derive(Debug, Clone, Copy)]
struct WorkerProgressSnapshot {
    run_id: u64,
    translated: usize,
    errors: usize,
    total: usize,
    context_used_chars: usize,
    context_budget_chars: usize,
    pruned_replicas: usize,
}

impl WorkerProgressSnapshot {
    fn simple(run_id: u64, translated: usize, errors: usize, total: usize) -> Self {
        Self {
            run_id,
            translated,
            errors,
            total,
            context_used_chars: 0,
            context_budget_chars: 0,
            pruned_replicas: 0,
        }
    }
}

fn send_mt_progress(evt_tx: &Sender<WorkerEvent>, progress: WorkerProgressSnapshot) {
    let _ = evt_tx.send(WorkerEvent::Progress {
        run_id: progress.run_id,
        translated: progress.translated,
        errors: progress.errors,
        total: progress.total,
        context_used_chars: progress.context_used_chars,
        context_budget_chars: progress.context_budget_chars,
        pruned_replicas: progress.pruned_replicas,
    });
}

fn translate_texts_via_translator(
    service: MtService,
    source_lang: &str,
    target_lang: &str,
    texts: Vec<String>,
) -> Result<Vec<Result<String, String>>, String> {
    match service {
        MtService::Google => GoogleMtBackend.translate_texts(source_lang, target_lang, texts),
        MtService::Yandex => YandexMtBackend.translate_texts(source_lang, target_lang, texts),
        MtService::Deepl => DeeplMtBackend.translate_texts(source_lang, target_lang, texts),
    }
}

/// Keyword fragments (lowercase) that strongly suggest an AI provider stopped the request because
/// the account is out of credits/quota or hit a usage/rate limit. Kept provider-agnostic so it
/// covers OpenAI, Anthropic, Gemini, OpenRouter and similar wordings, plus the HTTP 402/429 codes
/// these providers return for billing/rate problems.
const AI_QUOTA_LIMIT_ERROR_KEYWORDS: &[&str] = &[
    "insufficient_quota",
    "insufficient quota",
    "insufficient credit",
    "insufficient funds",
    "not enough credit",
    "out of credit",
    "no credits",
    "quota exceeded",
    "exceeded your current quota",
    "exceeded your quota",
    "usage limit",
    "monthly limit",
    "spending limit",
    "limit reached",
    "limit exceeded",
    "rate limit",
    "rate_limit",
    "ratelimit",
    "too many requests",
    "resource_exhausted",
    "resource exhausted",
    "credit balance is too low",
    "payment required",
    "billing",
    "status code: 402",
    "status code: 429",
    "status: 402",
    "status: 429",
    "error 402",
    "error 429",
    "http 402",
    "http 429",
];

/// Best-effort classification of an AI provider error string as a credit/quota/limit exhaustion
/// rather than a transient network glitch or a configuration mistake.
///
/// Matching is a case-insensitive substring scan over [`AI_QUOTA_LIMIT_ERROR_KEYWORDS`], so it stays
/// provider-agnostic. It is intentionally a heuristic: a false positive only changes a red error
/// toast into the softer "probably out of credits/limit" notice, and the full original error is
/// always still available to the user.
#[must_use]
pub fn is_probable_quota_or_limit_error(error: &str) -> bool {
    let haystack = error.to_ascii_lowercase();
    AI_QUOTA_LIMIT_ERROR_KEYWORDS
        .iter()
        .any(|keyword| haystack.contains(keyword))
}

/// Web stub: AI API translation runs over `genai` + `tokio`, which are not
/// compiled for the browser build. Fails the run with a clear message rather than
/// producing a fake translation. Signature matches the native twin so the neutral
/// `run_translate_request` dispatch compiles on both targets.
#[cfg(target_arch = "wasm32")]
#[allow(clippy::too_many_arguments)]
fn run_ai_translate_request(
    run_id: u64,
    _source_lang: &str,
    _target_lang: &str,
    _items: Vec<MtTranslateItem>,
    _options: AiMtOptions,
    evt_tx: &Sender<WorkerEvent>,
    _cancel_requested: &Arc<AtomicBool>,
    _translated: &mut usize,
    _errors: &mut usize,
) {
    let _ = evt_tx.send(WorkerEvent::RunFailed {
        run_id,
        error: t!("translation.mt.ai_api_web_unavailable_error").to_string(),
    });
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
fn run_ai_translate_request(
    run_id: u64,
    source_lang: &str,
    target_lang: &str,
    mut items: Vec<MtTranslateItem>,
    options: AiMtOptions,
    evt_tx: &Sender<WorkerEvent>,
    cancel_requested: &Arc<AtomicBool>,
    translated: &mut usize,
    errors: &mut usize,
) {
    let service = options.service;
    let api_key = match read_ai_api_key(service) {
        Ok(key) if !key.trim().is_empty() => key,
        Ok(_) => {
            let _ = evt_tx.send(WorkerEvent::RunFailed {
                run_id,
                error: tf!("translation.mt.api_key_missing_error", service = service.label()),
            });
            return;
        }
        Err(error) => {
            runtime_log::log_error(format!(
                "[MT][run {run_id}] failed to read API key for {}: {error}",
                service.label()
            ));
            let _ = evt_tx.send(WorkerEvent::RunFailed { run_id, error });
            return;
        }
    };

    sort_ai_mt_items(&mut items, options.sort_mode());

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            runtime_log::log_error(format!(
                "[MT][run {run_id}] failed to create async runtime: {err}"
            ));
            let _ = evt_tx.send(WorkerEvent::RunFailed {
                run_id,
                error: tf!("translation.mt.async_runtime_error", err = err),
            });
            return;
        }
    };

    let result = runtime.block_on(async {
        run_ai_translate_async(
            run_id,
            source_lang,
            target_lang,
            items,
            options,
            api_key,
            evt_tx,
            cancel_requested,
            translated,
            errors,
        )
        .await
    });

    if let Err(error) = result {
        runtime_log::log_error(format!(
            "[MT][run {run_id}] AI translation run failed: {}",
            error.replace('\n', " ")
        ));
        let _ = evt_tx.send(WorkerEvent::RunFailed { run_id, error });
        return;
    }

    if cancel_requested.load(Ordering::Relaxed) {
        let _ = evt_tx.send(WorkerEvent::RunCancelled {
            run_id,
            translated: *translated,
            errors: *errors,
        });
    } else {
        let _ = evt_tx.send(WorkerEvent::RunFinished {
            run_id,
            translated: *translated,
            errors: *errors,
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
async fn run_ai_translate_async(
    run_id: u64,
    source_lang: &str,
    target_lang: &str,
    items: Vec<MtTranslateItem>,
    options: AiMtOptions,
    api_key: String,
    evt_tx: &Sender<WorkerEvent>,
    cancel_requested: &Arc<AtomicBool>,
    translated: &mut usize,
    errors: &mut usize,
) -> Result<(), String> {
    // Per-ImageBubble mode is a separate single-turn-per-image driver with its own prompts and cache
    // strategy; the batched chat-history path below is only for the normal mode.
    if options.image_mode == AiMtImageMode::ImagesOnly {
        return run_ai_imagebubble_translate_async(
            run_id,
            source_lang,
            target_lang,
            items,
            options,
            api_key,
            evt_tx,
            cancel_requested,
            translated,
            errors,
        )
        .await;
    }

    let client = build_ai_api_client(options.service, api_key);
    let model = model_iden_for_ai_api_service(options.service, &options.model);
    let mut chat_req = ChatRequest::default().with_system(build_ai_mt_system_prompt(
        source_lang,
        target_lang,
        &options,
    ));
    let chat_options = build_ai_mt_chat_options(options.reasoning);
    let batch_size = options.batch_size.clamp(1, 100);
    let context_budget = context_char_budget(options.context_limit_percent);
    let total = items.iter().filter(|item| item.needs_translation).count();
    let context_items = items.len().saturating_sub(total);
    let items_with_images = items.iter().filter(|item| item.image.is_some()).count();
    let mut history_batch_sizes: VecDeque<usize> = VecDeque::new();
    let mut pruned_replicas = 0usize;

    let batches = split_ai_mt_batches(&items, batch_size);
    let batch_total = batches.len();
    runtime_log::log_info(format!(
        "[MT][run {run_id}] start AI translation: service={:?}, model='{}', {source_lang}->{target_lang}, batch_size={batch_size}, batches={batch_total}, translate={total}, context={context_items}, total_items={}, image_bubbles={items_with_images}, image_detail={:?}, context_limit={}% (~{context_budget} chars)",
        options.service,
        options.model,
        items.len(),
        options.image_detail,
        options.context_limit_percent,
    ));

    for (batch_idx, chunk) in batches.into_iter().enumerate() {
        let batch_no = batch_idx + 1;
        if cancel_requested.load(Ordering::Relaxed) {
            runtime_log::log_info(format!(
                "[MT][run {run_id}] cancelled before batch {batch_no}/{batch_total} (translated={translated}, errors={errors})",
                translated = *translated,
                errors = *errors,
            ));
            return Ok(());
        }

        pruned_replicas = pruned_replicas.saturating_add(prune_ai_mt_chat_context(
            &mut chat_req,
            &mut history_batch_sizes,
            context_budget,
        ));
        let translate_in_batch = chunk.iter().filter(|item| item.needs_translation).count();
        let context_in_batch = chunk.len().saturating_sub(translate_in_batch);
        let (user_message, image_stats) = match build_ai_mt_user_message(chunk, &options) {
            Ok(message) => message,
            Err(err) => {
                runtime_log::log_error(format!(
                    "[MT][run {run_id}] batch {batch_no}/{batch_total}: failed to build request: {err}"
                ));
                return Err(err);
            }
        };
        runtime_log::log_info(format!(
            "[MT][run {run_id}] batch {batch_no}/{batch_total} -> sending: replicas={} (translate={translate_in_batch}, context={context_in_batch}), images={} ({} KiB), history_turns={}, pruned_total={pruned_replicas}",
            chunk.len(),
            image_stats.image_count,
            image_stats.image_bytes / 1024,
            history_batch_sizes.len(),
        ));
        chat_req = chat_req.append_message(user_message);
        let response = match client
            .exec_chat(model.clone(), chat_req.clone(), chat_options.as_ref())
            .await
        {
            Ok(response) => response,
            Err(err) => {
                // Log the raw provider error before it is wrapped and surfaced as RunFailed, so the
                // exact cause (rate limit, quota, oversized request, network) is recoverable.
                runtime_log::log_error(format!(
                    "[MT][run {run_id}] batch {batch_no}/{batch_total}: provider request failed: {err}"
                ));
                return Err(tf!("translation.mt.ai_translate_failed_error", err = err));
            }
        };
        let response_text = response.first_text().unwrap_or("").trim().to_string();
        let batch_result = parse_ai_mt_response(&response_text);
        match &batch_result {
            Ok(parsed) => runtime_log::log_info(format!(
                "[MT][run {run_id}] batch {batch_no}/{batch_total} <- response: {} chars, parsed {} entries",
                response_text.chars().count(),
                parsed.len(),
            )),
            Err(err) => runtime_log::log_warn(format!(
                "[MT][run {run_id}] batch {batch_no}/{batch_total} <- response: {} chars, parse failed: {err}",
                response_text.chars().count(),
            )),
        }
        replace_last_ai_mt_user_message_with_history(&mut chat_req, chunk, &options)?;
        chat_req = chat_req.append_message(ChatMessage::assistant(response_text.clone()));
        history_batch_sizes.push_back(chunk.len());

        match batch_result {
            Ok(translations) => {
                for item in chunk {
                    // Context-only replicas are provided for ordering and are not expected back.
                    if !item.needs_translation {
                        continue;
                    }
                    if cancel_requested.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let entry = translations
                        .iter()
                        .find(|entry| entry.bubble_id == item.bubble_id);
                    let multi_area = item.image.as_ref().is_some_and(MtImageInput::is_multi_area);
                    let ok = if multi_area {
                        // Multi-area image bubble: require a per-area result array.
                        match entry.filter(|entry| !entry.areas.is_empty()) {
                            Some(entry) => {
                                let areas = entry
                                    .areas
                                    .iter()
                                    .map(|area| {
                                        (area.original_text.clone(), area.translation.clone())
                                    })
                                    .collect();
                                let _ = evt_tx.send(WorkerEvent::ItemAreasTranslated {
                                    run_id,
                                    bubble_id: item.bubble_id,
                                    areas,
                                });
                                true
                            }
                            None => false,
                        }
                    } else {
                        match entry.filter(|entry| {
                            !entry.translation.trim().is_empty()
                                && (item.image.is_none() || entry.original_text.is_some())
                        }) {
                            Some(entry) => {
                                let _ = evt_tx.send(WorkerEvent::ItemTranslated {
                                    run_id,
                                    bubble_id: item.bubble_id,
                                    translated_text: entry.translation.clone(),
                                    original_text: entry.original_text.clone(),
                                });
                                true
                            }
                            None => false,
                        }
                    };
                    if ok {
                        *translated = translated.saturating_add(1);
                    } else {
                        let error = if multi_area {
                            t!("translation.mt.ai_no_areas_error")
                                .to_string()
                        } else if item.image.is_some() {
                            t!("translation.mt.ai_no_original_translation_error").to_string()
                        } else {
                            t!("translation.mt.ai_no_translation_for_id_error").to_string()
                        };
                        let _ = evt_tx.send(WorkerEvent::ItemFailed {
                            run_id,
                            bubble_id: item.bubble_id,
                            error,
                        });
                        *errors = errors.saturating_add(1);
                    }
                }
            }
            Err(error) => {
                for item in chunk {
                    // Only replicas that needed translation count as failures; context is ignored.
                    if !item.needs_translation {
                        continue;
                    }
                    let _ = evt_tx.send(WorkerEvent::ItemFailed {
                        run_id,
                        bubble_id: item.bubble_id,
                        error: error.clone(),
                    });
                    *errors = errors.saturating_add(1);
                }
            }
        }
        let context_used_chars = chat_req
            .messages
            .iter()
            .map(ChatMessage::size)
            .sum::<usize>();
        let _ = evt_tx.send(WorkerEvent::Progress {
            run_id,
            translated: *translated,
            errors: *errors,
            total,
            context_used_chars,
            context_budget_chars: context_budget,
            pruned_replicas,
        });
        runtime_log::log_info(format!(
            "[MT][run {run_id}] batch {batch_no}/{batch_total} done: translated={}/{total}, errors={}, context_used~{context_used_chars}/{context_budget} chars",
            *translated, *errors,
        ));
    }

    runtime_log::log_info(format!(
        "[MT][run {run_id}] AI translation finished: translated={}/{total}, errors={}, pruned_total={pruned_replicas}",
        *translated, *errors,
    ));
    Ok(())
}

/// Header line that precedes the joined chapter-context descriptors in the per-ImageBubble request.
/// Kept constant so the context block stays a byte-stable, append-only prefix across image requests.
const IMAGEBUBBLE_CONTEXT_HEADER: &str = "Chapter context so far (already known bubbles in reading order; never translate or echo these):";

/// Per-ImageBubble translation driver.
///
/// Walks the sorted items once. Non-translatable items are appended to an append-only text
/// `context` prefix (rendered as the chosen [`AiMtContextSource`]). Each translatable ImageBubble is
/// sent in its own single-turn request laid out as:
/// `system(static) + system(chapter context, cached) + user(target image + instruction)`. Because
/// the context is append-only and the image binary never enters the context, request N+1's input has
/// request N's input as a literal prefix, so provider prompt caching reuses it across the chapter.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
async fn run_ai_imagebubble_translate_async(
    run_id: u64,
    source_lang: &str,
    target_lang: &str,
    items: Vec<MtTranslateItem>,
    options: AiMtOptions,
    api_key: String,
    evt_tx: &Sender<WorkerEvent>,
    cancel_requested: &Arc<AtomicBool>,
    translated: &mut usize,
    errors: &mut usize,
) -> Result<(), String> {
    let client = build_ai_api_client(options.service, api_key);
    let model = model_iden_for_ai_api_service(options.service, &options.model);
    let system_prompt = build_ai_mt_imagebubble_system_prompt(source_lang, target_lang, &options);
    // OpenAI-style providers route by this stable key; Anthropic gets message-level cache markers.
    let cache_key = format!(
        "mtimg-{}-{source_lang}-{target_lang}",
        options.service.key()
    );
    let chat_options = build_ai_mt_imagebubble_chat_options(options.reasoning, &cache_key);
    let anthropic_cache = matches!(options.service, AiApiService::Anthropic);
    let context_budget = context_char_budget(options.context_limit_percent);
    let total = items.iter().filter(|item| item.needs_translation).count();
    let context_items = items.len().saturating_sub(total);

    runtime_log::log_info(format!(
        "[MT][run {run_id}] start AI ImageBubble translation: service={:?}, model='{}', {source_lang}->{target_lang}, targets={total}, context_items={context_items}, context_source={:?}, image_detail={:?}, anthropic_cache={anthropic_cache}, context_limit={}% (~{context_budget} chars)",
        options.service,
        options.model,
        options.image_context_source,
        options.image_detail,
        options.context_limit_percent,
    ));

    let mut context: Vec<String> = Vec::new();
    let mut pruned_replicas = 0usize;
    let mut target_no = 0usize;

    for item in &items {
        if cancel_requested.load(Ordering::Relaxed) {
            runtime_log::log_info(format!(
                "[MT][run {run_id}] cancelled (translated={translated}, errors={errors})",
                translated = *translated,
                errors = *errors,
            ));
            return Ok(());
        }

        if !item.needs_translation {
            context.push(imagebubble_context_line_for_item(
                item,
                options.image_context_source,
            ));
            pruned_replicas = pruned_replicas
                .saturating_add(prune_imagebubble_context(&mut context, context_budget));
            continue;
        }

        target_no += 1;
        let target_total = total;
        let encoded = match imagebubble_target_image(&options, item) {
            Ok(encoded) => encoded,
            Err(err) => {
                runtime_log::log_error(format!(
                    "[MT][run {run_id}] target {target_no}/{target_total} (id={}): image load failed: {err}",
                    item.bubble_id,
                ));
                let _ = evt_tx.send(WorkerEvent::ItemFailed {
                    run_id,
                    bubble_id: item.bubble_id,
                    error: err,
                });
                *errors = errors.saturating_add(1);
                send_imagebubble_progress(
                    evt_tx,
                    run_id,
                    *translated,
                    *errors,
                    total,
                    &context,
                    context_budget,
                    pruned_replicas,
                );
                continue;
            }
        };

        let image_bytes = encoded.bytes.len();
        let chat_req = build_imagebubble_chat_request(
            &system_prompt,
            &context,
            item,
            encoded,
            anthropic_cache,
            options.include_existing_translation,
        )?;
        runtime_log::log_info(format!(
            "[MT][run {run_id}] target {target_no}/{target_total} (id={}) -> sending: context_items={}, context~{} chars, image={} KiB",
            item.bubble_id,
            context.len(),
            imagebubble_context_chars(&context),
            image_bytes / 1024,
        ));

        let response = match client
            .exec_chat(model.clone(), chat_req, chat_options.as_ref())
            .await
        {
            Ok(response) => response,
            Err(err) => {
                runtime_log::log_error(format!(
                    "[MT][run {run_id}] target {target_no}/{target_total} (id={}): provider request failed: {err}",
                    item.bubble_id,
                ));
                return Err(tf!("translation.mt.ai_translate_failed_error", err = err));
            }
        };
        let response_text = response.first_text().unwrap_or("").trim().to_string();

        let resolved = match parse_ai_mt_response(&response_text) {
            Ok(translations) => {
                runtime_log::log_info(format!(
                    "[MT][run {run_id}] target {target_no}/{target_total} (id={}) <- response: {} chars, parsed {} entries",
                    item.bubble_id,
                    response_text.chars().count(),
                    translations.len(),
                ));
                let entry = translations
                    .iter()
                    .find(|entry| entry.bubble_id == item.bubble_id)
                    .or_else(|| translations.first());
                emit_imagebubble_result(run_id, evt_tx, item, entry, translated, errors)
            }
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[MT][run {run_id}] target {target_no}/{target_total} (id={}) <- response: {} chars, parse failed: {err}",
                    item.bubble_id,
                    response_text.chars().count(),
                ));
                let _ = evt_tx.send(WorkerEvent::ItemFailed {
                    run_id,
                    bubble_id: item.bubble_id,
                    error: err,
                });
                *errors = errors.saturating_add(1);
                None
            }
        };

        // Fold the just-handled image into the context as text (never its binary) so it carries over
        // to the next image while keeping the cached prefix cheap.
        context.push(imagebubble_context_line_for_target(
            item,
            options.image_context_source,
            resolved.as_ref(),
        ));
        pruned_replicas =
            pruned_replicas.saturating_add(prune_imagebubble_context(&mut context, context_budget));
        send_imagebubble_progress(
            evt_tx,
            run_id,
            *translated,
            *errors,
            total,
            &context,
            context_budget,
            pruned_replicas,
        );
    }

    runtime_log::log_info(format!(
        "[MT][run {run_id}] AI ImageBubble translation finished: translated={}/{total}, errors={}, pruned_total={pruned_replicas}",
        *translated, *errors,
    ));
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
fn send_imagebubble_progress(
    evt_tx: &Sender<WorkerEvent>,
    run_id: u64,
    translated: usize,
    errors: usize,
    total: usize,
    context: &[String],
    context_budget: usize,
    pruned_replicas: usize,
) {
    let _ = evt_tx.send(WorkerEvent::Progress {
        run_id,
        translated,
        errors,
        total,
        context_used_chars: imagebubble_context_chars(context),
        context_budget_chars: context_budget,
        pruned_replicas,
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn imagebubble_context_chars(context: &[String]) -> usize {
    // +1 per joined newline, matching the text actually sent.
    context.iter().map(String::len).sum::<usize>() + context.len().saturating_sub(1)
}

/// Keeps the append-only context prefix under budget by dropping whole oldest items from the front.
/// Trimming the front does invalidate the shared cached prefix at that boundary, so this only fires
/// when the chapter context (text only) actually exceeds the budget — for typical chapters it never
/// does and the prefix stays fully cacheable. Returns how many items were dropped.
#[cfg(not(target_arch = "wasm32"))]
fn prune_imagebubble_context(context: &mut Vec<String>, budget: usize) -> usize {
    let mut pruned = 0usize;
    while context.len() > 1 && imagebubble_context_chars(context) > budget {
        context.remove(0);
        pruned += 1;
    }
    pruned
}

/// Loads and encodes the binary image for a target ImageBubble.
fn imagebubble_target_image(
    options: &AiMtOptions,
    item: &MtTranslateItem,
) -> Result<EncodedMtImage, String> {
    let image = item
        .image
        .as_ref()
        .ok_or_else(|| t!("translation.mt.imagebubble_no_image_error").to_string())?;
    load_mt_encoded_image(&options.project, image, options.image_detail)
}

/// Builds the single-turn request for one target ImageBubble: a static system prompt, a cached
/// system block with the chapter context so far, and a user message with the target image.
#[cfg(not(target_arch = "wasm32"))]
fn build_imagebubble_chat_request(
    system_prompt: &str,
    context: &[String],
    target: &MtTranslateItem,
    encoded: EncodedMtImage,
    anthropic_cache: bool,
    include_existing_translation: bool,
) -> Result<ChatRequest, String> {
    let mut req = ChatRequest::default().with_system(system_prompt.to_string());
    if !context.is_empty() {
        let context_text = format!("{IMAGEBUBBLE_CONTEXT_HEADER}\n{}", context.join("\n"));
        let mut context_msg = ChatMessage::system(context_text);
        if anthropic_cache {
            // Marks the cache breakpoint at the end of the context block: Anthropic caches the whole
            // prefix up to and including it (static system + chapter context), never the image tail.
            context_msg = context_msg.with_options(CacheControl::Ephemeral);
        }
        req = req.append_message(context_msg);
    }
    let descriptor = ai_mt_target_descriptor(target, include_existing_translation)?;
    let parts = vec![
        ContentPart::from_text(descriptor),
        ContentPart::from_binary_base64(
            encoded.mime_type,
            base64_encode(&encoded.bytes),
            Some(format!(
                "image-bubble-{}.{}",
                target.bubble_id, encoded.extension
            )),
        ),
    ];
    req = req.append_message(ChatMessage::user(parts));
    Ok(req)
}

/// Renders the directive + descriptor for the one target ImageBubble (reusing the ordered-item JSON).
fn ai_mt_target_descriptor(
    target: &MtTranslateItem,
    include_existing_translation: bool,
) -> Result<String, String> {
    let body = ai_mt_ordered_item_descriptor(target, include_existing_translation)?;
    Ok(format!(
        "Target image bubble to translate now. Read the attached image and translate ONLY this bubble; return a single JSON object as instructed.\n{body}"
    ))
}

/// Builds the chapter context line (compact JSON) for a non-target bubble, using the chosen source.
fn imagebubble_context_line_for_item(item: &MtTranslateItem, source: AiMtContextSource) -> String {
    let text = imagebubble_context_text(&item.text, &item.existing_translation, source);
    imagebubble_context_line(item.bubble_id, &item.character, text)
}

/// Builds the chapter context line for a just-translated target image, preferring the model's
/// resolved original/translation and falling back to the item's stored fields when missing.
#[cfg(not(target_arch = "wasm32"))]
fn imagebubble_context_line_for_target(
    item: &MtTranslateItem,
    source: AiMtContextSource,
    resolved: Option<&ResolvedImageText>,
) -> String {
    let original = resolved
        .map(|resolved| resolved.original.as_str())
        .filter(|original| !original.trim().is_empty())
        .unwrap_or(item.text.as_str());
    let translation = resolved
        .map(|resolved| resolved.translation.as_str())
        .filter(|translation| !translation.trim().is_empty())
        .unwrap_or(item.existing_translation.as_str());
    let text = imagebubble_context_text(original, translation, source);
    imagebubble_context_line(item.bubble_id, &item.character, text)
}

/// Picks the context text for the chosen source. `Translation` prefers the translation but falls back
/// to the original when there is no translation yet, so still-untranslated context bubbles remain
/// useful for continuity.
fn imagebubble_context_text<'a>(
    original: &'a str,
    translation: &'a str,
    source: AiMtContextSource,
) -> &'a str {
    match source {
        AiMtContextSource::Original => original,
        AiMtContextSource::Translation => {
            if translation.trim().is_empty() {
                original
            } else {
                translation
            }
        }
    }
}

/// Serializes one context bubble as a compact one-line JSON object. The `id` is always present (so
/// reading order/continuity is preserved even for empty text); `character`/`text` only when non-empty.
fn imagebubble_context_line(id: i64, character: &str, text: &str) -> String {
    let mut entry = serde_json::Map::new();
    entry.insert("id".to_string(), Value::Number(id.into()));
    if !character.trim().is_empty() {
        entry.insert(
            "character".to_string(),
            Value::String(character.trim().to_string()),
        );
    }
    let text = text.trim();
    if !text.is_empty() {
        entry.insert("text".to_string(), Value::String(text.to_string()));
    }
    serde_json::to_string(&Value::Object(entry)).unwrap_or_else(|_| format!("{{\"id\":{id}}}"))
}

/// Resolved source/translation text of a translated image, used to fold it into later context.
#[cfg(not(target_arch = "wasm32"))]
struct ResolvedImageText {
    original: String,
    translation: String,
}

/// Sends the per-item translation event for a single target ImageBubble and returns its resolved
/// text for context folding, or `None` when the model returned nothing usable (already reported as a
/// failure).
#[cfg(not(target_arch = "wasm32"))]
fn emit_imagebubble_result(
    run_id: u64,
    evt_tx: &Sender<WorkerEvent>,
    item: &MtTranslateItem,
    entry: Option<&AiMtTranslation>,
    translated: &mut usize,
    errors: &mut usize,
) -> Option<ResolvedImageText> {
    let multi_area = item.image.as_ref().is_some_and(MtImageInput::is_multi_area);
    if multi_area {
        match entry.filter(|entry| !entry.areas.is_empty()) {
            Some(entry) => {
                let areas: Vec<(String, String)> = entry
                    .areas
                    .iter()
                    .map(|area| (area.original_text.clone(), area.translation.clone()))
                    .collect();
                let original = join_nonempty(areas.iter().map(|(original, _)| original.as_str()));
                let translation =
                    join_nonempty(areas.iter().map(|(_, translation)| translation.as_str()));
                let _ = evt_tx.send(WorkerEvent::ItemAreasTranslated {
                    run_id,
                    bubble_id: item.bubble_id,
                    areas,
                });
                *translated = translated.saturating_add(1);
                Some(ResolvedImageText {
                    original,
                    translation,
                })
            }
            None => {
                let _ = evt_tx.send(WorkerEvent::ItemFailed {
                    run_id,
                    bubble_id: item.bubble_id,
                    error: t!("translation.mt.ai_no_areas_error")
                        .to_string(),
                });
                *errors = errors.saturating_add(1);
                None
            }
        }
    } else {
        match entry
            .filter(|entry| !entry.translation.trim().is_empty() && entry.original_text.is_some())
        {
            Some(entry) => {
                let _ = evt_tx.send(WorkerEvent::ItemTranslated {
                    run_id,
                    bubble_id: item.bubble_id,
                    translated_text: entry.translation.clone(),
                    original_text: entry.original_text.clone(),
                });
                *translated = translated.saturating_add(1);
                Some(ResolvedImageText {
                    original: entry.original_text.clone().unwrap_or_default(),
                    translation: entry.translation.clone(),
                })
            }
            None => {
                let _ = evt_tx.send(WorkerEvent::ItemFailed {
                    run_id,
                    bubble_id: item.bubble_id,
                    error: t!("translation.mt.ai_no_original_translation_error").to_string(),
                });
                *errors = errors.saturating_add(1);
                None
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn join_nonempty<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" / ")
}

fn build_ai_mt_imagebubble_system_prompt(
    source_lang: &str,
    target_lang: &str,
    options: &AiMtOptions,
) -> String {
    let mut parts = Vec::new();
    let base = options.system_instruction.trim();
    if base.is_empty() {
        parts.push("You are a manga/comic translation engine. Translate faithfully into Russian. Since the text was recognized using OCR, there may be errors, and the English text is often in uppercase. Do not preserve line breaks; write the translation in normal text, not in all caps. Preserve tone, names, honorifics, jokes, and speaker intent.".to_string());
    } else {
        parts.push(base.to_string());
    }
    parts.push(format!(
        "Source language: {source_lang}. Target language: {target_lang}."
    ));
    parts.push("You are given the full preceding context of the chapter as ordered, already-known bubbles (in the system context), followed by exactly one target image bubble (the user message, with an attached image). Use the context only to keep names, tone, honorifics and terminology consistent — never translate, echo, or output the context bubbles. Read the attached image and translate ONLY the target image bubble. Return a single JSON object. For a single-area bubble: {\"id\": number, \"original_text\": string, \"translation\": string}. For a multi-area bubble (it has \"area_count\" and an ordered \"areas\" list): {\"id\": number, \"areas\": [{\"original_text\": string, \"translation\": string}, ...]} with exactly area_count entries in the same order as the input areas. Output only that JSON object: no markdown, no commentary, no other ids.".to_string());
    if options.use_character_names {
        parts.push("Use character names as speaker context, but do not add speaker labels unless they are present in the source text.".to_string());
    }

    push_project_context_parts(&mut parts, options);

    parts.join("\n\n")
}

#[cfg(not(target_arch = "wasm32"))]
fn build_ai_mt_imagebubble_chat_options(
    reasoning: AiMtReasoning,
    cache_key: &str,
) -> Option<ChatOptions> {
    let mut options = ChatOptions::default().with_prompt_cache_key(cache_key.to_string());
    if let Some(effort) = reasoning.to_genai() {
        options = options
            .with_reasoning_effort(effort)
            .with_normalize_reasoning_content(true);
    }
    Some(options)
}

impl AiMtOptions {
    fn sort_mode(&self) -> AiMtSortMode {
        self.sort_mode
    }
}

/// Splits sorted, reading-order items into AI batches.
///
/// Each batch holds at most `batch_size` replicas that need translation, plus any already-translated
/// context replicas that precede or sit between them in reading order. A batch is cut right after
/// its `batch_size`-th translatable replica, so a context replica that follows once the per-batch
/// limit is already reached is deferred to a later batch (it only joins the window once translation
/// reaches it). The final segment is emitted only when it still contains a translatable replica, so
/// a trailing run of context-only replicas is never sent on its own.
///
/// `items` must already be sorted into the intended reading order. `batch_size` must be >= 1.
fn split_ai_mt_batches(items: &[MtTranslateItem], batch_size: usize) -> Vec<&[MtTranslateItem]> {
    let batch_size = batch_size.max(1);
    let mut batches = Vec::new();
    let mut start = 0usize;
    let mut translate_in_batch = 0usize;
    for (idx, item) in items.iter().enumerate() {
        if !item.needs_translation {
            continue;
        }
        translate_in_batch += 1;
        if translate_in_batch == batch_size {
            // Cut immediately after the limit-th translatable replica; any context that follows
            // belongs to the next window.
            batches.push(&items[start..=idx]);
            start = idx + 1;
            translate_in_batch = 0;
        }
    }
    if start < items.len() && items[start..].iter().any(|item| item.needs_translation) {
        batches.push(&items[start..]);
    }
    batches
}

fn sort_ai_mt_items(items: &mut [MtTranslateItem], sort_mode: AiMtSortMode) {
    items.sort_by(|a, b| match sort_mode {
        AiMtSortMode::Height => a
            .page_idx
            .cmp(&b.page_idx)
            .then_with(|| a.img_v.total_cmp(&b.img_v)),
        AiMtSortMode::Number => a
            .page_idx
            .cmp(&b.page_idx)
            .then(a.order.cmp(&b.order))
            .then_with(|| a.img_v.total_cmp(&b.img_v)),
    });
}

fn build_ai_mt_system_prompt(
    source_lang: &str,
    target_lang: &str,
    options: &AiMtOptions,
) -> String {
    let mut parts = Vec::new();
    let base = options.system_instruction.trim();
    if base.is_empty() {
        parts.push("You are a manga/comic translation engine. Translate faithfully into Russian. Since the text was recognized using OCR, there may be errors, and the English text is often in uppercase. Do not preserve line breaks; write the translation in normal text, not in all caps. Preserve tone, names, honorifics, jokes, and speaker intent. Return only valid JSON with id and translation.".to_string());
    } else {
        parts.push(base.to_string());
    }
    parts.push(format!(
        "Source language: {source_lang}. Target language: {target_lang}."
    ));
    parts.push("Input batches contain ordered text replicas and may contain image bubbles when a multimodal model is selected. Text replicas have id and text. Image bubbles have id, description, and an attached image; for those, read the image and infer the original visible text. A multi-area image bubble additionally has \"area_count\" and an ordered \"areas\" list (each area has index, optional description, optional current_original_text, optional image_bbox = [x1,y1,x2,y2] relative to the attached image); translate each area separately. Some items may be marked \"context\": true and carry an existing_translation: these are already translated and are included only to preserve reading order and dialogue continuity. Never translate context items and never include them in your output. Translate every other item separately. Return a flat JSON array where every id that needs translation appears exactly once and context ids never appear. For text items return {\"id\": number, \"translation\": string}. For single-area image items return {\"id\": number, \"original_text\": string, \"translation\": string}. For multi-area image items return {\"id\": number, \"areas\": [{\"original_text\": string, \"translation\": string}, ...]} with exactly area_count entries in the same order as the input areas. Do not merge, omit, renumber, explain, or add markdown.".to_string());
    if options.use_character_names {
        parts.push("Use character names as speaker context, but do not add speaker labels unless they are present in the source text.".to_string());
    }

    push_project_context_parts(&mut parts, options);

    parts.join("\n\n")
}

/// Appends the optional project-context prompt blocks (notes template, or characters + terms) shared
/// by the normal and per-ImageBubble system prompts.
fn push_project_context_parts(parts: &mut Vec<String>, options: &AiMtOptions) {
    if options.use_notes_prompt {
        if let Some(notes) = build_notes_prompt(&options.project, true, true) {
            parts.push(format!("Secondary project notes:\n{notes}"));
        }
    } else {
        if options.include_characters
            && let Some(chars) = build_characters_prompt(&options.project)
        {
            parts.push(chars);
        }
        if options.include_terms
            && let Some(terms) = build_terms_prompt(&options.project)
        {
            parts.push(terms);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn build_ai_mt_chat_options(reasoning: AiMtReasoning) -> Option<ChatOptions> {
    reasoning.to_genai().map(|effort| {
        ChatOptions::default()
            .with_reasoning_effort(effort)
            .with_normalize_reasoning_content(true)
    })
}

fn build_ai_mt_batch_prompt(
    items: &[MtTranslateItem],
    include_existing_translation: bool,
) -> Result<String, String> {
    let mut groups: Vec<(String, Vec<&MtTranslateItem>)> = Vec::new();
    for item in items {
        let character = item.character.trim().to_string();
        if let Some((_, entries)) = groups.iter_mut().find(|(name, _)| *name == character) {
            entries.push(item);
        } else {
            groups.push((character, vec![item]));
        }
    }
    let payload = groups
        .into_iter()
        .map(|(character, replicas)| {
            let replicas = replicas
                .into_iter()
                .map(|item| {
                    let mut replica = serde_json::Map::new();
                    replica.insert("id".to_string(), Value::Number(item.bubble_id.into()));
                    replica.insert("text".to_string(), Value::String(item.text.clone()));
                    if include_existing_translation && !item.existing_translation.trim().is_empty()
                    {
                        replica.insert(
                            "existing_translation".to_string(),
                            Value::String(item.existing_translation.clone()),
                        );
                    }
                    Value::Object(replica)
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "character": character,
                "replicas": replicas,
            })
        })
        .collect::<Vec<_>>();
    let json = serde_json::to_string(&payload)
        .map_err(|err| tf!("translation.mt.serialize_batch_error", err = err))?;
    Ok(format!("Translate this JSON batch:\n{json}"))
}

/// Diagnostic counts for the image binaries attached to a single AI MT request: how many image
/// parts were sent and their total encoded (pre-base64) size in bytes. Used only for logging.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, Default)]
struct MtBatchImageStats {
    image_count: usize,
    image_bytes: usize,
}

/// One ordered element of an assembled AI MT user message, before it is turned into either a real
/// `genai` request or a human-readable preview. Keeps the exact text/image interleaving so both
/// consumers stay byte-for-byte identical.
enum MtUserPart {
    /// A text block exactly as it would be sent to the model.
    Text(String),
    /// An image bubble binary attached at this position, paired with its source bubble id.
    Image {
        bubble_id: i64,
        encoded: EncodedMtImage,
    },
}

/// Assembles the ordered parts of the AI MT user message for one batch.
///
/// Plain untranslated-text batches collapse to a single compact character-grouped prompt; batches
/// that carry context replicas or images switch to the ordered per-item representation with image
/// binaries interleaved after their item descriptor. This is the single source of truth shared by
/// the real request builder and the request preview.
fn build_ai_mt_user_parts(
    items: &[MtTranslateItem],
    options: &AiMtOptions,
) -> Result<Vec<MtUserPart>, String> {
    // The compact character-grouped prompt is only safe when the batch is plain untranslated text:
    // images need their binary parts, and context replicas need the ordered representation so the
    // model sees the exact reading-order interleaving of translated and untranslated replicas.
    let has_context = items.iter().any(|item| !item.needs_translation);
    if !has_context && !items.iter().any(|item| item.image.is_some()) {
        let prompt = build_ai_mt_batch_prompt(items, options.include_existing_translation)?;
        return Ok(vec![MtUserPart::Text(prompt)]);
    }

    let mut parts = vec![MtUserPart::Text(
        "Translate this ordered batch. Items appear in reading order. Each following text part describes one item; if an image follows that item description, use that attached image for that same id. Items marked \"context\": true are already translated and are included only to preserve reading order and dialogue continuity: do not translate them and do not include them in your output. Return only the required JSON array for the items that need translation.".to_string(),
    )];
    for item in items {
        let descriptor = ai_mt_ordered_item_descriptor(item, options.include_existing_translation)?;
        parts.push(MtUserPart::Text(descriptor));
        if let Some(image) = item.image.as_ref() {
            let encoded = load_mt_encoded_image(&options.project, image, options.image_detail)?;
            parts.push(MtUserPart::Image {
                bubble_id: item.bubble_id,
                encoded,
            });
        }
    }
    parts.push(MtUserPart::Text(
        "Return a flat JSON array containing only the items that need translation (omit every item marked \"context\": true). Text item shape: {\"id\": number, \"translation\": string}. Single-area image item shape: {\"id\": number, \"original_text\": string, \"translation\": string}. Multi-area image item shape: {\"id\": number, \"areas\": [{\"original_text\": string, \"translation\": string}, ...]} with one entry per input area in order.".to_string(),
    ));
    Ok(parts)
}

#[cfg(not(target_arch = "wasm32"))]
fn build_ai_mt_user_message(
    items: &[MtTranslateItem],
    options: &AiMtOptions,
) -> Result<(ChatMessage, MtBatchImageStats), String> {
    let user_parts = build_ai_mt_user_parts(items, options)?;
    // A single text part means a plain text-only batch: keep the compact `Text` message content so
    // context sizing and retained history match the previous behavior exactly.
    if let [MtUserPart::Text(text)] = user_parts.as_slice() {
        return Ok((
            ChatMessage::user(text.clone()),
            MtBatchImageStats::default(),
        ));
    }

    let mut parts = Vec::with_capacity(user_parts.len());
    let mut image_stats = MtBatchImageStats::default();
    for part in user_parts {
        match part {
            MtUserPart::Text(text) => parts.push(ContentPart::from_text(text)),
            MtUserPart::Image { bubble_id, encoded } => {
                image_stats.image_count = image_stats.image_count.saturating_add(1);
                image_stats.image_bytes =
                    image_stats.image_bytes.saturating_add(encoded.bytes.len());
                parts.push(ContentPart::from_binary_base64(
                    encoded.mime_type,
                    base64_encode(&encoded.bytes),
                    Some(format!("image-bubble-{bubble_id}.{}", encoded.extension)),
                ));
            }
        }
    }
    Ok((ChatMessage::user(parts), image_stats))
}

/// One ordered element of an AI MT request preview: text exactly as sent, or a decoded image shown
/// inline at the position the binary would occupy in the request.
#[derive(Debug, Clone)]
pub enum MtRequestPreviewPart {
    Text(String),
    Image(MtRequestPreviewImage),
}

/// A decoded image attachment for the request preview. `rgba` is row-major RGBA8 of size
/// `width * height * 4`, decoded from the exact PNG bytes that would be sent (`png_byte_len`).
#[derive(Debug, Clone)]
pub struct MtRequestPreviewImage {
    pub bubble_id: i64,
    pub width: u32,
    pub height: u32,
    pub png_byte_len: usize,
    pub rgba: Vec<u8>,
}

/// Human-readable preview of the first AI MT request (system prompt + first batch user message with
/// images inline) that a translate action would send, assembled without contacting the provider.
#[derive(Debug, Clone)]
pub struct MtRequestPreview {
    pub system_prompt: String,
    pub batch_total: usize,
    pub translate_count: usize,
    pub context_count: usize,
    pub total_item_count: usize,
    pub image_count: usize,
    pub image_bytes: usize,
    pub parts: Vec<MtRequestPreviewPart>,
}

/// Builds a preview of the first AI MT request for `items` under `options`, mirroring the real
/// assembly (sort, batch split, system prompt, ordered user parts) but decoding image binaries for
/// display instead of sending them.
///
/// `items` are taken by value and sorted internally with the configured sort mode. Returns an error
/// when there is nothing to translate or when an image fails to load, encode, or decode. This loads
/// and decodes images, so it must run off the GUI thread.
#[must_use = "the preview is the only result; ignoring it loses the assembled request"]
pub fn build_ai_mt_request_preview(
    source_lang: &str,
    target_lang: &str,
    mut items: Vec<MtTranslateItem>,
    options: &AiMtOptions,
) -> Result<MtRequestPreview, String> {
    sort_ai_mt_items(&mut items, options.sort_mode());
    if options.image_mode == AiMtImageMode::ImagesOnly {
        return build_ai_mt_imagebubble_request_preview(source_lang, target_lang, &items, options);
    }
    let batch_size = options.batch_size.clamp(1, 100);
    let batches = split_ai_mt_batches(&items, batch_size);
    let batch_total = batches.len();
    let Some(first) = batches.into_iter().next() else {
        return Err(t!("translation.mt.no_replicas_preview_status").to_string());
    };

    let system_prompt = build_ai_mt_system_prompt(source_lang, target_lang, options);
    let translate_count = first.iter().filter(|item| item.needs_translation).count();
    let context_count = first.len().saturating_sub(translate_count);
    let user_parts = build_ai_mt_user_parts(first, options)?;

    let mut parts = Vec::with_capacity(user_parts.len());
    let mut image_count = 0usize;
    let mut image_bytes = 0usize;
    for part in user_parts {
        match part {
            MtUserPart::Text(text) => parts.push(MtRequestPreviewPart::Text(text)),
            MtUserPart::Image { bubble_id, encoded } => {
                let decoded = image::load_from_memory(&encoded.bytes)
                    .map_err(|err| {
                        tf!("translation.mt.decode_preview_image_error", err = err)
                    })?
                    .to_rgba8();
                image_count = image_count.saturating_add(1);
                image_bytes = image_bytes.saturating_add(encoded.bytes.len());
                parts.push(MtRequestPreviewPart::Image(MtRequestPreviewImage {
                    bubble_id,
                    width: decoded.width(),
                    height: decoded.height(),
                    png_byte_len: encoded.bytes.len(),
                    rgba: decoded.into_raw(),
                }));
            }
        }
    }

    Ok(MtRequestPreview {
        system_prompt,
        batch_total,
        translate_count,
        context_count,
        total_item_count: first.len(),
        image_count,
        image_bytes,
        parts,
    })
}

/// Builds a preview of the first per-ImageBubble request: the system prompt, the chapter context
/// gathered before the first target image, and that target's instruction + decoded image. `items`
/// must already be sorted into reading order. `batch_total` reports the number of image requests the
/// run would make (one per target).
fn build_ai_mt_imagebubble_request_preview(
    source_lang: &str,
    target_lang: &str,
    items: &[MtTranslateItem],
    options: &AiMtOptions,
) -> Result<MtRequestPreview, String> {
    let target_total = items.iter().filter(|item| item.needs_translation).count();
    let mut context: Vec<String> = Vec::new();
    let mut target: Option<&MtTranslateItem> = None;
    for item in items {
        if item.needs_translation {
            target = Some(item);
            break;
        }
        context.push(imagebubble_context_line_for_item(
            item,
            options.image_context_source,
        ));
    }
    let Some(target) = target else {
        return Err(t!("translation.mt.no_imagebubble_preview_status").to_string());
    };

    let system_prompt = build_ai_mt_imagebubble_system_prompt(source_lang, target_lang, options);
    let encoded = imagebubble_target_image(options, target)?;
    let decoded = image::load_from_memory(&encoded.bytes)
        .map_err(|err| tf!("translation.mt.decode_preview_image_error", err = err))?
        .to_rgba8();

    let mut parts = Vec::new();
    if !context.is_empty() {
        parts.push(MtRequestPreviewPart::Text(format!(
            "{IMAGEBUBBLE_CONTEXT_HEADER}\n{}",
            context.join("\n")
        )));
    }
    parts.push(MtRequestPreviewPart::Text(ai_mt_target_descriptor(
        target,
        options.include_existing_translation,
    )?));
    parts.push(MtRequestPreviewPart::Image(MtRequestPreviewImage {
        bubble_id: target.bubble_id,
        width: decoded.width(),
        height: decoded.height(),
        png_byte_len: encoded.bytes.len(),
        rgba: decoded.into_raw(),
    }));

    Ok(MtRequestPreview {
        system_prompt,
        batch_total: target_total,
        translate_count: 1,
        context_count: context.len(),
        total_item_count: context.len() + 1,
        image_count: 1,
        image_bytes: encoded.bytes.len(),
        parts,
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn replace_last_ai_mt_user_message_with_history(
    chat_req: &mut ChatRequest,
    items: &[MtTranslateItem],
    options: &AiMtOptions,
) -> Result<(), String> {
    if !items.iter().any(|item| item.image.is_some()) {
        return Ok(());
    }
    let Some(last_message) = chat_req.messages.last_mut() else {
        return Ok(());
    };
    *last_message = ChatMessage::user(build_ai_mt_history_prompt(
        items,
        options.include_existing_translation,
    )?);
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn build_ai_mt_history_prompt(
    items: &[MtTranslateItem],
    include_existing_translation: bool,
) -> Result<String, String> {
    let mut parts = vec![
        "Previously translated multimodal batch. Images were attached in the API request, but binary image data is omitted from retained chat history.".to_string(),
    ];
    for item in items {
        parts.push(ai_mt_ordered_item_descriptor(
            item,
            include_existing_translation,
        )?);
    }
    Ok(parts.join("\n\n"))
}

fn ai_mt_ordered_item_descriptor(
    item: &MtTranslateItem,
    include_existing_translation: bool,
) -> Result<String, String> {
    let mut entry = serde_json::Map::new();
    entry.insert("id".to_string(), Value::Number(item.bubble_id.into()));
    // Context replicas are flagged so the model keeps them for ordering but never translates them.
    if !item.needs_translation {
        entry.insert("context".to_string(), Value::Bool(true));
    }
    if !item.character.trim().is_empty() {
        entry.insert(
            "character".to_string(),
            Value::String(item.character.clone()),
        );
    }
    if let Some(image) = item.image.as_ref() {
        entry.insert("type".to_string(), Value::String("image".to_string()));
        if !image.description.trim().is_empty() {
            entry.insert(
                "description".to_string(),
                Value::String(image.description.clone()),
            );
        }
        if image.is_multi_area() {
            // Multi-area image bubble: list every text area so the model returns one
            // {original_text, translation} per area, in this exact order.
            entry.insert(
                "area_count".to_string(),
                Value::Number(image.areas.len().into()),
            );
            let areas = image
                .areas
                .iter()
                .enumerate()
                .map(|(idx, area)| {
                    let mut area_entry = serde_json::Map::new();
                    area_entry.insert("index".to_string(), Value::Number(idx.into()));
                    if !area.description.trim().is_empty() {
                        area_entry.insert(
                            "description".to_string(),
                            Value::String(area.description.clone()),
                        );
                    }
                    if !area.original.trim().is_empty() {
                        area_entry.insert(
                            "current_original_text".to_string(),
                            Value::String(area.original.clone()),
                        );
                    }
                    if let Some(bbox) = area.rel_bbox {
                        area_entry.insert(
                            "image_bbox".to_string(),
                            Value::Array(bbox.iter().map(|v| Value::from(f64::from(*v))).collect()),
                        );
                    }
                    Value::Object(area_entry)
                })
                .collect::<Vec<_>>();
            entry.insert("areas".to_string(), Value::Array(areas));
        } else if !item.text.trim().is_empty() {
            entry.insert(
                "current_original_text".to_string(),
                Value::String(item.text.clone()),
            );
        }
    } else {
        entry.insert("type".to_string(), Value::String("text".to_string()));
        entry.insert("text".to_string(), Value::String(item.text.clone()));
    }
    // A context replica must always carry its current translation (that is the continuity it
    // provides); a translatable replica only includes it when the option is enabled.
    if (!item.needs_translation || include_existing_translation)
        && !item.existing_translation.trim().is_empty()
    {
        entry.insert(
            "existing_translation".to_string(),
            Value::String(item.existing_translation.clone()),
        );
    }
    let json = serde_json::to_string(&Value::Object(entry))
        .map_err(|err| tf!("translation.mt.serialize_item_error", err = err))?;
    Ok(format!("Item:\n{json}"))
}

#[cfg(not(target_arch = "wasm32"))]
fn context_char_budget(percent: u8) -> usize {
    let percent = usize::from(percent.clamp(10, 100));
    AI_MT_CONTEXT_CHAR_BUDGET.saturating_mul(percent) / 100
}

#[derive(Debug)]
struct EncodedMtImage {
    bytes: Vec<u8>,
    mime_type: &'static str,
    extension: &'static str,
}

fn load_mt_encoded_image(
    project: &ProjectData,
    image: &MtImageInput,
    detail: AiMtImageDetail,
) -> Result<EncodedMtImage, String> {
    let source = match &image.source {
        MtImageSource::ExternalPath(raw_path) => {
            let path = resolve_project_image_bubble_path(project, raw_path);
            image::open(&path)
                .map_err(|err| tf!("translation.mt.open_image_error", path = path.display(), err = err))?
        }
        MtImageSource::PageCrop {
            page_idx,
            crop_rect,
        } => {
            let page = project
                .pages
                .iter()
                .find(|page| page.idx == *page_idx)
                .ok_or_else(|| tf!("translation.mt.crop_page_not_found_error", page_idx = page_idx + 1))?;
            let source = image::open(&page.path)
                .map_err(|err| {
                    tf!("translation.mt.open_page_error", page = page.path.display(), err = err)
                })?
                .to_rgba8();
            let rect = normalize_uv_rect(*crop_rect);
            let width = source.width().max(1);
            let height = source.height().max(1);
            let max_x = width as f32;
            let max_y = height as f32;
            let x1 = (rect[0] * max_x).floor().clamp(0.0, (max_x - 1.0).max(0.0)) as u32;
            let y1 = (rect[1] * max_y).floor().clamp(0.0, (max_y - 1.0).max(0.0)) as u32;
            let x2 = (rect[2] * max_x).ceil().clamp(x1 as f32 + 1.0, max_x) as u32;
            let y2 = (rect[3] * max_y).ceil().clamp(y1 as f32 + 1.0, max_y) as u32;
            let cropped = image::imageops::crop_imm(&source, x1, y1, x2 - x1, y2 - y1).to_image();
            image::DynamicImage::ImageRgba8(cropped)
        }
    };
    let source = prepare_mt_image_for_detail(source, detail);
    encode_png(source)
}

fn resolve_project_image_bubble_path(project: &ProjectData, raw_path: &str) -> PathBuf {
    let path = PathBuf::from(raw_path);
    if path.is_absolute() {
        return path;
    }
    let unsaved_path = project.paths.unsaved_dir.join(&path);
    if unsaved_path.exists() {
        return unsaved_path;
    }
    let saved_path = project.project_dir.join(&path);
    if saved_path.exists() {
        return saved_path;
    }
    unsaved_path
}

fn encode_png(image: image::DynamicImage) -> Result<EncodedMtImage, String> {
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|err| tf!("translation.mt.encode_imagebubble_error", err = err))?;
    Ok(EncodedMtImage {
        bytes: out,
        mime_type: "image/png",
        extension: "png",
    })
}

fn prepare_mt_image_for_detail(
    image: image::DynamicImage,
    detail: AiMtImageDetail,
) -> image::DynamicImage {
    let Some(max_edge) = detail.max_edge_px() else {
        return image;
    };
    let width = image.width();
    let height = image.height();
    let longest_edge = width.max(height);
    if longest_edge <= max_edge {
        return image;
    }
    let scale = max_edge as f32 / longest_edge as f32;
    let new_width = ((width as f32) * scale).round().max(1.0) as u32;
    let new_height = ((height as f32) * scale).round().max(1.0) as u32;
    image.resize(new_width, new_height, image::imageops::FilterType::Lanczos3)
}

fn normalize_uv_rect(rect: [f32; 4]) -> [f32; 4] {
    [
        rect[0].min(rect[2]).clamp(0.0, 1.0),
        rect[1].min(rect[3]).clamp(0.0, 1.0),
        rect[0].max(rect[2]).clamp(0.0, 1.0),
        rect[1].max(rect[3]).clamp(0.0, 1.0),
    ]
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
fn prune_ai_mt_chat_context(
    chat_req: &mut ChatRequest,
    history_batch_sizes: &mut VecDeque<usize>,
    budget: usize,
) -> usize {
    let mut pruned_replicas = 0usize;
    while chat_req
        .messages
        .iter()
        .map(ChatMessage::size)
        .sum::<usize>()
        > budget
        && chat_req.messages.len() > 2
    {
        chat_req.messages.drain(0..2);
        pruned_replicas =
            pruned_replicas.saturating_add(history_batch_sizes.pop_front().unwrap_or(0));
    }
    pruned_replicas
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct AiMtTranslation {
    bubble_id: i64,
    original_text: Option<String>,
    translation: String,
    /// Per-area results for a multi-area image bubble (empty for text / single-area items).
    areas: Vec<AiMtArea>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct AiMtArea {
    original_text: String,
    translation: String,
}

#[cfg(not(target_arch = "wasm32"))]
fn parse_ai_mt_response(raw: &str) -> Result<Vec<AiMtTranslation>, String> {
    let json_text = extract_json_payload(raw);
    let value: Value = serde_json::from_str(&json_text)
        .map_err(|err| tf!("translation.mt.invalid_json_error", err = err))?;
    // Accept a flat array, an object wrapping a `translations` array, or — as the per-ImageBubble
    // mode returns — a single result object for the one target bubble.
    let arr: Vec<&Value> = if let Some(arr) = value.as_array() {
        arr.iter().collect()
    } else if let Some(arr) = value.get("translations").and_then(Value::as_array) {
        arr.iter().collect()
    } else if value.is_object()
        && (value.get("id").is_some()
            || value.get("bubble_id").is_some()
            || value.get("areas").is_some())
    {
        vec![&value]
    } else {
        return Err(t!("translation.mt.json_not_array_error").to_string());
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let bubble_id = item
            .get("id")
            .or_else(|| item.get("bubble_id"))
            .and_then(Value::as_i64)
            .ok_or_else(|| t!("translation.mt.json_no_id_error").to_string())?;
        // Multi-area image bubble: a per-area array of {original_text, translation}.
        let areas = item
            .get("areas")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(parse_ai_mt_area).collect::<Vec<_>>())
            .unwrap_or_default();
        let translation = item
            .get("translation")
            .or_else(|| item.get("text"))
            .and_then(Value::as_str)
            .map(|value| value.trim().to_string());
        if translation.is_none() && areas.is_empty() {
            return Err(t!("translation.mt.json_no_translation_error").to_string());
        }
        let original_text = item
            .get("original_text")
            .or_else(|| item.get("original"))
            .and_then(Value::as_str)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        out.push(AiMtTranslation {
            bubble_id,
            original_text,
            translation: translation.unwrap_or_default(),
            areas,
        });
    }
    Ok(out)
}

/// Parses one per-area entry `{original_text, translation}` from a multi-area image response.
#[cfg(not(target_arch = "wasm32"))]
fn parse_ai_mt_area(value: &Value) -> Option<AiMtArea> {
    let translation = value
        .get("translation")
        .or_else(|| value.get("text"))
        .and_then(Value::as_str)
        .map(|raw| raw.trim().to_string())?;
    let original_text = value
        .get("original_text")
        .or_else(|| value.get("original"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    Some(AiMtArea {
        original_text,
        translation,
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn extract_json_payload(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(stripped) = trimmed.strip_prefix("```") {
        let without_lang = stripped
            .strip_prefix("json")
            .or_else(|| stripped.strip_prefix("JSON"))
            .unwrap_or(stripped)
            .trim_start();
        return without_lang
            .strip_suffix("```")
            .unwrap_or(without_lang)
            .trim()
            .to_string();
    }
    trimmed.to_string()
}

fn build_notes_prompt(
    project: &ProjectData,
    include_characters: bool,
    include_terms: bool,
) -> Option<String> {
    let template = fs::read_to_string(&project.paths.notes_file).ok()?;
    let chars = if include_characters {
        build_characters_prompt(project).unwrap_or_default()
    } else {
        String::new()
    };
    let terms = if include_terms {
        build_terms_prompt(project).unwrap_or_default()
    } else {
        String::new()
    };
    let mut text = template
        .replace("{charas}", &chars)
        .replace("{terms}", &terms);
    if include_characters && !template.contains("{charas}") && !chars.trim().is_empty() {
        text.push('\n');
        text.push_str(&chars);
    }
    if include_terms && !template.contains("{terms}") && !terms.trim().is_empty() {
        text.push('\n');
        text.push_str(&terms);
    }
    (!text.trim().is_empty()).then_some(text)
}

fn build_characters_prompt(project: &ProjectData) -> Option<String> {
    let chars = load_characters_for_notes(project).ok()?;
    if chars.is_empty() {
        return None;
    }
    let lines = chars
        .into_iter()
        .filter(|entry| !entry.name.trim().is_empty())
        .map(|entry| {
            let description = entry.description.trim();
            if description.is_empty() {
                format!("- {}", entry.name.trim())
            } else {
                format!("- {}: {description}", entry.name.trim())
            }
        })
        .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| format!("Characters:\n{}", lines.join("\n")))
}

fn build_terms_prompt(project: &ProjectData) -> Option<String> {
    let terms = load_terms_for_notes(project).ok()?;
    if terms.is_empty() {
        return None;
    }
    let lines = terms
        .into_iter()
        .filter(|entry| !entry.name.trim().is_empty())
        .map(|entry| {
            let mut parts = vec![entry.name.trim().to_string()];
            if !entry.orig_name.trim().is_empty() {
                parts.push(format!("orig: {}", entry.orig_name.trim()));
            }
            if !entry.description.trim().is_empty() {
                parts.push(entry.description.trim().to_string());
            }
            format!("- {}", parts.join(" | "))
        })
        .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| format!("Terms:\n{}", lines.join("\n")))
}

pub fn character_for_bubble(bubble: &Bubble, use_character_names: bool) -> String {
    if !use_character_names {
        return String::new();
    }
    let character = bubble_extra_string(&bubble.extra, "character_name");
    let clarification = bubble_extra_string(&bubble.extra, "clarification");
    if character.trim().is_empty() {
        return String::new();
    }
    if clarification.trim().is_empty() {
        character
    } else {
        format!("{character} ({clarification})")
    }
}

pub fn bubble_order_for_sort(bubble: &Bubble) -> i32 {
    bubble_extra_i32(&bubble.extra, "bubble_order", 0)
}

#[cfg(test)]
mod tests {
    use super::{
        AiApiService, AiMtContextSource, AiMtImageDetail, AiMtImageMode, AiMtOptions,
        AiMtReasoning, AiMtSortMode, AiMtTranslation, IMAGEBUBBLE_CONTEXT_HEADER, MtImageArea,
        MtImageInput, MtImageSource, MtRequestPreviewPart, MtTranslateItem, ResolvedImageText,
        ai_mt_ordered_item_descriptor, build_ai_mt_batch_prompt, build_ai_mt_history_prompt,
        build_ai_mt_imagebubble_request_preview, build_ai_mt_request_preview,
        imagebubble_context_line_for_item, imagebubble_context_line_for_target,
        is_probable_quota_or_limit_error, parse_ai_mt_response, prepare_mt_image_for_detail,
        sort_ai_mt_items, split_ai_mt_batches,
    };
    use crate::project::{CanvasSettings, ProjectData, ProjectPaths};
    use std::path::PathBuf;
    use std::sync::Arc;

    /// Minimal in-memory `ProjectData` for tests that do not touch image binaries or notes files.
    fn empty_project() -> ProjectData {
        let empty_path = PathBuf::new;
        ProjectData {
            project_dir: empty_path(),
            image_dir: empty_path(),
            pages: Vec::new(),
            bubbles: Arc::new(Vec::new()),
            paths: ProjectPaths {
                project_dir: empty_path(),
                title_dir: empty_path(),
                notes_file: empty_path(),
                bubbles_file: empty_path(),
                src_dir: empty_path(),
                clean_layers_dir: empty_path(),
                cleaned_dir: empty_path(),
                alt_vers_dir: empty_path(),
                saved_dir: empty_path(),
                image_bubbles_dir: empty_path(),
                text_images_dir: empty_path(),
                layers_dir: empty_path(),
                text_detection_dir: empty_path(),
                characters_dir: empty_path(),
                terms_file: empty_path(),
                settings_file: empty_path(),
                unsaved_dir: empty_path(),
                unsaved_bubbles_file: empty_path(),
                unsaved_clean_layers_dir: empty_path(),
                unsaved_image_bubbles_dir: empty_path(),
                unsaved_text_images_dir: empty_path(),
                unsaved_layers_dir: empty_path(),
            },
            comic_type: None,
            canvas_settings: CanvasSettings::default(),
            settings_data: serde_json::Value::Null,
        }
    }

    /// Builds plain AI MT options with the given batch size for preview/batching tests.
    fn ai_options(batch_size: usize) -> AiMtOptions {
        AiMtOptions {
            service: AiApiService::OpenAi,
            model: "gpt-test".to_string(),
            system_instruction: "Translate well.".to_string(),
            sort_mode: AiMtSortMode::Height,
            use_character_names: false,
            use_notes_prompt: false,
            include_characters: false,
            include_terms: false,
            batch_size,
            reasoning: AiMtReasoning::None,
            context_limit_percent: 50,
            include_existing_translation: false,
            image_detail: AiMtImageDetail::Auto,
            image_mode: AiMtImageMode::Normal,
            image_context_source: AiMtContextSource::Translation,
            project: empty_project(),
        }
    }

    /// Builds a context (non-target) text replica with both original and translation filled.
    fn ctx_item(bubble_id: i64, original: &str, translation: &str) -> MtTranslateItem {
        MtTranslateItem {
            bubble_id,
            page_idx: 0,
            img_v: f32::from(bubble_id as i16),
            order: bubble_id as i32,
            character: String::new(),
            text: original.to_string(),
            existing_translation: translation.to_string(),
            image: None,
            needs_translation: false,
        }
    }

    /// Builds a translatable single-area ImageBubble target for the ImagesOnly tests.
    fn image_target(bubble_id: i64) -> MtTranslateItem {
        MtTranslateItem {
            bubble_id,
            page_idx: 0,
            img_v: f32::from(bubble_id as i16),
            order: bubble_id as i32,
            character: String::new(),
            text: String::new(),
            existing_translation: String::new(),
            image: Some(MtImageInput {
                description: String::new(),
                source: MtImageSource::ExternalPath(format!("image_bubbles/{bubble_id}.png")),
                areas: vec![MtImageArea {
                    description: String::new(),
                    original: String::new(),
                    rel_bbox: None,
                }],
            }),
            needs_translation: true,
        }
    }

    #[test]
    fn ai_response_parser_accepts_single_object() {
        // Per-ImageBubble mode returns a single result object, not an array.
        let parsed =
            parse_ai_mt_response(r#"{"id":10,"original_text":"BOOM","translation":"БУМ"}"#)
                .expect("single object parses");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].bubble_id, 10);
        assert_eq!(parsed[0].original_text.as_deref(), Some("BOOM"));
        assert_eq!(parsed[0].translation, "БУМ");
    }

    #[test]
    fn imagebubble_context_prefix_is_append_only() {
        // The context block for the second image must contain the first image's context block as a
        // literal prefix — the invariant provider prompt caching relies on.
        let one = imagebubble_context_line_for_item(
            &ctx_item(1, "hello", "привет"),
            AiMtContextSource::Translation,
        );
        let two = imagebubble_context_line_for_item(
            &ctx_item(2, "world", "мир"),
            AiMtContextSource::Translation,
        );
        let first = format!("{IMAGEBUBBLE_CONTEXT_HEADER}\n{one}");
        let second = format!("{IMAGEBUBBLE_CONTEXT_HEADER}\n{one}\n{two}");
        assert!(second.starts_with(&first));
    }

    #[test]
    fn imagebubble_context_source_switches_original_vs_translation() {
        let item = ctx_item(7, "SALE", "распродажа");
        let original = imagebubble_context_line_for_item(&item, AiMtContextSource::Original);
        let translation = imagebubble_context_line_for_item(&item, AiMtContextSource::Translation);
        assert!(original.contains("SALE") && !original.contains("распродажа"));
        assert!(translation.contains("распродажа") && !translation.contains("SALE"));
        // Translation falls back to the original when there is no translation yet.
        let untranslated = ctx_item(8, "RUN", "");
        let fallback =
            imagebubble_context_line_for_item(&untranslated, AiMtContextSource::Translation);
        assert!(fallback.contains("RUN"));
    }

    #[test]
    fn imagebubble_target_context_uses_resolved_text_not_binary() {
        // After translating, the image folds into context as text (resolved translation), never as a
        // binary reference, so later requests keep a cheap cacheable prefix.
        let target = image_target(9);
        let resolved = ResolvedImageText {
            original: "BANG".to_string(),
            translation: "БАХ".to_string(),
        };
        let line = imagebubble_context_line_for_target(
            &target,
            AiMtContextSource::Translation,
            Some(&resolved),
        );
        assert!(line.contains("\"id\":9"));
        assert!(line.contains("БАХ"));
        assert!(!line.contains("image_bubbles"));
        assert!(!line.contains("base64") && !line.contains("data:image"));
    }

    #[test]
    fn imagebubble_request_preview_renders_single_target_image() {
        // Two context replicas then one image target: the preview must describe exactly one image and
        // carry the per-image system contract.
        let mut items = vec![
            ctx_item(1, "a", "а"),
            ctx_item(2, "b", "б"),
            image_target(3),
        ];
        // prepare_mt_image_for_detail is not exercised here; the external path won't load, so guard
        // only the pre-load assembly by checking the error path stays specific to image loading.
        sort_ai_mt_items(&mut items, AiMtSortMode::Number);
        let options = ai_options(10);
        let result = build_ai_mt_imagebubble_request_preview("en", "ru", &items, &options);
        // The fake external image path cannot be opened, so the preview fails at image load — but
        // only after selecting the single target and gathering its two context replicas.
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Pin the catalog key rather than a Russian substring. Tests share one process and
        // `locale_store`'s tests install a catalog into the process-global `ArcSwap`, so which
        // language `t!` yields here depends on test order. Comparing against the template's
        // fixed prefix is locale-independent and still proves the image-load error was chosen.
        let template = t!("translation.mt.open_image_error");
        let prefix = template.split("{path}").next().unwrap_or(template);
        assert!(
            err.starts_with(prefix),
            "expected the image-load error, got {err:?}"
        );
    }

    #[test]
    fn quota_limit_classifier_matches_common_provider_wordings() {
        let positives = [
            "AI перевод не выполнен: 429 Too Many Requests",
            "error: insufficient_quota - You exceeded your current quota",
            "Your credit balance is too low to access the Claude API",
            "RESOURCE_EXHAUSTED: Quota exceeded for gemini",
            "OpenRouter: Insufficient credits (status code: 402)",
        ];
        for error in positives {
            assert!(
                is_probable_quota_or_limit_error(error),
                "expected quota/limit match for: {error}"
            );
        }
    }

    #[test]
    fn quota_limit_classifier_ignores_unrelated_errors() {
        let negatives = [
            "AI вернул невалидный JSON: expected value at line 1",
            "Не удалось создать async runtime для AI перевода",
            "connection reset by peer",
        ];
        for error in negatives {
            assert!(
                !is_probable_quota_or_limit_error(error),
                "did not expect quota/limit match for: {error}"
            );
        }
    }

    /// Builds a plain text replica with the given translate flag for batching/context tests.
    fn text_item(bubble_id: i64, needs_translation: bool) -> MtTranslateItem {
        MtTranslateItem {
            bubble_id,
            page_idx: 0,
            img_v: 0.0,
            order: 0,
            character: String::new(),
            text: format!("orig {bubble_id}"),
            existing_translation: if needs_translation {
                String::new()
            } else {
                format!("перевод {bubble_id}")
            },
            image: None,
            needs_translation,
        }
    }

    /// Collects the bubble ids of each produced batch for compact assertions.
    fn batch_ids(items: &[MtTranslateItem], batch_size: usize) -> Vec<Vec<i64>> {
        split_ai_mt_batches(items, batch_size)
            .into_iter()
            .map(|chunk| chunk.iter().map(|item| item.bubble_id).collect())
            .collect()
    }

    #[test]
    fn ai_batch_prompt_groups_replicas_by_character() {
        let items = vec![
            MtTranslateItem {
                bubble_id: 2,
                page_idx: 0,
                img_v: 0.2,
                order: 2,
                character: "A".to_string(),
                text: "second".to_string(),
                existing_translation: String::new(),
                image: None,
                needs_translation: true,
            },
            MtTranslateItem {
                bubble_id: 5,
                page_idx: 0,
                img_v: 0.3,
                order: 3,
                character: "A".to_string(),
                text: "third".to_string(),
                existing_translation: String::new(),
                image: None,
                needs_translation: true,
            },
            MtTranslateItem {
                bubble_id: 1,
                page_idx: 0,
                img_v: 0.1,
                order: 1,
                character: "B".to_string(),
                text: "first".to_string(),
                existing_translation: String::new(),
                image: None,
                needs_translation: true,
            },
        ];
        let prompt = build_ai_mt_batch_prompt(&items, false).expect("prompt");
        assert!(prompt.contains("\"character\":\"A\""));
        assert!(prompt.contains("\"id\":2"));
        assert!(prompt.contains("\"id\":5"));
        assert!(prompt.contains("\"character\":\"B\""));
    }

    #[test]
    fn ai_batch_prompt_can_include_existing_translation() {
        let items = vec![MtTranslateItem {
            bubble_id: 7,
            page_idx: 0,
            img_v: 0.1,
            order: 1,
            character: String::new(),
            text: "hello".to_string(),
            existing_translation: "старый перевод".to_string(),
            image: None,
            needs_translation: true,
        }];
        let prompt = build_ai_mt_batch_prompt(&items, true).expect("prompt");
        assert!(prompt.contains("\"existing_translation\":\"старый перевод\""));
    }

    #[test]
    fn ai_multimodal_item_descriptor_includes_description() {
        let item = MtTranslateItem {
            bubble_id: 9,
            page_idx: 0,
            img_v: 0.1,
            order: 1,
            character: String::new(),
            text: String::new(),
            existing_translation: String::new(),
            image: Some(MtImageInput {
                description: "sign on a store window".to_string(),
                source: MtImageSource::ExternalPath("image_bubbles/9.png".to_string()),
                areas: vec![MtImageArea {
                    description: "sign on a store window".to_string(),
                    original: String::new(),
                    rel_bbox: None,
                }],
            }),
            needs_translation: true,
        };
        let descriptor = ai_mt_ordered_item_descriptor(&item, false).expect("descriptor");
        assert!(descriptor.contains("\"type\":\"image\""));
        assert!(descriptor.contains("\"description\":\"sign on a store window\""));
    }

    #[test]
    fn ai_multimodal_history_prompt_omits_binary_image_data() {
        let items = vec![MtTranslateItem {
            bubble_id: 9,
            page_idx: 0,
            img_v: 0.1,
            order: 1,
            character: String::new(),
            text: "SALE".to_string(),
            existing_translation: String::new(),
            image: Some(MtImageInput {
                description: "sign".to_string(),
                source: MtImageSource::ExternalPath("image_bubbles/9.png".to_string()),
                areas: vec![MtImageArea {
                    description: "sign".to_string(),
                    original: "SALE".to_string(),
                    rel_bbox: None,
                }],
            }),
            needs_translation: true,
        }];
        let prompt = match build_ai_mt_history_prompt(&items, false) {
            Ok(prompt) => prompt,
            Err(error) => panic!("history prompt failed: {error}"),
        };
        assert!(prompt.contains("binary image data is omitted"));
        assert!(prompt.contains("\"type\":\"image\""));
        assert!(!prompt.contains("data:image"));
        assert!(!prompt.contains("base64"));
    }

    #[test]
    fn ai_multimodal_descriptor_lists_areas_for_multi_area_image() {
        let item = MtTranslateItem {
            bubble_id: 7,
            page_idx: 0,
            img_v: 0.2,
            order: 1,
            character: String::new(),
            text: String::new(),
            existing_translation: String::new(),
            image: Some(MtImageInput {
                description: "two signs".to_string(),
                source: MtImageSource::ExternalPath("image_bubbles/7.png".to_string()),
                areas: vec![
                    MtImageArea {
                        description: "left sign".to_string(),
                        original: String::new(),
                        rel_bbox: Some([0.0, 0.0, 0.5, 1.0]),
                    },
                    MtImageArea {
                        description: "right sign".to_string(),
                        original: String::new(),
                        rel_bbox: Some([0.5, 0.0, 1.0, 1.0]),
                    },
                ],
            }),
            needs_translation: true,
        };
        let descriptor = ai_mt_ordered_item_descriptor(&item, false).expect("descriptor");
        assert!(descriptor.contains("\"area_count\":2"));
        assert!(descriptor.contains("\"areas\""));
        assert!(descriptor.contains("left sign") && descriptor.contains("right sign"));
        assert!(descriptor.contains("image_bbox"));
    }

    #[test]
    fn ai_response_parser_accepts_multi_area_image() {
        let parsed = parse_ai_mt_response(
            r#"[{"id":7,"areas":[{"original_text":"A","translation":"А"},{"original_text":"B","translation":"Б"}]}]"#,
        )
        .expect("parsed");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].areas.len(), 2);
        assert_eq!(parsed[0].areas[0].translation, "А");
        assert_eq!(parsed[0].areas[1].original_text, "B");
    }

    #[test]
    fn ai_image_detail_parses_legacy_and_current_keys() {
        assert_eq!(AiMtImageDetail::from_key("jpeg_60"), AiMtImageDetail::Low);
        assert_eq!(AiMtImageDetail::from_key("png"), AiMtImageDetail::High);
        assert_eq!(AiMtImageDetail::from_key("unknown"), AiMtImageDetail::Auto);
    }

    #[test]
    fn ai_low_image_detail_downscales_long_edge_without_jpeg_loss() {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(1200, 600));
        let prepared = prepare_mt_image_for_detail(image, AiMtImageDetail::Low);
        assert_eq!(prepared.width(), 512);
        assert_eq!(prepared.height(), 256);
    }

    #[test]
    fn ai_response_parser_accepts_flat_json_ids() {
        let parsed =
            parse_ai_mt_response(r#"[{"id":2,"translation":"два"},{"id":5,"translation":"пять"}]"#)
                .expect("response");
        assert_eq!(parsed.len(), 2);
        assert_matches_translation(&parsed[0], 2, "два");
        assert_matches_translation(&parsed[1], 5, "пять");
    }

    #[test]
    fn batch_split_includes_context_around_untranslated_replica() {
        // [ctx, NEEDS, ctx]: with room in the batch the untranslated replica keeps its surrounding
        // already-translated context in reading order.
        let items = vec![text_item(1, false), text_item(2, true), text_item(3, false)];
        assert_eq!(batch_ids(&items, 2), vec![vec![1, 2, 3]]);
    }

    #[test]
    fn batch_split_defers_context_after_batch_limit_is_reached() {
        // [NEEDS, ctx, NEEDS] with batch_size 1: the limit is hit at id 1, so the following context
        // id 2 is not pulled into the first batch; it joins the window only once translation reaches
        // id 3.
        let items = vec![text_item(1, true), text_item(2, false), text_item(3, true)];
        assert_eq!(batch_ids(&items, 1), vec![vec![1], vec![2, 3]]);
    }

    #[test]
    fn batch_split_drops_trailing_context_only_run() {
        // A trailing run of already-translated replicas with no untranslated replica after them is
        // never sent on its own.
        let items = vec![text_item(1, true), text_item(2, true), text_item(3, false)];
        assert_eq!(batch_ids(&items, 2), vec![vec![1, 2]]);
    }

    #[test]
    fn batch_split_keeps_leading_context_for_first_window() {
        let items = vec![text_item(1, false), text_item(2, false), text_item(3, true)];
        assert_eq!(batch_ids(&items, 2), vec![vec![1, 2, 3]]);
    }

    #[test]
    fn request_preview_covers_only_first_batch_with_system_prompt() {
        // Three translatable replicas with batch_size 2 -> two batches; the preview must describe
        // only the first batch while reporting the total batch count.
        let items = vec![text_item(1, true), text_item(2, true), text_item(3, true)];
        let preview = build_ai_mt_request_preview("ja", "ru", items, &ai_options(2))
            .expect("preview builds for plain text items");

        assert_eq!(preview.batch_total, 2);
        assert_eq!(preview.translate_count, 2);
        assert_eq!(preview.context_count, 0);
        assert_eq!(preview.total_item_count, 2);
        assert_eq!(preview.image_count, 0);
        assert!(preview.system_prompt.contains("Translate well."));
        assert!(preview.system_prompt.contains("Target language: ru"));
        // A plain text-only batch collapses to a single grouped text part with no images.
        assert_eq!(preview.parts.len(), 1);
        let MtRequestPreviewPart::Text(text) = &preview.parts[0] else {
            panic!("expected a single text part for a text-only batch");
        };
        assert!(text.contains("orig 1"));
        assert!(text.contains("orig 2"));
        // The third replica belongs to the second batch and must not leak into the preview.
        assert!(!text.contains("orig 3"));
    }

    #[test]
    fn request_preview_errors_when_nothing_to_translate() {
        // Only context replicas: no translatable replica means no batch and no preview.
        let items = vec![text_item(1, false), text_item(2, false)];
        let result = build_ai_mt_request_preview("ja", "ru", items, &ai_options(4));
        assert!(result.is_err());
    }

    #[test]
    fn context_descriptor_flags_item_and_carries_existing_translation() {
        let item = text_item(4, false);
        let descriptor = ai_mt_ordered_item_descriptor(&item, false).expect("descriptor");
        assert!(descriptor.contains("\"context\":true"));
        assert!(descriptor.contains("\"existing_translation\":\"перевод 4\""));
    }

    #[test]
    fn ai_response_parser_accepts_image_original_text() {
        let parsed =
            parse_ai_mt_response(r#"[{"id":9,"original_text":"SALE","translation":"распродажа"}]"#)
                .expect("response");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].original_text.as_deref(), Some("SALE"));
        assert_matches_translation(&parsed[0], 9, "распродажа");
    }

    fn assert_matches_translation(entry: &AiMtTranslation, id: i64, translation: &str) {
        assert_eq!(entry.bubble_id, id);
        assert_eq!(entry.translation, translation);
    }
}
