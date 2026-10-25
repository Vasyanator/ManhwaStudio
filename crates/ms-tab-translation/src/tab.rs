/*
FILE OVERVIEW: crates/ms-tab-translation/src/tab.rs
Translation tab state: the shared runtime types, the constants, `TranslationTabState` with its
lifecycle and host-facing API. The tab's orchestration of the tools, OCR, machine translation,
text detector and footer metadata (synchronized with CanvasView/BubblesModel) lives in the `tab/`
child modules listed below.

Main types:
- `TranslationTool` / `ToolToggles` (child module `tab/tool_panels.rs`, imported here): the five
  tools (`Bubbles`, `Ocr`, `Composition`, `MachineTranslation`, `TextDetector`) and which of them
  are switched on — independent flags, each tool a dock tab.
- `canvas_input::CanvasSelection` (child module `tab/canvas_input.rs`): the running canvas-wide
  rect selection (quick OCR, advanced recognition, image-bubble page crop) driven from the input
  router's claim; the crop writes crop metadata instead of dispatching OCR.
- `route_inputs::OcrRouteInputsRefresh` (child module `tab/route_inputs.rs`, desktop-only): the
  cached OCR/detector routing inputs, re-read from `user_config` by a background worker.
- `neurolate_settings` / `neurolate_key` (wiring in child module `tab/neurolate.rs`): the ONE
  NeuroLate settings store (persisted under `OCR.params.neurolate`) and the ONE key + limit
  panel, shared by the detector and OCR panels; `neurolate_params` resolves the request.
- `AdvancedRecognitionWindow`: floating OCR preview/editor for manual region recognition.
- `OcrToast`: short-lived foreground notification (`text`, `severity`, `hide_at_s`); its colour
  comes from `ms_theme::Severity` (toasts and detector status never hand-type colours).
- `OcrPendingBubbleInsert`: deferred bubble-create payload after OCR response.
- `BuiltOcrRequest`: OCR request + resolved page index for a scene selection.
- `tab/block_bubbles.rs`: block + recognized text -> bubble (block OCR results and the NeuroLate
  page texts of `PageDetected::texts`), `block_uv_rect` for block OCR tasks.
- `TextDetectorOcrTask`: OCR task derived from text-detector block (`page_idx`, `uv_rect`, retry);
  `block_ocr_retry_after_failure` decides its one automatic retry (never for NeuroLate).
- `TextDetectorOcrRetryState`: delayed retry state for failed text-detector OCR task.
- `TextDetectorLineSelection` / `TextDetectorLineDragState`: runtime состояния
  режима ручного редактирования строк детектора (выбор/перетаскивание/resize).
- `TextDetectorMaskStrokeState`: runtime состояние штриха кисти в режиме
  ручного редактирования маски детектора.
- `TextDetectorMaskTextureTile` / `TextDetectorMaskTexturePage`: tiled GPU mask textures
  for visualizing text-detector alpha masks, including LRU metadata for cache eviction.
- `TextDetectionStorageEvent` / `DetectionLoad`: async load/save result for `text_detection`
  disk storage; a load lists the pages whose files could not be read, which Save then keeps
  untouched (`load_failed_detection_pages`).
- `TranslationSettingsSaveRequest`: snapshot request for async settings persistence.
- `TranslationTabState`: primary tab state (controllers, panel options, caches, pending actions).

Key constants:
- `FOOTER_PATCH_DEBOUNCE_SECS`: debounce before flushing footer patches into model.
- `TEXT_DETECTOR_MASK_TILE_SIDE`: tile size for text-detector mask textures.
- `TEXT_DETECTOR_MASK_TEXTURE_OPTIONS`: texture sampling for mask tiles.
- `TEXT_DETECTOR_MASK_VISUAL_ALPHA_MAX`: max overlay alpha used when drawing detector mask.
- `footer_additional_character_names()`: built-in extra names for footer character picker.
- `TEXT_DETECTOR_OCR_RETRY_DELAY_SECS`: retry delay for failed detector-ocr block.

TranslationTabState lifecycle:
- `new`: initializes state and starts async settings saver worker.
- `Default::default`: wraps `new(true)`.
- `Drop::drop`: sends shutdown sentinel and joins settings saver thread.

TranslationTabState public API (kept here):
- `begin_input_frame` / `end_input_frame`: the host hands the frame's input (and binding labels)
  to the state for its draw; `input_contexts`, `lines_edit_active`, `input_labels`,
  `brush_input_active` / `apply_brush_input` feed the host.
- `sync_with_project_settings`: lazy-loads OCR/MT/composition/text-detector settings.
- `notify_characters_changed`, `set_text_mask_model`, `set_session_capabilities`: host setters.
- `text_detector_gpu_memory_snapshot` / `evict_text_detector_gpu_cache`: detector mask texture
  cache for the studio's GPU eviction.

Child modules (map and contracts: `tab/MODULE_README.md`):
- Behaviour of `TranslationTabState` lives in `impl` blocks of the `tab/` children:
  `ocr_routing` (OCR states, backend health, model downloads, route gates), `characters`
  (footer character data and patches), `ocr_flow` (OCR events and selection OCR, request
  builders), `detection_storage` (detector events, `text_detection` storage), `detection_mask`
  (detector edit modes, shared text mask, mask / line overlays, detector geometry),
  `detection_ocr` (detector runs and the block OCR batch), `mt` (machine translation, AI MT
  preview), `canvas_hooks` (`CanvasHooks`, footer halves, toasts, image-bubble files),
  `settings_io` (title settings load / save and parsers), plus `tool_panels`, `dock`,
  `canvas_input`, `detector_region`, `block_bubbles`, `neurolate`, `background`,
  `scrollbar_marks`, `route_inputs` and `browser_extension`.
- This file keeps the shared types, the constants, the state struct with its lifecycle, the GPU
  cache eviction API and the host-facing input / setter API.

Key TranslationTabState field groups:
- Tools/controllers/options: `tools` (+ `tools_restored`, `pending_tool_focus`,
  `tools_persist_pending`, `current_page_size`), `local_ai` (which local AI runtimes the process
  may use; OCR/detector routes are gated per route through it, MT and AI API never are),
  `ocr_controller`, `ocr_panel_options`,
  `ocr_engine_states` + `ocr_loading_engine` (runtime per-engine OCR statuses),
  `ocr_model_downloads` (external model status/download controller, polled per frame; it
  serves the OCR panel's bundles AND the detector panel's Surya checkpoint, so one download
  runs at a time across both),
  `ocr_last_panel_engine`/`ocr_last_health_check_request_s`,
  `ai_backend_health*`,
  `mt_controller`, `mt_panel_options`, `text_detector_controller`,
  `text_detector_options`, `composition_panel_options`, `composition_panel_state`,
  `composition_presets` (app-wide presets; per-title selection).
- Detector runtime/cache: `text_detector_results`, `text_detector_mask_textures`,
  shared `text_mask_model`, `text_mask_synced_revision`,
  `text_detector_status`, `text_detector_status_severity`, `text_detector_progress`,
  `text_detector_model_download`, `text_detection_storage_*`.
- Model downloads of OCR loads / detector batches: `ocr_model_download` and
  `text_detector_model_download` (`model_download_status::ModelDownloadStatus`), drawn by the
  panels with a Cancel (`TranslationOcrController::cancel_model_download`,
  `TranslationTextDetectorController::cancel_detection`).
- Input: `input_frame` (this frame's `InputFrame`, `Some` only during the host's draw),
  `input_labels` (this frame's binding texts).
- OCR interaction runtime: `canvas_selection`, `ocr_toast`, `next_ocr_request_id`,
  `advanced_recognition`, `advanced_recognition_request`, `pending_bubble_inserts`,
  `pending_textdetector_ocr_tasks`,
  `textdetector_ocr_active_*`, `textdetector_ocr_retry_state`, `textdetector_ocr_* counters`.
- MT runtime: `pending_translate_actions`, `pending_mt_start_all`, `pending_mt_start_page`,
  `mt_progress`, `mt_stop_notice`, `mt_request_preview_rx`, `mt_request_preview`.
- Footer/characters runtime: `character_names` (assembled autocomplete base) and its two inputs
  `character_names_project` (`characters.json` roster) + `character_names_chapter` (names typed on
  this chapter's bubbles), `characters_loaded_for`,
  `characters_file_signature`, `character_names_watch_last_check_s`,
  `pending_characters_refresh`, `footer_bootstrapped`,
  `footer_tracking_synced_key`, `footer_known_ids`,
  `footer_overrides`, `pending_footer_patches`, `pending_footer_patch_changed_at`,
  `footer_character_autocomplete`,
  `last_*` defaults.
- Settings persistence: dirty flags, `*_settings_loaded_for`, `settings_save_tx`,
  `settings_save_thread`.
*/

use ms_canvas::HookBubblesKey;
use ms_input::{InputFrame, InputRegistry};
use crate::input::{ContextFacts, TranslationInputLabels};
use ms_memory::{CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo, CacheResourceKind, select_eviction_candidates};
use ms_models::SessionCapabilities;
use ms_models::text_mask_model::TextMaskModel;
use ms_project::ProjectData;
use crate::adv_rec::AdvancedRecognitionWindow;
use ms_local_ai::backend_health::{AiBackendHealthSnapshot, AiBackendProbeCommand};
use ms_local_ai::local_ai::LocalAiAvailability;
use crate::machine_translation::{MtRequestPreview, TranslationMtController};
use crate::ocr::{OcrEngine, OcrLoadState, OcrRecognizeRequest, TranslationOcrController};
use crate::model_download_status::ModelDownloadStatus;
use crate::ocr_model_download::OcrModelDownloadController;
use ms_ai_api::AiApiTaskRunner;
use crate::panels::bubbles::{BubbleFooterState, BubblesPanelState, ImageBubbleFilePick, ImageBubbleSave};
use crate::panels::composition::{CompositionPanelOptions, CompositionPanelState};
use crate::panels::composition_presets::{CompositionPresetRef, CompositionPresets};
use crate::panels::machine_translation::{MtPanelOptions, MtPanelProgress, MtStopNotice};
use crate::panels::ocr::OcrPanelOptions;
use crate::panels::text_detector::{TextDetectorPanelOptions, TextDetectorPlanNoticeCache};
use crate::neurolate::NeurolateSettings;
use ms_ai_api::neurolate::NeurolateKeyPanel;
use crate::text_detector::{RegionDetection, TextDetectorPageResult, TextDetectorRect, TranslationTextDetectorController};
use ms_tools::MaskBrush;
use ms_widgets::AutocompleteLine;
use eframe::egui;
use egui::{Pos2, Rect};
use ms_theme::Severity;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use ms_thread::JoinHandle;

// The canvas pointer drivers (rect selections, lines / mask edit), fed by the input frame.
mod canvas_input;
// An OCR selection's region detection (trigger, merge into both detector stores) and the
// detector Save snapshot of the shown masks.
mod detector_region;
// The tab's NeuroLate key panel step and the resolved NeuroLate request parameters.
mod neurolate;
// Detector block + recognized text -> bubble, shared by block OCR and NeuroLate page texts.
mod block_bubbles;
// Off-GUI-thread refresh of the OCR / detector routing inputs (desktop-only).
#[cfg(not(target_arch = "wasm32"))]
mod route_inputs;
// The panel dock: default arrangement, the per-frame dock run, «Последние персонажи».
mod dock;
// The five tools: their vocabulary, the canvas tools bar, the tool window and one body per tool.
mod tool_panels;
// Translation-status marks on the canvas scrollbar and their cross-frame cache.
mod scrollbar_marks;
// Off-draw drain of OCR / MT / detector results while the tab is hidden.
mod background;
pub(crate) use background::tab_hidden_last_frame;
// The MT panel's browser-extension block: its state and the bridge start / install workers
// (`pub(crate)`: the MT panel draws its view types).
pub(crate) mod browser_extension;
// OCR engine states, backend health, model downloads and the OCR / detector route gates.
mod ocr_routing;
// Footer character names, footer tracking / patches, recent characters, image-bubble pick.
mod characters;
// OCR events, image-crop / advanced / manual OCR on a selection, OCR request builders.
mod ocr_flow;
// Detector events and the `text_detection` disk storage (load / save jobs, file IO).
mod detection_storage;
// Detector status / edit modes, the shared text-mask sync, mask painting and drawing.
mod detection_mask;
// Detected-block queries, detector runs and the sequential detector-block OCR batch.
mod detection_ocr;
// Machine translation and AI API events, MT scopes and starts, the AI MT request preview.
mod mt;
// `CanvasHooks` for the tab, image / hint footer controls, toasts, image-bubble files.
mod canvas_hooks;
// Title settings hydration, the coalescing settings saver and the settings parsers.
mod settings_io;

use canvas_input::CanvasSelection;
pub use dock::translation_default_dock_layout;
use tool_panels::{ToolToggles, TranslationTool};
use detection_mask::text_detector_mask_texture_page_estimated_bytes;
use settings_io::spawn_translation_settings_saver_thread;

/// What the stored-detection load read from a chapter's `text_detection` directory.
#[derive(Debug, Default)]
struct DetectionLoad {
    /// The pages read, each with its result.
    pages: Vec<(usize, TextDetectorPageResult)>,
    /// Pages whose stored files exist but could not be read (each logged with its cause). Save
    /// never deletes nor rewrites them (`load_failed_detection_pages`).
    failed_pages: Vec<usize>,
}

const FOOTER_PATCH_DEBOUNCE_SECS: f64 = 0.25;
/// Bubble `extra` key holding a hint bubble's "show outside the translation tab" flag.
/// Wire key, deliberately not localized (`dev-docs/i18n_exclusions.md`).
const HINT_SHOW_OUTSIDE_EXTRA_KEY: &str = "hint_show_outside_translation";
const TEXT_DETECTOR_MASK_TILE_SIDE: usize = 1024;
const TEXT_DETECTOR_MASK_TEXTURE_OPTIONS: egui::TextureOptions = egui::TextureOptions::NEAREST;
const TEXT_DETECTOR_MASK_VISUAL_ALPHA_MAX: u8 = 96;
/// Built-in extra character-name suggestions for the bubble footer picker.
///
/// Runtime (not `const`) because `t!` is not const. These are display-only
/// autocomplete suggestions; the value a user picks is stored verbatim, so
/// localizing the suggestions does not rewrite any saved bubble data.
fn footer_additional_character_names() -> [&'static str; 13] {
    [
        t!("translation.tab.footer_character.caption"),
        t!("translation.tab.footer_character.sound"),
        t!("translation.tab.footer_character.mc"),
        t!("translation.tab.footer_character.mc_thoughts"),
        t!("translation.tab.footer_character.unclear"),
        t!("translation.tab.footer_character.someone"),
        t!("translation.tab.footer_character.someone_of_them"),
        t!("translation.tab.footer_character.some_girl"),
        t!("translation.tab.footer_character.some_boy"),
        t!("translation.tab.footer_character.some_guy"),
        t!("translation.tab.footer_character.some_young_woman"),
        t!("translation.tab.footer_character.some_woman"),
        t!("translation.tab.footer_character.some_man"),
    ]
}
const TEXT_DETECTOR_OCR_RETRY_DELAY_SECS: f64 = 3.0;
const OCR_HEALTH_CHECK_THROTTLE_SECS: f64 = 0.5;
/// Minimum interval between starts of the background read of the native-runtime
/// routing inputs (selected `AiRuntime` + SIGILL load guard) that back the
/// route-aware OCR readiness gate (`route_inputs::OcrRouteInputsRefresh`). The gate
/// runs every frame against the cached snapshot; the read itself never runs on the
/// GUI thread. Desktop-only.
#[cfg(not(target_arch = "wasm32"))]
const OCR_ROUTE_INPUTS_CACHE_TTL: web_time::Duration = web_time::Duration::from_millis(500);
const FOOTER_CHARACTER_AUTOCOMPLETE_MAX: usize = 7;
const CHARACTER_NAMES_WATCH_CHECK_SECS: f64 = 1.0;
const RECENT_CHARACTER_HISTORY_LIMIT: usize = 6;

#[derive(Debug, Clone)]
struct OcrToast {
    text: String,
    /// Meaning of the message; the frame stroke and text colour are `severity.color()`.
    severity: Severity,
    hide_at_s: f64,
}

/// Debug window state for "Отобразить полный запрос": holds the assembled first AI request and the
/// GPU textures lazily created for its inline images. `scope_label` describes which action built it
/// ("текущая страница" / "весь проект"). `open` is driven by the egui window close button.
struct MtRequestPreviewWindow {
    preview: MtRequestPreview,
    scope_label: String,
    open: bool,
    /// One lazily-loaded texture per `MtRequestPreviewPart::Image`, indexed by image order.
    image_textures: Vec<Option<egui::TextureHandle>>,
}

#[derive(Debug, Clone, Copy)]
struct OcrPendingBubbleInsert {
    page_idx: usize,
    uv_rect: [f32; 4],
    join_newlines: bool,
    recent_character_rank: Option<usize>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ManualOcrResultTarget {
    AdvancedWindowText,
    ToastAndClipboard,
}

#[derive(Debug, Clone)]
struct BuiltOcrRequest {
    request: OcrRecognizeRequest,
    page_idx: usize,
}

#[derive(Debug, Clone, Copy)]
struct TextDetectorOcrTask {
    page_idx: usize,
    uv_rect: [f32; 4],
    retry_attempt: u8,
}

/// The retry of a block-OCR task whose request failed: the same selection once more, only for a
/// first attempt sent with an engine that may repeat a request automatically (never NeuroLate:
/// a paid request is not sent twice without the user; `None` engine = unknown, no retry).
fn block_ocr_retry_after_failure(task: TextDetectorOcrTask, engine: Option<OcrEngine>) -> Option<TextDetectorOcrTask> {
    (task.retry_attempt == 0 && engine.is_some_and(OcrEngine::retries_failed_block_request)).then_some(TextDetectorOcrTask { retry_attempt: 1, ..task })
}

#[derive(Debug, Clone, Copy)]
struct TextDetectorOcrRetryState {
    task: TextDetectorOcrTask,
    retry_at_s: f64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct TextDetectorLineSelection {
    page_idx: usize,
    line_idx: usize,
}

#[derive(Debug, Clone, Copy)]
enum TextDetectorLineDragKind {
    Move,
    Resize { handle_idx: usize },
}

#[derive(Debug, Clone, Copy)]
struct TextDetectorLineDragState {
    selection: TextDetectorLineSelection,
    start_pointer_src: Pos2,
    start_rect: TextDetectorRect,
    kind: TextDetectorLineDragKind,
}

/// Where one text-mask brush edit lands: the page, its source-image and mask sizes in pixels
/// (`text_mask_target_sizes`) and the page's on-screen rect that scene positions map through.
#[derive(Debug, Clone, Copy)]
struct TextMaskPageGeom {
    page_idx: usize,
    source_size: [u32; 2],
    mask_size: [u32; 2],
    page_rect: Rect,
}

#[derive(Debug, Clone, Copy)]
struct TextDetectorMaskStrokeState {
    page_idx: usize,
    erase: bool,
    last_scene_pos: Pos2,
}

#[derive(Clone)]
struct TextDetectorMaskTextureTile {
    texture: egui::TextureHandle,
    origin_px: [usize; 2],
    size_px: [usize; 2],
}

#[derive(Clone)]
struct TextDetectorMaskTexturePage {
    size: [usize; 2],
    tiles: Vec<TextDetectorMaskTextureTile>,
    last_used_frame: u64,
}

#[derive(Debug)]
enum TextDetectionStorageEvent {
    Loaded {
        project_dir: PathBuf,
        load: DetectionLoad,
    },
    Saved {
        project_dir: PathBuf,
        saved: usize,
        failed: usize,
        /// Pages left alone because their stored files could not be read at load.
        kept_unreadable: usize,
    },
    Failed {
        project_dir: PathBuf,
        error: String,
    },
}

#[derive(Debug, Clone)]
struct TranslationSettingsSaveRequest {
    settings_file: PathBuf,
    ocr_options: OcrPanelOptions,
    mt_options: MtPanelOptions,
    composition_options: CompositionPanelOptions,
    /// The selected composition preset, saved as `composition.preset` (`None` removes it).
    composition_preset: Option<CompositionPresetRef>,
    text_detector_options: TextDetectorPanelOptions,
    neurolate: NeurolateSettings,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct RecentCharacterEntry {
    is_known_character: bool,
    character_name: String,
    clarification: String,
}

pub struct TranslationTabState {
    /// Which of the five tools are switched on (`tab/tool_panels.rs`): the ONE source of truth
    /// for whether a tool is open. Read during the canvas draw (detector overlay), so it is never
    /// derived from the dock's output.
    tools: ToolToggles,
    /// Whether `tools` was adopted from the dock's persisted toolbar extras (once per state,
    /// `restore_tool_toggles`).
    tools_restored: bool,
    /// Tools switched on since the last dock frame; their tabs are focused before the next
    /// `PanelDock::begin` (`apply_tool_dock_requests`).
    pending_tool_focus: Vec<TranslationTool>,
    /// `tools` changed since it was last written into the toolbar's dock extras.
    tools_persist_pending: bool,
    /// Size of the current page, cached by the host before the canvas draw for the detector
    /// tool body (the overlay hook receives no `page_infos`); `None` while unknown.
    current_page_size: Option<[u32; 2]>,
    /// Source size of every page whose `PageImageInfo` is known (decoded, non-zero), refreshed by
    /// the host each frame (`note_page_sizes`): the GUI thread's page size source, so no hook
    /// reads an image header from disk.
    known_page_sizes: HashMap<usize, [u32; 2]>,
    /// Which local AI runtimes (Python backend, native ONNX) this process may use (`--no-ai`
    /// and platform). Only routes needing an off runtime are refused (`ocr_route_availability`,
    /// `TextDetectorAlgorithm::availability`); cloud AI API and machine translation never are.
    local_ai: LocalAiAvailability,
    ai_backend_health: Arc<Mutex<AiBackendHealthSnapshot>>,
    ai_backend_health_cached: AiBackendHealthSnapshot,
    ai_backend_probe_tx: Option<Sender<AiBackendProbeCommand>>,
    ocr_controller: TranslationOcrController,
    ocr_panel_options: OcrPanelOptions,
    /// Key / metadata requests of the OCR panel's AI API connection widget
    /// (`ocr_panel_options.ai_api`), polled by `poll_ai_api_events`.
    ocr_ai_api_tasks: AiApiTaskRunner,
    ocr_engine_states: [OcrLoadState; 8],
    /// Install status and explicit downloads of the external OCR models (Baberu OCR,
    /// PaddleOCR-VL variants); polled every frame by `poll_ocr_model_downloads`.
    ocr_model_downloads: OcrModelDownloadController,
    ocr_loading_engine: Option<OcrEngine>,
    /// Latest app-managed model download of the OCR command in flight (shown under the OCR
    /// panel's status for `ocr_loading_engine`); cleared on every state change that ends it.
    ocr_model_download: ModelDownloadStatus,
    ocr_last_panel_engine: Option<OcrEngine>,
    ocr_last_health_check_request_s: f64,
    /// Cached native-runtime routing inputs (selected `AiRuntime` + provider-scoped
    /// SIGILL load guard) that drive the route-aware "does OCR need the Python
    /// backend?" gate. Re-read by a background worker at most every
    /// `OCR_ROUTE_INPUTS_CACHE_TTL` (`refresh_ocr_route_inputs_cache`), so the GUI
    /// thread never touches the disk for it. Desktop-only: the native runtime is
    /// compiled out on web.
    #[cfg(not(target_arch = "wasm32"))]
    ocr_route_inputs: route_inputs::OcrRouteInputsRefresh,
    mt_controller: TranslationMtController,
    mt_panel_options: MtPanelOptions,
    /// Key / metadata requests of the MT panel's AI API connection widget
    /// (`mt_panel_options.ai_api`), polled by `poll_ai_api_events`.
    mt_ai_api_tasks: AiApiTaskRunner,
    /// The MT panel's browser-extension block (bridge start, folder probe, install job, the
    /// transport save), polled every frame by `poll_browser_extension`.
    browser_extension: browser_extension::BrowserExtensionController,
    text_detector_controller: TranslationTextDetectorController,
    text_detector_options: TextDetectorPanelOptions,
    text_detector_plan_notice: TextDetectorPlanNoticeCache,
    /// The ONE store of the NeuroLate language / comic type / "create mask" choices, read by the
    /// detector panel, the OCR panel and every NeuroLate request (persisted with `OCR`).
    neurolate_settings: NeurolateSettings,
    /// The ONE NeuroLate key + limit panel of the tab, shared by the detector and OCR panels.
    neurolate_key: NeurolateKeyPanel,
    text_detector_results: HashMap<usize, TextDetectorPageResult>,
    text_detector_mask_textures: HashMap<usize, TextDetectorMaskTexturePage>,
    text_mask_model: Option<Arc<Mutex<TextMaskModel>>>,
    text_mask_synced_revision: u64,
    text_detector_status: String,
    text_detector_status_severity: Severity,
    text_detector_progress: Option<(usize, usize)>,
    /// Latest model download of the running detector batch; cleared by every non-download
    /// event of the batch (pages, end).
    text_detector_model_download: ModelDownloadStatus,
    text_detector_edit_lines_mode: bool,
    text_detector_edit_mask_mode: bool,
    /// Outer rect the dock panel holding the «Лента» tab was drawn at on the LAST
    /// frame, in main-window coordinates; `None` when the tab was not on screen
    /// (hidden, or the user moved it into a detached sub-window, whose rects say
    /// nothing about this window).
    ///
    /// The two text-detector edit boxes hang under that panel — as they hung under
    /// the canvas controls `Area` before it became a dock tab — and they are drawn
    /// EARLIER in `draw_canvas_overlay_top_left` than the dock runs, so they can
    /// only ever see the previous frame's rect. That is exactly what the old
    /// `Memory::area_rect` read gave them.
    ribbon_panel_rect: Option<Rect>,
    text_detector_line_selection: Option<TextDetectorLineSelection>,
    text_detector_line_drag_state: Option<TextDetectorLineDragState>,
    /// Pages whose detected lines were edited (moved, resized, added, deleted) and not yet handed
    /// to the shared `TextMaskModel` (`canvas_input::flush_line_edits_to_text_mask_model`).
    line_edited_pages: BTreeSet<usize>,
    /// Where (page-normalized) the lines edit page menu was opened: its "create line" item adds
    /// the line there.
    text_detector_line_menu_uv: Option<Pos2>,
    text_detector_mask_stroke_state: Option<TextDetectorMaskStrokeState>,
    text_detector_mask_brush: MaskBrush,
    text_detection_storage_loaded_for: Option<PathBuf>,
    /// Pages of the loaded chapter whose stored detection files exist but could not be read
    /// (set by each load). Save neither deletes nor rewrites their files and says so; a full
    /// page detection of such a page (`PageDetected`) replaces the whole page and lifts it.
    load_failed_detection_pages: HashSet<usize>,
    text_detection_storage_busy: bool,
    text_detection_storage_rx: Option<Receiver<TextDetectionStorageEvent>>,
    /// Region detections that arrived while the chapter's stored detection was loading (or
    /// being saved); applied in arrival order once that job ends
    /// (`detector_region::replay_pending_region_detections`), so a region never replaces a
    /// stored page the load has not delivered yet.
    pending_region_detections: Vec<RegionDetection>,
    /// Quick-selection OCR requests whose answer also carries the selection's region (NeuroLate
    /// `MASK_AND_TEXT`, `detector_region::receive_ocr_region`); dropped by Clear and a replacing
    /// page batch like `pending_region_detections`.
    pending_ocr_regions: HashSet<u64>,
    /// The running canvas-wide rect selection (quick / advanced OCR, image crop), driven from the
    /// router's claim (`canvas_input::advance_selection`).
    canvas_selection: Option<CanvasSelection>,
    advanced_recognition: AdvancedRecognitionWindow,
    advanced_recognition_request: Option<BuiltOcrRequest>,
    ocr_toast: Option<OcrToast>,
    next_ocr_request_id: u64,
    manual_ocr_active_request_id: Option<u64>,
    manual_ocr_result_target: Option<ManualOcrResultTarget>,
    pending_bubble_inserts: HashMap<u64, OcrPendingBubbleInsert>,
    pending_textdetector_ocr_tasks: VecDeque<TextDetectorOcrTask>,
    textdetector_ocr_active_request_id: Option<u64>,
    textdetector_ocr_active_task: Option<TextDetectorOcrTask>,
    /// The engine the active block-OCR request was sent with (its automatic retry policy,
    /// `OcrEngine::retries_failed_block_request`); set and cleared with the active task.
    textdetector_ocr_active_engine: Option<OcrEngine>,
    textdetector_ocr_retry_state: Option<TextDetectorOcrRetryState>,
    textdetector_ocr_total: usize,
    textdetector_ocr_done: usize,
    textdetector_ocr_recognized: usize,
    pending_translate_actions: Vec<i64>,
    pending_mt_start_all: bool,
    pending_mt_start_page: bool,
    mt_progress: Option<MtPanelProgress>,
    /// Sticky notice shown when an AI run stopped due to a probable credit/quota/limit error.
    mt_stop_notice: Option<MtStopNotice>,
    /// Pending background build of the AI request preview (debug "Отобразить полный запрос").
    /// Payload is `(preview, scope_label)` on success.
    mt_request_preview_rx: Option<Receiver<Result<(MtRequestPreview, String), String>>>,
    /// Open debug window showing the first AI request that a translate action would send.
    mt_request_preview: Option<MtRequestPreviewWindow>,
    ocr_settings_dirty: bool,
    mt_settings_dirty: bool,
    composition_settings_dirty: bool,
    text_detector_settings_dirty: bool,
    ocr_settings_loaded_for: Option<PathBuf>,
    mt_settings_loaded_for: Option<PathBuf>,
    composition_settings_loaded_for: Option<PathBuf>,
    text_detector_settings_loaded_for: Option<PathBuf>,
    settings_save_tx: Sender<TranslationSettingsSaveRequest>,
    settings_save_thread: Option<JoinHandle<()>>,
    /// Assembled character list, in suggestion priority order (see
    /// `build_translation_character_names`). Rebuilt only when one of its two inputs below
    /// changes. Two consumers with different needs read it as a SLICE, never a per-frame clone:
    /// the bubbles panel takes it whole (its combo box needs the leading `footer_no_character()`
    /// sentinel and uses `[0]` as a fallback value), the footer takes
    /// `footer_autocomplete_candidates(..)` — the same slice minus that sentinel.
    character_names: Vec<String>,
    /// Character roster loaded from the project's `characters.json`, in file order.
    /// Refreshed by `reload_character_names` (project switch, mtime watch, explicit notify).
    character_names_project: Vec<String>,
    /// Free-form character names actually typed on this chapter's bubbles, ordered by bubble id.
    /// Recomputed inside the bubble-revision gate of `sync_footer_tracking`.
    character_names_chapter: Vec<String>,
    characters_loaded_for: Option<PathBuf>,
    /// Docstore signature of `characters.json` at the last roster load; the watch compares it.
    characters_file_signature: Option<ms_docstore::Signature>,
    character_names_watch_last_check_s: f64,
    pending_characters_refresh: bool,
    footer_bootstrapped: bool,
    // Last `CanvasView::hook_bubbles_key` for which `sync_footer_tracking` ran a full recompute.
    // Used to skip the per-frame snapshot/clone/sort when the bubble set is unchanged.
    footer_tracking_synced_key: Option<HookBubblesKey>,
    /// Scrollbar status entries, rebuilt only when the bubble set or the page layout changed.
    scrollbar_status_cache: scrollbar_marks::ScrollbarStatusCache,
    footer_known_ids: HashSet<i64>,
    footer_overrides: HashMap<i64, BubbleFooterState>,
    /// The one open «choose file» dialog of an external image bubble, started by the canvas
    /// footer or a bubbles-panel card and resolved by [`Self::poll_image_bubble_pick`].
    image_bubble_pick: Option<ImageBubbleFilePick>,
    /// The one running image-bubble file job (clipboard paste or picked-file copy), started
    /// from the canvas footer, a bubbles-panel card or a finished pick, and resolved by
    /// [`Self::poll_image_bubble_save`].
    image_bubble_save: Option<ImageBubbleSave>,
    pending_footer_patches: HashMap<i64, Map<String, Value>>,
    pending_footer_patch_changed_at: HashMap<i64, f64>,
    footer_character_autocomplete: HashMap<i64, AutocompleteLine>,
    bubbles_panel: BubblesPanelState,
    composition_panel_options: CompositionPanelOptions,
    composition_panel_state: CompositionPanelState,
    /// App-wide composition presets (seeded by `seed_composition_presets`); the selection is
    /// per title, saved with the `composition` section.
    composition_presets: CompositionPresets,
    composition_rebuild_requested: bool,
    /// This frame's binding texts (`TranslationInputLabels::from_registry`), set by the host.
    input_labels: TranslationInputLabels,
    /// This frame's input, set by the host for its draw (`begin_input_frame`) and cleared after
    /// it (`end_input_frame`); the canvas hooks and panels read it. `None` outside the draw.
    input_frame: Option<InputFrame>,
    last_is_known_character: bool,
    last_character_name: String,
    last_clarification: String,
    recent_characters: VecDeque<RecentCharacterEntry>,
    last_page_idx: i64,
    last_bubble_order: i32,
    /// What the open session offers (`set_session_capabilities`, pushed once by the root):
    /// `title_documents` gates the MT project context and the character roster,
    /// `chapter_storage` the text-detection storage.
    session_caps: SessionCapabilities,
}

impl Default for TranslationTabState {
    fn default() -> Self {
        Self::new(
            // No `--no-ai`: the platform alone decides.
            LocalAiAvailability::for_process(false),
            Arc::new(Mutex::new(AiBackendHealthSnapshot::default())),
            None,
        )
    }
}

impl TranslationTabState {
    /// Builds the tab. `local_ai` is the process's local-AI availability (carried by the
    /// backend supervisor handle); `ai_backend_health` / `ai_backend_probe_tx` are the shared
    /// health snapshot and the probe channel (`None` when no probe runs).
    pub fn new(
        local_ai: LocalAiAvailability,
        ai_backend_health: Arc<Mutex<AiBackendHealthSnapshot>>,
        ai_backend_probe_tx: Option<Sender<AiBackendProbeCommand>>,
    ) -> Self {
        let (settings_save_tx, settings_save_thread) = spawn_translation_settings_saver_thread();
        Self {
            tools: ToolToggles::default(),
            tools_restored: false,
            pending_tool_focus: Vec::new(),
            tools_persist_pending: false,
            current_page_size: None,
            known_page_sizes: HashMap::new(),
            local_ai,
            ai_backend_health: Arc::clone(&ai_backend_health),
            ai_backend_health_cached: match ai_backend_health.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            },
            ai_backend_probe_tx,
            ocr_controller: TranslationOcrController::default(),
            ocr_panel_options: OcrPanelOptions::default(),
            ocr_ai_api_tasks: AiApiTaskRunner::default(),
            ocr_engine_states: [OcrLoadState::NotLoaded; 8],
            ocr_model_downloads: OcrModelDownloadController::new(),
            ocr_loading_engine: None,
            ocr_model_download: ModelDownloadStatus::default(),
            ocr_last_panel_engine: None,
            ocr_last_health_check_request_s: -10_000.0,
            // Unknown until the first background read lands (started on the first
            // refresh); the gates are evaluated optimistically meanwhile
            // (`route_inputs::gate_over_inputs`) and the native-ORT capability slot stays
            // as primed (unknown).
            #[cfg(not(target_arch = "wasm32"))]
            ocr_route_inputs: route_inputs::OcrRouteInputsRefresh::new(),
            mt_controller: TranslationMtController::default(),
            mt_panel_options: MtPanelOptions::default(),
            mt_ai_api_tasks: AiApiTaskRunner::default(),
            browser_extension: browser_extension::BrowserExtensionController::default(),
            text_detector_controller: TranslationTextDetectorController::default(),
            text_detector_options: TextDetectorPanelOptions::default(),
            text_detector_plan_notice: TextDetectorPlanNoticeCache::default(),
            neurolate_settings: NeurolateSettings::default(),
            neurolate_key: NeurolateKeyPanel::new(),
            text_detector_results: HashMap::new(),
            text_detector_mask_textures: HashMap::new(),
            text_mask_model: None,
            text_mask_synced_revision: 0,
            text_detector_status: t!("translation.tab.ready_status").to_string(),
            text_detector_status_severity: Severity::Success,
            text_detector_progress: None,
            text_detector_model_download: ModelDownloadStatus::default(),
            text_detector_edit_lines_mode: false,
            text_detector_edit_mask_mode: false,
            ribbon_panel_rect: None,
            text_detector_line_selection: None,
            text_detector_line_drag_state: None,
            line_edited_pages: BTreeSet::new(),
            text_detector_line_menu_uv: None,
            text_detector_mask_stroke_state: None,
            text_detector_mask_brush: MaskBrush::default(),
            text_detection_storage_loaded_for: None,
            load_failed_detection_pages: HashSet::new(),
            text_detection_storage_busy: false,
            pending_region_detections: Vec::new(),
            pending_ocr_regions: HashSet::new(),
            text_detection_storage_rx: None,
            canvas_selection: None,
            advanced_recognition: AdvancedRecognitionWindow::default(),
            advanced_recognition_request: None,
            ocr_toast: None,
            next_ocr_request_id: 1,
            manual_ocr_active_request_id: None,
            manual_ocr_result_target: None,
            pending_bubble_inserts: HashMap::new(),
            pending_textdetector_ocr_tasks: VecDeque::new(),
            textdetector_ocr_active_request_id: None,
            textdetector_ocr_active_task: None,
            textdetector_ocr_active_engine: None,
            textdetector_ocr_retry_state: None,
            textdetector_ocr_total: 0,
            textdetector_ocr_done: 0,
            textdetector_ocr_recognized: 0,
            pending_translate_actions: Vec::new(),
            pending_mt_start_all: false,
            pending_mt_start_page: false,
            mt_progress: None,
            mt_stop_notice: None,
            mt_request_preview_rx: None,
            mt_request_preview: None,
            ocr_settings_dirty: false,
            mt_settings_dirty: false,
            composition_settings_dirty: false,
            text_detector_settings_dirty: false,
            ocr_settings_loaded_for: None,
            mt_settings_loaded_for: None,
            composition_settings_loaded_for: None,
            text_detector_settings_loaded_for: None,
            settings_save_tx,
            settings_save_thread: Some(settings_save_thread),
            character_names: Vec::new(),
            character_names_project: Vec::new(),
            character_names_chapter: Vec::new(),
            characters_loaded_for: None,
            characters_file_signature: None,
            character_names_watch_last_check_s: -10_000.0,
            pending_characters_refresh: false,
            footer_bootstrapped: false,
            footer_tracking_synced_key: None,
            scrollbar_status_cache: scrollbar_marks::ScrollbarStatusCache::default(),
            footer_known_ids: HashSet::new(),
            footer_overrides: HashMap::new(),
            image_bubble_pick: None,
            image_bubble_save: None,
            pending_footer_patches: HashMap::new(),
            pending_footer_patch_changed_at: HashMap::new(),
            footer_character_autocomplete: HashMap::new(),
            bubbles_panel: BubblesPanelState::default(),
            composition_panel_options: CompositionPanelOptions::default(),
            composition_panel_state: CompositionPanelState::default(),
            composition_presets: CompositionPresets::default(),
            composition_rebuild_requested: true,
            input_labels: TranslationInputLabels::default(),
            input_frame: None,
            last_is_known_character: true,
            last_character_name: String::new(),
            last_clarification: String::new(),
            recent_characters: VecDeque::new(),
            last_page_idx: -1,
            last_bubble_order: -1,
            session_caps: SessionCapabilities::default(),
        }
    }

    pub fn text_detector_gpu_memory_snapshot(
        &self,
        pinned_pages: &BTreeSet<usize>,
    ) -> Vec<CacheResourceInfo> {
        self.text_detector_mask_textures
            .iter()
            .map(|(page_idx, page_tex)| CacheResourceInfo {
                id: format!("translation-detector-mask-gpu:{page_idx}"),
                kind: CacheResourceKind::DetectorMaskGpu,
                page_idx: Some(*page_idx),
                estimated_bytes: text_detector_mask_texture_page_estimated_bytes(page_tex),
                last_used_frame: page_tex.last_used_frame,
                reload_cost: CacheReloadCost::RebuildFromModel,
                dirty: false,
                visible: pinned_pages.contains(page_idx),
                reconstructable: self.text_detector_results.contains_key(page_idx),
            })
            .collect()
    }

    pub fn evict_text_detector_gpu_cache(
        &mut self,
        request: &CacheEvictionRequest,
    ) -> CacheEvictionReport {
        let snapshot = self.text_detector_gpu_memory_snapshot(&request.pinned_pages);
        let candidates = select_eviction_candidates(&snapshot, request);
        let mut evicted = Vec::new();
        let mut freed = 0_u64;
        for resource in candidates.resources {
            let Some(page_idx) = resource.page_idx else {
                continue;
            };
            if self.text_detector_mask_textures.remove(&page_idx).is_some() {
                freed = freed.saturating_add(resource.estimated_bytes);
                evicted.push(resource);
            }
        }
        CacheEvictionReport {
            resources: evicted,
            estimated_freed_bytes: freed,
        }
    }
}

impl TranslationTabState {
    /// Seeds the app-wide composition presets from the pre-read `user_config` root (no file
    /// I/O); later preset operations persist to `preset_store` off the GUI thread (`None`: kept in
    /// memory, the form every test construction uses). Called once by the host when the tab is built.
    pub(crate) fn seed_composition_presets(&mut self, user_settings: &Value, preset_store: Option<PathBuf>) {
        self.composition_presets = CompositionPresets::from_user_settings(user_settings, preset_store);
    }
}

impl Drop for TranslationTabState {
    fn drop(&mut self) {
        let _ = self.settings_save_tx.send(TranslationSettingsSaveRequest {
            settings_file: PathBuf::new(),
            ocr_options: OcrPanelOptions::default(),
            mt_options: MtPanelOptions::default(),
            composition_options: CompositionPanelOptions::default(),
            composition_preset: None,
            text_detector_options: TextDetectorPanelOptions::default(),
            neurolate: NeurolateSettings::default(),
        });
        if let Some(handle) = self.settings_save_thread.take() {
            let _ = handle.join();
        }
    }
}

impl TranslationTabState {
    /// The contexts this tab publishes this frame (`crate::input::contexts`): the detector edit
    /// mode, the advanced-recognition window and its brush, and `BRUSH_ACTIVE` while a brush of
    /// this tab is on. The host adds its canvas's contexts.
    #[must_use]
    pub fn input_contexts(&self) -> Vec<ms_input::ContextId> {
        crate::input::contexts(ContextFacts {
            lines_edit: self.text_detector_edit_lines_mode,
            mask_edit: self.text_detector_edit_mask_mode,
            adv_rec_open: self.advanced_recognition.is_open(),
            adv_rec_brush: self.advanced_recognition.brush_input_active(),
        })
    }

    /// Whether the text-detector lines edit mode is on (the host adds its hint rows).
    #[must_use]
    pub fn lines_edit_active(&self) -> bool {
        self.text_detector_edit_lines_mode
    }

    /// Takes this frame's input for the draw: a copy of `input` for the canvas hooks and panels,
    /// and the binding texts of the hints from `registry`. Paired with [`Self::end_input_frame`].
    pub fn begin_input_frame(&mut self, input: &InputFrame, registry: &InputRegistry) {
        self.input_frame = Some(input.clone());
        self.input_labels = TranslationInputLabels::from_registry(registry);
    }

    /// Drops this frame's input copy after the draw (nothing reads a stale frame next frame).
    pub fn end_input_frame(&mut self) {
        self.input_frame = None;
    }

    /// This frame's binding texts.
    #[must_use]
    pub fn input_labels(&self) -> &TranslationInputLabels {
        &self.input_labels
    }

    /// Whether mode `id` is active in this frame's input (`false` outside the draw).
    fn input_mode_active(&self, id: ms_input::ActionId) -> bool {
        self.input_frame.as_ref().is_some_and(|frame| frame.mode_active(id))
    }

    /// Whether a brush of this tab is on — the detector mask edit or the advanced-recognition
    /// brush — so the host publishes `ms_tools::mask_brush::BRUSH_ACTIVE` and the shared brush
    /// actions (`brush.size` / `smaller` / `larger`) reach [`Self::apply_brush_input`].
    #[must_use]
    pub fn brush_input_active(&self) -> bool {
        self.text_detector_edit_mask_mode || self.advanced_recognition.brush_input_active()
    }

    /// Applies this frame's shared brush actions to the brushes that are on (the detector mask
    /// brush in mask edit, the advanced-recognition brush). Called once per frame by the host
    /// before drawing. Returns whether a radius changed.
    pub fn apply_brush_input(&mut self, input: &InputFrame) -> bool {
        let mut changed = false;
        if self.text_detector_edit_mask_mode {
            changed |= self.text_detector_mask_brush.apply_input(input);
        }
        changed |= self.advanced_recognition.apply_brush_input(input);
        changed
    }

    /// Force an immediate refresh of the character names cache on the next frame,
    /// bypassing the periodic mtime-watch interval.
    pub fn notify_characters_changed(&mut self) {
        self.pending_characters_refresh = true;
        // Reset the watch timer so the next mtime-check does not conflict.
        self.character_names_watch_last_check_s = f64::NEG_INFINITY;
    }

    pub fn set_text_mask_model(&mut self, model: Arc<Mutex<TextMaskModel>>) {
        self.text_mask_model = Some(model);
        self.text_mask_synced_revision = 0;
        self.text_detector_mask_textures.clear();
    }

    pub fn sync_with_project_settings(&mut self, project: &ProjectData) {
        self.ensure_ocr_settings_loaded(project);
        self.ensure_mt_settings_loaded(project);
        self.ensure_composition_settings_loaded(project);
        self.ensure_text_detector_settings_loaded(project);
    }

    /// Sets what the open session offers (`SessionMode::capabilities` in the root). The root pushes it
    /// once, right after construction and before the first frame; the session kind never changes
    /// during the tab's lifetime.
    pub fn set_session_capabilities(&mut self, caps: SessionCapabilities) {
        self.session_caps = caps;
    }
}

struct TextDetectorMaskOverlayDrawParams<'a> {
    textures: &'a mut HashMap<usize, TextDetectorMaskTexturePage>,
    ctx: &'a egui::Context,
    painter: &'a egui::Painter,
    page_idx: usize,
    page_rect: Rect,
    mask_size: [u32; 2],
    mask_alpha: &'a [u8],
    current_frame: u64,
}

#[cfg(test)]
mod tests;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod detection_storage_tests;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod settings_persistence_tests;

#[cfg(test)]
mod ocr_model_settings_tests;

#[cfg(test)]
mod ocr_model_download_wiring_tests;
