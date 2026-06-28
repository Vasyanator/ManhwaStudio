/*
FILE OVERVIEW: src/tabs/translation/tab.rs
Translation tab state and orchestration for side panels, OCR, machine translation,
text detector, and footer metadata synchronization with CanvasView/BubblesModel.

Main types:
- `TranslationPanel`: active floating panel (`None`, `Bubbles`, `Ocr`, `Composition`,
  `MachineTranslation`, `TextDetector`).
- `OcrDragSelection`: transient OCR region selection on canvas (`start`, `current`).
- `ImageCropDragSelection`: transient ImageBubble crop selection (`Shift+Q+drag`) that writes
  crop metadata instead of dispatching OCR.
- `AdvancedRecognitionWindow`: floating OCR preview/editor for manual region recognition.
- `OcrToast`: short-lived foreground notification (`text`, `color`, `hide_at_s`).
- `OcrPendingBubbleInsert`: deferred bubble-create payload after OCR response.
- `BuiltOcrRequest`: OCR request + resolved page index for a scene selection.
- `TextDetectorOcrTask`: OCR task derived from text-detector block (`page_idx`, `uv_rect`, retry).
- `TextDetectorOcrRetryState`: delayed retry state for failed text-detector OCR task.
- `TextDetectorLineSelection` / `TextDetectorLineDragState`: runtime состояния
  режима ручного редактирования строк детектора (выбор/перетаскивание/resize).
- `TextDetectorMaskStrokeState`: runtime состояние штриха кисти в режиме
  ручного редактирования маски детектора.
- `TextDetectorMaskTextureTile` / `TextDetectorMaskTexturePage`: tiled GPU mask textures
  for visualizing text-detector alpha masks, including LRU metadata for cache eviction.
- `TextDetectionStorageEvent`: async load/save result for `text_detection` disk storage.
- `TranslationSettingsSaveRequest`: snapshot request for async settings persistence.
- `TranslationTabState`: primary tab state (controllers, panel options, caches, pending actions).

Key constants:
- `FOOTER_PATCH_DEBOUNCE_SECS`: debounce before flushing footer patches into model.
- `TEXT_DETECTOR_MASK_TILE_SIDE`: tile size for text-detector mask textures.
- `TEXT_DETECTOR_MASK_TEXTURE_OPTIONS`: texture sampling for mask tiles.
- `TEXT_DETECTOR_MASK_VISUAL_ALPHA_MAX`: max overlay alpha used when drawing detector mask.
- `FOOTER_ADDITIONAL_CHARACTER_NAMES`: built-in extra names for footer character picker.
- `TEXT_DETECTOR_STATUS_OK` / `TEXT_DETECTOR_STATUS_WARN` / `TEXT_DETECTOR_STATUS_ERR`:
  status colors for detector panel.
- `TEXT_DETECTOR_OCR_RETRY_DELAY_SECS`: retry delay for failed detector-ocr block.

TranslationPanel helpers:
- `title_with_hotkey`: localized panel caption with hotkey hint (`[P]`, `[O]`, ...).
- `open_button_label`: compact canvas button caption with hotkey hint (`(P)`, `(O)`, ...).
- `Default::default`: defaults to `TranslationPanel::None`.

OcrDragSelection helper:
- `rect`: current normalized selection rectangle.

TranslationTabState lifecycle:
- `new`: initializes state and starts async settings saver worker.
- `Default::default`: wraps `new(true)`.
- `Drop::drop`: sends shutdown sentinel and joins settings saver thread.

TranslationTabState public API:
- `sync_with_project_settings`: lazy-loads OCR/MT/composition/text-detector settings.
- `draw_side_panel`: draws floating panel window and active panel body.
- `toggle_bubbles_panel_hotkey`, `toggle_ocr_panel_hotkey`,
  `toggle_composition_panel_hotkey`, `toggle_machine_translation_panel_hotkey`,
  `toggle_text_detector_panel_hotkey`: hotkey actions to toggle translation panels.

Panel/UI flow:
- `draw_active_panel`: routes rendering and actions for current active panel.
- `toggle_panel`: toggles panel visibility and handles first-show expand/rebuild flags.
- `toggle_bubbles_panel_hotkey`, `toggle_ocr_panel_hotkey`,
  `toggle_composition_panel_hotkey`, `toggle_machine_translation_panel_hotkey`,
  `toggle_text_detector_panel_hotkey`: hotkey entry points for panel toggles.
- `draw_startup_page_load_toast`: renders startup image-loading progress toast on canvas.
- `draw_toast`: renders toast popup.
- `draw_text_detector_mask_layer_on_page`: draws detector mask and mask-edit interaction on canvas mask layer.
- `draw_text_detector_additional_overlay_on_page`: draws detector boxes/line-edit overlay on additional-elements layer.
- `draw_canvas_overlay_top_left` (CanvasHooks): frame orchestrator, buttons, polling, flushes.

Characters/footer sync:
- `ensure_character_names_loaded`, `reload_character_names`: load character names cache.
- `maybe_refresh_character_names_by_watch`: throttled mtime-watch `characters.json` with auto-refresh.
- `sync_footer_tracking`: tracks bubble lifecycle and prunes footer caches.
- `init_last_footer_values`: bootstraps last-used footer defaults from latest bubble.
- `apply_defaults_for_new_bubble`: applies footer defaults to newly detected bubbles.
- `queue_footer_patch`: buffers one footer field patch with timestamp.
- `flush_footer_patches`: debounced flush of footer patches to `CanvasView::patch_bubble_extra_fields`.
- `footer_state_for`: resolves runtime footer state with overrides.
- `build_bubble_footer` (CanvasHooks): footer editor UI + patch scheduling.
- `build_bubble_header` (CanvasHooks): currently no-op hook.

OCR flow:
- `poll_ocr_events`: consumes OCR controller events, toasts, optional bubble insert, retry logic.
- `handle_ocr_selection`: OCR selection capture layer and finalize behavior.
- `open_advanced_recognition_for_scene_rect`: opens advanced-recognition window for selected scene rect.
- `wants_canvas_shift_drag_selection` (CanvasHooks): reserves canvas drag for OCR capture.

Text detector flow:
- `poll_text_detector_events`: consumes detector events and updates status/progress/results.
- `ensure_text_detection_storage_loaded`: lazy auto-load persisted detector results.
- `start_text_detection_storage_load`, `start_text_detection_storage_save`: async IO jobs.
- `poll_text_detection_storage_events`: applies load/save completion results.
- `set_text_detector_status`: updates detector status text and color.
- `materialize_text_mask_page_from_blocks_if_missing`: legacy fallback that converts
  current detector blocks into a real editable mask if stored results have no `mask_alpha`.
- `has_detected_blocks_on_page`, `has_detected_blocks_any`, `detected_page_indices`:
  availability helpers for detector OCR actions.
- `collect_detected_block_rects_px`: resolved detector blocks with current panel options.
- `textdetector_ocr_is_running`: aggregate detector-OCR busy state.
- `text_detector_run_mode`, `text_detector_running_status`: detector mode/status selection.
- `start_text_detection_for_current_page`, `start_text_detection_for_all_pages`:
  detector run entry points.
- `start_text_detector_ocr_for_indices`: queues OCR tasks for detected blocks.
- `maybe_dispatch_next_textdetector_ocr_request`: scheduler for sequential detector OCR.
- `finish_textdetector_ocr_if_done`: finalize detector OCR batch.
- `abort_textdetector_ocr`: hard-reset detector OCR state after fatal condition.
- `draw_text_detector_line_edit_overlay_on_page`, `handle_text_detector_line_edit_hotkeys`,
  `create_text_detector_line_at_uv`: интерактивный режим правки строк + Del/N + создание из ПКМ.
- `draw_text_detector_mask_edit_overlay_on_page`: интерактивный режим правки маски
  детектора кистью (`ЛКМ` рисует, `ПКМ`/`Shift+ЛКМ` стирают).

Machine translation flow:
- `poll_mt_events`: consumes MT controller events and applies translated text to canvas.
- `mt_has_active_or_pending`: checks running/queued machine-translation work.
- `cancel_active_mt`: cancels queued MT start requests and active worker run.
- `handle_pending_mt_actions`: dispatches deferred MT requests (single bubble/page/all).
- `start_mt_for_scope`: prepares MT items for current page or whole project.
- `start_mt_for_ids`: prepares MT items for explicit bubble ids.
- `start_mt_with_items`: validates options and starts MT controller run.
- `on_bubble_action` (CanvasHooks): queues translate action for bubble button.

Composition/settings handling:
- `rebuild_composition_text`: rebuilds composed text using composition options.
- `ensure_ocr_settings_loaded`, `ensure_mt_settings_loaded`,
  `ensure_composition_settings_loaded`, `ensure_text_detector_settings_loaded`:
  one-time settings hydration per project settings path.
- `flush_settings_save_if_needed`: sends coalesced async save request when any settings are dirty.

Module-level utility functions:
- Character names / detector geometry:
  `build_translation_character_names`, `detector_blocks_with_options`, `detector_expand_blocks`,
  `detector_merge_blocks`, `detector_rects_touch_or_near`, `source_rect_to_scene_rect`.
- Detector mask rendering:
  `draw_text_detector_mask_overlay_on_page`, `build_text_detector_mask_texture_page`,
  `build_text_detector_mask_tile_image`.
- Detector storage paths/IO:
  `text_detection_blocks_file_path`, `text_detection_mask_file_name`,
  `text_detection_mask_file_path`, `load_text_detection_storage`,
  `load_text_detection_page`, `save_text_detection_storage`, `save_text_detection_page`.
- Misc helpers:
  `parse_u32_pair`, `contains_any_page`.
- Async settings persistence:
  `spawn_translation_settings_saver_thread`, `save_translation_settings_to_project_file`.
- Settings parsing/conversion:
  `normalized_lang_input`, `parse_text_detector_algorithm_key`, `parse_ocr_engine_key`,
  `ocr_engine_to_project_key`, `parse_ocr_lang_text_setting`, `parse_single_ocr_lang_setting`.
- OCR request/text helpers:
  `build_ocr_request`, `build_bubble_original_text`.

Key TranslationTabState field groups:
- Panel/controllers/options: `active_panel`, `ai_enabled`, `ocr_controller`, `ocr_panel_options`,
  `ocr_engine_states` + `ocr_loading_engine` (runtime per-engine OCR statuses),
  `ocr_last_panel_engine`/`ocr_last_health_check_request_s`,
  `ai_backend_health*`,
  `mt_controller`, `mt_panel_options`, `text_detector_controller`,
  `text_detector_options`, `composition_panel_options`, `composition_panel_state`.
- Detector runtime/cache: `text_detector_results`, `text_detector_mask_textures`,
  shared `text_mask_model`, `text_mask_synced_revision`,
  `text_detector_status`, `text_detector_status_color`, `text_detector_progress`,
  `text_detection_storage_*`.
- OCR interaction runtime: `ocr_selection`, `ocr_toast`, `next_ocr_request_id`,
  `advanced_recognition`, `advanced_recognition_request`, `pending_bubble_inserts`,
  `pending_textdetector_ocr_tasks`,
  `textdetector_ocr_active_*`, `textdetector_ocr_retry_state`, `textdetector_ocr_* counters`.
- MT runtime: `pending_translate_actions`, `pending_mt_start_all`, `pending_mt_start_page`,
  `mt_progress`, `mt_stop_notice`, `mt_request_preview_rx`, `mt_request_preview`.
- Footer/characters runtime: `character_names`, `characters_loaded_for`,
  `characters_file_mtime`, `character_names_watch_last_check_s`,
  `pending_characters_refresh`, `footer_bootstrapped`,
  `footer_tracking_synced_revision`, `footer_known_ids`,
  `footer_overrides`, `pending_footer_patches`, `pending_footer_patch_changed_at`,
  `footer_character_autocomplete`,
  `last_*` defaults.
- Settings persistence: dirty flags, `*_settings_loaded_for`, `settings_save_tx`,
  `settings_save_thread`.
*/

use crate::bubble_status::{BubbleBorderStyle, BubbleStatusContext, evaluate_bubble_status_rules};
use crate::canvas::{
    BubbleAction, BubbleClass, CanvasHooks, CanvasScrollbarContext, CanvasUiStatus, CanvasView,
    TranslationStatusDisplay,
};
use crate::input_manager_v2::{HotkeyScopeV2, HotkeySpecV2, ModifierOnlyV2};
use crate::memory_manager::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, select_eviction_candidates,
};
use crate::models::text_mask_model::{TextMaskModel, TextMaskPage};
use crate::paste_image;
use crate::project::{Bubble, Page, ProjectData};
use crate::tabs::AppTab;
use crate::tabs::characters::load_character_names;
use crate::tabs::translation::adv_rec::{
    AdvancedRecognitionAction, AdvancedRecognitionSelection, AdvancedRecognitionWindow,
};
use crate::tabs::translation::backend_health::{AiBackendHealthSnapshot, AiBackendProbeCommand};
use crate::tabs::translation::machine_translation::{
    AiMtContextSource, AiMtImageDetail, AiMtImageMode, AiMtOptions, AiMtReasoning, AiMtSortMode,
    MtControllerEvent, MtImageArea, MtImageInput, MtImageSource, MtRequestPreview,
    MtRequestPreviewPart, MtService, MtTranslateItem, MtTranslateRequest, TranslationMtController,
    bubble_order_for_sort, build_ai_mt_request_preview, character_for_bubble,
    is_probable_quota_or_limit_error,
};
use crate::tabs::translation::ocr::{
    AiApiService, OcrControllerEvent, OcrEngine, OcrLoadState, OcrRecognizeRequest,
    OcrRuntimeOptions, TranslationOcrController, is_likely_multimodal_model,
};
use crate::tabs::translation::panels::bubbles::{
    BubbleFooterState, BubblesPanelContext, BubblesPanelState, FOOTER_NO_CHARACTER,
    FOOTER_NO_CHARACTERS, bubble_extra_string, bubble_footer_state_from_record, draw_bubbles_panel,
    footer_state_for_bubble,
};
use crate::tabs::translation::panels::composition::{
    CompositionPanelOptions, CompositionPanelState, CompositionSortMethod, CompositionSourceMode,
    compose_translation_text, draw_composition_panel, normalize_wrap_with,
};
use crate::tabs::translation::panels::machine_translation::{
    MtPanelOptions, MtPanelProgress, MtPanelTab, MtStopNotice, draw_machine_translation_panel,
};
use crate::tabs::translation::panels::ocr::{
    CharReplacementRuleUi, OcrPanelOptions, draw_ocr_panel,
};
use crate::tabs::translation::panels::text_detector::{
    TextDetectorAlgorithm, TextDetectorPanelOptions, draw_text_detector_panel,
};
use crate::tabs::translation::text_detector::{
    TextDetectorAiCtdOptions, TextDetectorControllerEvent, TextDetectorPaddleOcrOptions,
    TextDetectorPageResult, TextDetectorRect, TextDetectorRunMode, TextDetectorSuryaOptions,
    TranslationTextDetectorController,
};
use crate::tools::MaskBrush;
use crate::widgets::{
    AutocompleteLine, MarkFill, ScrollMark, ScrollSpan, WheelComboBox, WheelSlider, WheelSpinBox,
};
use eframe::egui;
use egui::{Color32, Pos2, Rect, Stroke};
use rfd::FileDialog;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::time::SystemTime;

/// (detected results per page, total loaded, total failed)
type DetectionLoadResult = Result<(Vec<(usize, TextDetectorPageResult)>, usize, usize), String>;

const FOOTER_PATCH_DEBOUNCE_SECS: f64 = 0.25;
const TEXT_DETECTOR_MASK_TILE_SIDE: usize = 1024;
const TEXT_DETECTOR_MASK_TEXTURE_OPTIONS: egui::TextureOptions = egui::TextureOptions::NEAREST;
const TEXT_DETECTOR_MASK_VISUAL_ALPHA_MAX: u8 = 96;
const FOOTER_ADDITIONAL_CHARACTER_NAMES: &[&str] = &[
    "Подпись",
    "Звук",
    "ГГ",
    "Мысли ГГ",
    "Непонятно",
    "Кто-то",
    "Кто-то из них",
    "Какая-то девочка",
    "Какой-то мальчик",
    "Какой-то парень",
    "Какая-то девушка",
    "Какая-то женщина",
    "Какой-то мужчина",
];
const TEXT_DETECTOR_STATUS_OK: Color32 = Color32::from_rgb(143, 218, 143);
const TEXT_DETECTOR_STATUS_WARN: Color32 = Color32::from_rgb(247, 201, 72);
const TEXT_DETECTOR_STATUS_ERR: Color32 = Color32::from_rgb(240, 102, 102);
const TEXT_DETECTOR_OCR_RETRY_DELAY_SECS: f64 = 3.0;
const OCR_HEALTH_CHECK_THROTTLE_SECS: f64 = 0.5;
const FOOTER_CHARACTER_AUTOCOMPLETE_MAX: usize = 7;
const CHARACTER_NAMES_WATCH_CHECK_SECS: f64 = 1.0;
const RECENT_CHARACTER_HISTORY_LIMIT: usize = 6;
const RECENT_CHARACTER_CARDS_LEFT_MARGIN: f32 = 340.0;
const RECENT_CHARACTER_CARDS_TOP_MARGIN: f32 = 18.0;
const RECENT_CHARACTER_CARDS_RIGHT_MARGIN: f32 = 24.0;

pub const HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE: &str =
    "translation.ocr.quick.selection_mode";
pub const HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE: &str =
    "translation.ocr.advanced.selection_mode";
pub const HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL: &str = "translation.panel.bubbles.toggle";
pub const HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL: &str = "translation.panel.ocr.toggle";
pub const HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL: &str =
    "translation.panel.composition.toggle";
pub const HOTKEY_TRANSLATION_TOGGLE_MT_PANEL: &str = "translation.panel.machine_translation.toggle";
pub const HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL: &str = "translation.panel.detector.toggle";
pub const HOTKEY_TRANSLATION_COPY_BUBBLE_ORIGINAL: &str = "translation.bubble.copy_original";
pub const HOTKEY_TRANSLATION_COPY_BUBBLE_TRANSLATION: &str = "translation.bubble.copy_translation";
pub const HOTKEY_TRANSLATION_PASTE_BUBBLE_ORIGINAL: &str = "translation.bubble.paste_original";
pub const HOTKEY_TRANSLATION_PASTE_BUBBLE_TRANSLATION: &str =
    "translation.bubble.paste_translation";

#[derive(Debug, Clone, Default)]
pub struct TranslationHotkeyHints {
    pub ocr_quick_selection_mode: Option<String>,
    pub ocr_quick_selection_mode_modifier_down: bool,
    pub ocr_advanced_selection_mode: Option<String>,
    pub ocr_advanced_selection_mode_modifier_down: bool,
    pub bubbles_panel: Option<String>,
    pub ocr_panel: Option<String>,
    pub composition_panel: Option<String>,
    pub machine_translation_panel: Option<String>,
    pub text_detector_panel: Option<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum TranslationPanel {
    #[default]
    None,
    Bubbles,
    Ocr,
    Composition,
    MachineTranslation,
    TextDetector,
}

impl TranslationPanel {
    fn title(self) -> &'static str {
        match self {
            TranslationPanel::None => "",
            TranslationPanel::Bubbles => "Пузыри",
            TranslationPanel::Ocr => "Распознавание текста",
            TranslationPanel::Composition => "Компоновка",
            TranslationPanel::MachineTranslation => "Машинный/ИИ перевод",
            TranslationPanel::TextDetector => "Массовый детектор текста",
        }
    }

    fn short_button_title(self) -> &'static str {
        match self {
            TranslationPanel::None => "",
            TranslationPanel::Bubbles => "Пузыри",
            TranslationPanel::Ocr => "Распознавание текста",
            TranslationPanel::Composition => "Компоновка",
            TranslationPanel::MachineTranslation => "Маш./ИИ перевод",
            TranslationPanel::TextDetector => "Массовый детектор текста",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum OcrSelectionKind {
    Simple,
    Advanced,
}

#[derive(Debug, Clone)]
struct OcrDragSelection {
    start: Pos2,
    current: Pos2,
    kind: OcrSelectionKind,
    recent_character_rank: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
struct ImageCropDragSelection {
    start: Pos2,
    current: Pos2,
}

impl ImageCropDragSelection {
    fn rect(self) -> Rect {
        Rect::from_two_pos(self.start, self.current)
    }
}

impl OcrDragSelection {
    fn rect(&self) -> Rect {
        Rect::from_two_pos(self.start, self.current)
    }
}

#[derive(Debug, Clone)]
struct OcrToast {
    text: String,
    color: Color32,
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
        pages: Vec<(usize, TextDetectorPageResult)>,
        loaded: usize,
        failed: usize,
    },
    Saved {
        project_dir: PathBuf,
        saved: usize,
        failed: usize,
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
    text_detector_options: TextDetectorPanelOptions,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct RecentCharacterEntry {
    is_known_character: bool,
    character_name: String,
    clarification: String,
}

pub struct TranslationTabState {
    active_panel: TranslationPanel,
    ai_enabled: bool,
    ai_backend_health: Arc<Mutex<AiBackendHealthSnapshot>>,
    ai_backend_health_cached: AiBackendHealthSnapshot,
    ai_backend_probe_tx: Option<Sender<AiBackendProbeCommand>>,
    ocr_controller: TranslationOcrController,
    ocr_panel_options: OcrPanelOptions,
    ocr_engine_states: [OcrLoadState; 6],
    ocr_loading_engine: Option<OcrEngine>,
    ocr_last_panel_engine: Option<OcrEngine>,
    ocr_last_health_check_request_s: f64,
    mt_controller: TranslationMtController,
    mt_panel_options: MtPanelOptions,
    text_detector_controller: TranslationTextDetectorController,
    text_detector_options: TextDetectorPanelOptions,
    text_detector_results: HashMap<usize, TextDetectorPageResult>,
    text_detector_mask_textures: HashMap<usize, TextDetectorMaskTexturePage>,
    text_mask_model: Option<Arc<Mutex<TextMaskModel>>>,
    text_mask_synced_revision: u64,
    text_detector_status: String,
    text_detector_status_color: Color32,
    text_detector_progress: Option<(usize, usize)>,
    text_detector_edit_lines_mode: bool,
    text_detector_edit_mask_mode: bool,
    text_detector_line_selection: Option<TextDetectorLineSelection>,
    text_detector_line_drag_state: Option<TextDetectorLineDragState>,
    text_detector_mask_stroke_state: Option<TextDetectorMaskStrokeState>,
    text_detector_mask_brush: MaskBrush,
    text_detection_storage_loaded_for: Option<PathBuf>,
    text_detection_storage_busy: bool,
    text_detection_storage_rx: Option<Receiver<TextDetectionStorageEvent>>,
    ocr_selection: Option<OcrDragSelection>,
    image_crop_selection: Option<ImageCropDragSelection>,
    /// Edge gate for the plain-`Q` image-bubble shortcut. Cleared while `Q` participates in a
    /// `Shift+Q` crop session or after a creation, and re-armed only once `Q` is fully released, so
    /// a lingering `Q` (e.g. `Shift` released a frame before `Q`) cannot spawn an extra bubble.
    image_create_q_armed: bool,
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
    character_names: Vec<String>,
    characters_loaded_for: Option<PathBuf>,
    characters_file_mtime: Option<SystemTime>,
    character_names_watch_last_check_s: f64,
    pending_characters_refresh: bool,
    footer_bootstrapped: bool,
    // Last `CanvasView::hook_bubbles_revision()` for which `sync_footer_tracking` ran a full
    // recompute. Used to skip the per-frame snapshot/clone/sort when the bubble set is unchanged.
    footer_tracking_synced_revision: Option<u64>,
    footer_known_ids: HashSet<i64>,
    footer_overrides: HashMap<i64, BubbleFooterState>,
    pending_footer_patches: HashMap<i64, Map<String, Value>>,
    pending_footer_patch_changed_at: HashMap<i64, f64>,
    footer_character_autocomplete: HashMap<i64, AutocompleteLine>,
    bubbles_panel: BubblesPanelState,
    composition_panel_options: CompositionPanelOptions,
    composition_panel_state: CompositionPanelState,
    composition_rebuild_requested: bool,
    expand_panel_on_show: bool,
    hotkey_hints: TranslationHotkeyHints,
    last_is_known_character: bool,
    last_character_name: String,
    last_clarification: String,
    recent_characters: VecDeque<RecentCharacterEntry>,
    last_page_idx: i64,
    last_bubble_order: i32,
}

impl Default for TranslationTabState {
    fn default() -> Self {
        Self::new(
            true,
            Arc::new(Mutex::new(AiBackendHealthSnapshot::default())),
            None,
        )
    }
}

impl TranslationTabState {
    pub fn hotkey_specs() -> [HotkeySpecV2; 11] {
        [
            HotkeySpecV2 {
                id: HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE,
                title: "Быстрое распознавание: режим выделения",
                section: "OCR",
                default_shortcut: None,
                default_modifier_only: Some(ModifierOnlyV2::Shift),
                scope: HotkeyScopeV2::Tab(AppTab::Translation),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE,
                title: "Продвинутое распознавание: режим выделения",
                section: "OCR",
                default_shortcut: None,
                default_modifier_only: Some(ModifierOnlyV2::Alt),
                scope: HotkeyScopeV2::Tab(AppTab::Translation),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL,
                title: "Открыть панель пузырей",
                section: "Панели",
                default_shortcut: Some(egui::KeyboardShortcut::new(
                    egui::Modifiers::NONE,
                    egui::Key::P,
                )),
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Translation),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL,
                title: "Открыть панель распознавания",
                section: "Панели",
                default_shortcut: Some(egui::KeyboardShortcut::new(
                    egui::Modifiers::NONE,
                    egui::Key::S,
                )),
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Translation),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL,
                title: "Открыть панель компоновки",
                section: "Панели",
                default_shortcut: Some(egui::KeyboardShortcut::new(
                    egui::Modifiers::NONE,
                    egui::Key::K,
                )),
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Translation),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_TRANSLATION_TOGGLE_MT_PANEL,
                title: "Открыть панель машинного перевода",
                section: "Панели",
                default_shortcut: Some(egui::KeyboardShortcut::new(
                    egui::Modifiers::NONE,
                    egui::Key::M,
                )),
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Translation),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL,
                title: "Открыть панель детектора текста",
                section: "Панели",
                default_shortcut: Some(egui::KeyboardShortcut::new(
                    egui::Modifiers::NONE,
                    egui::Key::D,
                )),
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Translation),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_TRANSLATION_COPY_BUBBLE_ORIGINAL,
                title: "Копировать оригинал выбранного пузыря",
                section: "Пузыри",
                default_shortcut: None,
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Translation),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_TRANSLATION_COPY_BUBBLE_TRANSLATION,
                title: "Копировать перевод выбранного пузыря",
                section: "Пузыри",
                default_shortcut: None,
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Translation),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_TRANSLATION_PASTE_BUBBLE_ORIGINAL,
                title: "Вставить с заменой в оригинал выбранного пузыря",
                section: "Пузыри",
                default_shortcut: None,
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Translation),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_TRANSLATION_PASTE_BUBBLE_TRANSLATION,
                title: "Вставить с заменой в перевод выбранного пузыря",
                section: "Пузыри",
                default_shortcut: None,
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Translation),
                active_when_input: false,
            },
        ]
    }

    pub fn new(
        ai_enabled: bool,
        ai_backend_health: Arc<Mutex<AiBackendHealthSnapshot>>,
        ai_backend_probe_tx: Option<Sender<AiBackendProbeCommand>>,
    ) -> Self {
        let (settings_save_tx, settings_save_thread) = spawn_translation_settings_saver_thread();
        Self {
            active_panel: TranslationPanel::None,
            ai_enabled,
            ai_backend_health: Arc::clone(&ai_backend_health),
            ai_backend_health_cached: match ai_backend_health.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            },
            ai_backend_probe_tx,
            ocr_controller: TranslationOcrController::default(),
            ocr_panel_options: OcrPanelOptions::default(),
            ocr_engine_states: [OcrLoadState::NotLoaded; 6],
            ocr_loading_engine: None,
            ocr_last_panel_engine: None,
            ocr_last_health_check_request_s: -10_000.0,
            mt_controller: TranslationMtController::default(),
            mt_panel_options: MtPanelOptions::default(),
            text_detector_controller: TranslationTextDetectorController::default(),
            text_detector_options: TextDetectorPanelOptions::default(),
            text_detector_results: HashMap::new(),
            text_detector_mask_textures: HashMap::new(),
            text_mask_model: None,
            text_mask_synced_revision: 0,
            text_detector_status: "Готов к работе".to_string(),
            text_detector_status_color: TEXT_DETECTOR_STATUS_OK,
            text_detector_progress: None,
            text_detector_edit_lines_mode: false,
            text_detector_edit_mask_mode: false,
            text_detector_line_selection: None,
            text_detector_line_drag_state: None,
            text_detector_mask_stroke_state: None,
            text_detector_mask_brush: MaskBrush::default(),
            text_detection_storage_loaded_for: None,
            text_detection_storage_busy: false,
            text_detection_storage_rx: None,
            ocr_selection: None,
            image_crop_selection: None,
            image_create_q_armed: true,
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
            characters_loaded_for: None,
            characters_file_mtime: None,
            character_names_watch_last_check_s: -10_000.0,
            pending_characters_refresh: false,
            footer_bootstrapped: false,
            footer_tracking_synced_revision: None,
            footer_known_ids: HashSet::new(),
            footer_overrides: HashMap::new(),
            pending_footer_patches: HashMap::new(),
            pending_footer_patch_changed_at: HashMap::new(),
            footer_character_autocomplete: HashMap::new(),
            bubbles_panel: BubblesPanelState::default(),
            composition_panel_options: CompositionPanelOptions::default(),
            composition_panel_state: CompositionPanelState::default(),
            composition_rebuild_requested: true,
            expand_panel_on_show: false,
            hotkey_hints: TranslationHotkeyHints::default(),
            last_is_known_character: true,
            last_character_name: String::new(),
            last_clarification: String::new(),
            recent_characters: VecDeque::new(),
            last_page_idx: -1,
            last_bubble_order: -1,
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

impl Drop for TranslationTabState {
    fn drop(&mut self) {
        let _ = self.settings_save_tx.send(TranslationSettingsSaveRequest {
            settings_file: PathBuf::new(),
            ocr_options: OcrPanelOptions::default(),
            mt_options: MtPanelOptions::default(),
            composition_options: CompositionPanelOptions::default(),
            text_detector_options: TextDetectorPanelOptions::default(),
        });
        if let Some(handle) = self.settings_save_thread.take() {
            let _ = handle.join();
        }
    }
}

impl TranslationTabState {
    fn ocr_engine_index(engine: OcrEngine) -> usize {
        match engine {
            OcrEngine::MangaOcr => 0,
            OcrEngine::EasyOcr => 1,
            OcrEngine::PaddleOcr => 2,
            OcrEngine::Surya => 3,
            OcrEngine::AiApi => 4,
            OcrEngine::PaddleVl => 5,
        }
    }

    fn ocr_state_for_engine(&self, engine: OcrEngine) -> OcrLoadState {
        self.ocr_engine_states[Self::ocr_engine_index(engine)]
    }

    fn set_ocr_state_for_engine(&mut self, engine: OcrEngine, state: OcrLoadState) {
        self.ocr_engine_states[Self::ocr_engine_index(engine)] = state;
    }

    fn mark_ocr_load_requested_for_engine(&mut self, engine: OcrEngine) {
        self.set_ocr_state_for_engine(engine, OcrLoadState::Loading);
        self.ocr_loading_engine = Some(engine);
    }

    fn mark_ocr_model_download_started_for_engine(&mut self, engine: OcrEngine) {
        self.set_ocr_state_for_engine(engine, OcrLoadState::DownloadingModel);
        self.ocr_loading_engine = Some(engine);
    }

    fn sync_ocr_states_from_backend_health_snapshot(&mut self) {
        let snapshot = self.ai_backend_health_snapshot();
        if !snapshot.connected {
            return;
        }
        let manga_ready = snapshot.ocr_manga_ready;
        let easy_ready = snapshot.ocr_easy_ready;
        let paddle_ready = snapshot.ocr_paddle_ready;
        let paddle_vl_ready = snapshot.ocr_paddle_vl_ready;
        let surya_ready = snapshot.ocr_surya_ready;
        let sync_engine = |this: &mut Self, engine: OcrEngine, ready: Option<bool>| {
            let Some(ready) = ready else {
                return;
            };
            if ready {
                this.set_ocr_state_for_engine(engine, OcrLoadState::Ready);
            } else if !this.ocr_state_for_engine(engine).is_busy() {
                this.set_ocr_state_for_engine(engine, OcrLoadState::NotLoaded);
            }
        };
        sync_engine(self, OcrEngine::MangaOcr, manga_ready);
        sync_engine(self, OcrEngine::EasyOcr, easy_ready);
        sync_engine(self, OcrEngine::PaddleOcr, paddle_ready);
        sync_engine(self, OcrEngine::PaddleVl, paddle_vl_ready);
        sync_engine(self, OcrEngine::Surya, surya_ready);
    }

    fn request_ai_backend_health_check(&mut self, ctx: &egui::Context, force: bool) {
        if !self.ai_enabled {
            return;
        }
        let snapshot = self.ai_backend_health_snapshot();
        let now_s = ctx.input(|i| i.time);
        let stale = snapshot
            .checked_at
            .map(|checked| checked.elapsed().as_secs_f64() >= OCR_HEALTH_CHECK_THROTTLE_SECS)
            .unwrap_or(true);
        if (force || stale)
            && now_s - self.ocr_last_health_check_request_s >= OCR_HEALTH_CHECK_THROTTLE_SECS
            && let Some(tx) = self.ai_backend_probe_tx.as_ref()
        {
            let _ = tx.send(AiBackendProbeCommand::CheckNow);
            self.ocr_last_health_check_request_s = now_s;
        }
    }

    pub fn blocks_canvas_bubble_hotkeys(&self) -> bool {
        self.text_detector_edit_lines_mode
            || self.text_detector_edit_mask_mode
            || self.advanced_recognition.is_open()
    }

    pub fn blocks_canvas_zoom(&self) -> bool {
        self.advanced_recognition.is_open()
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

    pub fn set_hotkey_hints(&mut self, hints: TranslationHotkeyHints) {
        self.hotkey_hints = hints;
    }

    fn panel_shortcut_hint(&self, panel: TranslationPanel) -> Option<&str> {
        match panel {
            TranslationPanel::None => None,
            TranslationPanel::Bubbles => self.hotkey_hints.bubbles_panel.as_deref(),
            TranslationPanel::Ocr => self.hotkey_hints.ocr_panel.as_deref(),
            TranslationPanel::Composition => self.hotkey_hints.composition_panel.as_deref(),
            TranslationPanel::MachineTranslation => {
                self.hotkey_hints.machine_translation_panel.as_deref()
            }
            TranslationPanel::TextDetector => self.hotkey_hints.text_detector_panel.as_deref(),
        }
    }

    fn active_panel_title(&self) -> String {
        self.panel_title(self.active_panel)
    }

    fn ocr_quick_selection_mode_active(&self) -> bool {
        self.hotkey_hints.ocr_quick_selection_mode_modifier_down
    }

    fn ocr_advanced_selection_mode_active(&self) -> bool {
        self.hotkey_hints.ocr_advanced_selection_mode_modifier_down
    }

    fn image_crop_selection_mode_active(ctx: &egui::Context) -> bool {
        ctx.input(|input| {
            input.modifiers.shift && input.key_down(egui::Key::Q) && !input.any_touches()
        }) && !ctx.wants_keyboard_input()
    }

    fn panel_title(&self, panel: TranslationPanel) -> String {
        match self.panel_shortcut_hint(panel) {
            Some(shortcut) if !shortcut.is_empty() => format!("{} [{}]", panel.title(), shortcut),
            _ => panel.title().to_string(),
        }
    }

    fn open_button_label(&self, panel: TranslationPanel) -> String {
        match self.panel_shortcut_hint(panel) {
            Some(shortcut) if !shortcut.is_empty() => {
                format!("{} ({shortcut})", panel.short_button_title())
            }
            _ => panel.short_button_title().to_string(),
        }
    }

    pub fn draw_side_panel(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        if self.active_panel == TranslationPanel::None {
            return;
        }
        if self.active_panel == TranslationPanel::Bubbles {
            self.ensure_character_names_loaded(project);
        }
        self.refresh_ai_backend_health_snapshot_cache();

        let viewport = ctx.content_rect();
        let default_pos = egui::pos2(
            (viewport.right() - 372.0).max(0.0),
            (viewport.center().y - 230.0).max(0.0),
        );
        let panel_window_id = egui::Id::new("translation_overlay_panel");
        if self.expand_panel_on_show {
            let mut collapsing = egui::collapsing_header::CollapsingState::load_with_default_open(
                ctx,
                panel_window_id.with("collapsing"),
                true,
            );
            collapsing.set_open(true);
            collapsing.store(ctx);
            self.expand_panel_on_show = false;
        }

        let mut panel_open = true;
        egui::Window::new(self.active_panel_title())
            .id(panel_window_id)
            .open(&mut panel_open)
            .default_pos(default_pos)
            .default_size(egui::vec2(360.0, 460.0))
            .resizable(true)
            .show(ctx, |ui| {
                self.draw_active_panel(ui, ctx, canvas, project);
            });
        if !panel_open {
            if self.active_panel == TranslationPanel::TextDetector {
                self.set_text_detector_edit_lines_mode(false);
                self.set_text_detector_edit_mask_mode(false);
            }
            self.active_panel = TranslationPanel::None;
        }
    }

    pub fn toggle_ocr_panel_hotkey(&mut self) {
        self.toggle_panel(TranslationPanel::Ocr);
    }

    pub fn toggle_bubbles_panel_hotkey(&mut self) {
        self.toggle_panel(TranslationPanel::Bubbles);
    }

    pub fn toggle_composition_panel_hotkey(&mut self) {
        self.toggle_panel(TranslationPanel::Composition);
    }

    pub fn toggle_machine_translation_panel_hotkey(&mut self) {
        self.toggle_panel(TranslationPanel::MachineTranslation);
    }

    pub fn toggle_text_detector_panel_hotkey(&mut self) {
        self.toggle_panel(TranslationPanel::TextDetector);
    }

    fn refresh_ai_backend_health_snapshot_cache(&mut self) {
        self.ai_backend_health_cached = match self.ai_backend_health.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
    }

    fn ai_backend_health_snapshot(&self) -> &AiBackendHealthSnapshot {
        &self.ai_backend_health_cached
    }

    fn ai_backend_unavailable(&self) -> bool {
        if !self.ai_enabled {
            return false;
        }
        let snapshot = self.ai_backend_health_snapshot();
        snapshot.checked_at.is_some() && !snapshot.connected
    }

    fn ai_backend_torch_available(&self) -> Option<bool> {
        self.ai_backend_health_snapshot()
            .is_torch_available
            .or_else(crate::ai_backend_capabilities::torch_available)
    }

    fn selected_ocr_mode_requires_torch(&self) -> bool {
        match self.ocr_panel_options.engine {
            OcrEngine::EasyOcr | OcrEngine::PaddleVl | OcrEngine::Surya => true,
            OcrEngine::MangaOcr => self
                .ocr_panel_options
                .manga_model
                .trim()
                .eq_ignore_ascii_case("base_torch"),
            OcrEngine::PaddleOcr | OcrEngine::AiApi => false,
        }
    }

    fn current_ocr_torch_requirement_error(&self) -> Option<String> {
        if !self.selected_ocr_mode_requires_torch() {
            return None;
        }
        matches!(self.ai_backend_torch_available(), Some(false))
            .then(|| "PyTorch не установлен".to_string())
    }

    fn draw_active_panel(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        match self.active_panel {
            TranslationPanel::Bubbles => {
                let mut panel_ctx = BubblesPanelContext {
                    character_names: &self.character_names,
                    footer_overrides: &mut self.footer_overrides,
                    pending_footer_patches: &mut self.pending_footer_patches,
                    pending_footer_patch_changed_at: &mut self.pending_footer_patch_changed_at,
                    pending_characters_refresh: &mut self.pending_characters_refresh,
                    last_is_known_character: &mut self.last_is_known_character,
                    last_character_name: &mut self.last_character_name,
                    last_clarification: &mut self.last_clarification,
                    last_page_idx: &mut self.last_page_idx,
                    last_bubble_order: &mut self.last_bubble_order,
                };
                draw_bubbles_panel(
                    &mut self.bubbles_panel,
                    ui,
                    ctx,
                    canvas,
                    project,
                    &mut panel_ctx,
                );
            }
            TranslationPanel::Ocr => {
                let selected_engine_before = self.ocr_panel_options.engine;
                let force_health_check = self.ocr_last_panel_engine != Some(selected_engine_before);
                if ocr_engine_requires_backend_runtime(&self.ocr_panel_options) {
                    self.request_ai_backend_health_check(ctx, force_health_check);
                    self.sync_ocr_states_from_backend_health_snapshot();
                }
                let backend_unavailable =
                    ocr_engine_requires_backend_runtime(&self.ocr_panel_options)
                        && self.ai_backend_unavailable();
                let torch_available = self.ai_backend_torch_available();
                let quick_selection_active = self.ocr_quick_selection_mode_active();
                let advanced_selection_active = self.ocr_advanced_selection_mode_active();
                let actions = ui
                    .add_enabled_ui(self.ai_enabled, |ui| {
                        draw_ocr_panel(
                            ui,
                            self.ocr_state_for_engine(self.ocr_panel_options.engine),
                            &mut self.ocr_panel_options,
                            backend_unavailable,
                            torch_available,
                            self.ocr_controller.last_error(),
                            self.ocr_controller.last_result(),
                            self.hotkey_hints.ocr_quick_selection_mode.as_deref(),
                            quick_selection_active,
                            self.hotkey_hints.ocr_advanced_selection_mode.as_deref(),
                            advanced_selection_active,
                        )
                    })
                    .inner;
                if self.ai_enabled {
                    if actions.options_changed {
                        self.ocr_settings_dirty = true;
                        if selected_engine_before != self.ocr_panel_options.engine
                            && ocr_engine_requires_backend_runtime(&self.ocr_panel_options)
                        {
                            self.request_ai_backend_health_check(ctx, true);
                            self.sync_ocr_states_from_backend_health_snapshot();
                        }
                    }
                    if actions.save_ai_api_key {
                        self.ocr_panel_options.ai_api_status = "Сохранение API key...".to_string();
                        self.ocr_controller.store_ai_api_key(
                            self.ocr_panel_options.ai_api_service,
                            self.ocr_panel_options.ai_api_key_edit.clone(),
                        );
                    }
                    if actions.clear_ai_api_key {
                        self.ocr_panel_options.ai_api_status = "Удаление API key...".to_string();
                        self.ocr_controller
                            .clear_ai_api_key(self.ocr_panel_options.ai_api_service);
                    }
                    if actions.refresh_ai_api_metadata {
                        self.ocr_panel_options.ai_api_status =
                            "Обновление AI API данных...".to_string();
                        self.ocr_controller
                            .refresh_ai_api_metadata(self.ocr_panel_options.ai_api_service);
                    }
                    if actions.request_load && !backend_unavailable {
                        if let Some(error) = self.current_ocr_torch_requirement_error() {
                            self.push_toast(ctx, error, Color32::RED, 2.8);
                        } else {
                            self.mark_ocr_load_requested_for_engine(self.ocr_panel_options.engine);
                            self.ocr_controller.request_load(
                                self.ocr_panel_options.engine,
                                build_ocr_runtime_options(&self.ocr_panel_options),
                            );
                        }
                    }
                } else {
                    ui.separator();
                    ui.colored_label(
                        Color32::from_rgb(225, 180, 60),
                        "OCR отключён флагом --no-ai.",
                    );
                    ui.small("Перезапустите приложение без --no-ai, чтобы использовать OCR.");
                }
                self.ocr_last_panel_engine = Some(self.ocr_panel_options.engine);
            }
            TranslationPanel::MachineTranslation => {
                let mt_busy = self.mt_controller.is_busy();
                let mt_can_cancel = self.mt_has_active_or_pending();
                let actions = ui
                    .add_enabled_ui(self.ai_enabled, |ui| {
                        draw_machine_translation_panel(
                            ui,
                            mt_busy,
                            mt_can_cancel,
                            self.mt_progress,
                            &mut self.mt_stop_notice,
                            &mut self.mt_panel_options,
                        )
                    })
                    .inner;
                if self.ai_enabled {
                    if actions.options_changed {
                        self.mt_settings_dirty = true;
                    }
                    if actions.save_ai_api_key {
                        self.mt_panel_options.ai_api_status = "Сохранение API key...".to_string();
                        self.mt_controller.store_ai_api_key(
                            self.mt_panel_options.ai_api_service,
                            self.mt_panel_options.ai_api_key_edit.clone(),
                        );
                    }
                    if actions.clear_ai_api_key {
                        self.mt_panel_options.ai_api_status = "Удаление API key...".to_string();
                        self.mt_controller
                            .clear_ai_api_key(self.mt_panel_options.ai_api_service);
                    }
                    if actions.refresh_ai_api_metadata {
                        self.mt_panel_options.ai_api_status =
                            "Обновление AI API данных...".to_string();
                        self.mt_controller
                            .refresh_ai_api_metadata(self.mt_panel_options.ai_api_service);
                    }
                    if actions.start_all {
                        self.pending_mt_start_all = true;
                    }
                    if actions.start_page {
                        self.pending_mt_start_page = true;
                    }
                    if actions.preview_request_page {
                        self.start_ai_mt_request_preview(ctx, canvas, project, true);
                    }
                    if actions.preview_request_all {
                        self.start_ai_mt_request_preview(ctx, canvas, project, false);
                    }
                    if actions.cancel {
                        self.cancel_active_mt(ctx);
                    }
                    self.poll_ai_mt_request_preview(ctx);
                    self.draw_ai_mt_request_preview_window(ctx);
                } else {
                    ui.separator();
                    ui.colored_label(
                        Color32::from_rgb(225, 180, 60),
                        "Машинный перевод отключён флагом --no-ai.",
                    );
                    ui.small(
                        "Перезапустите приложение без --no-ai, чтобы использовать машинный перевод.",
                    );
                }
            }
            TranslationPanel::Composition => {
                if self.composition_rebuild_requested {
                    self.rebuild_composition_text(project);
                }
                let actions = draw_composition_panel(
                    ui,
                    project,
                    &mut self.composition_panel_state,
                    &mut self.composition_panel_options,
                );
                if actions.options_changed {
                    self.composition_settings_dirty = true;
                }
                if actions.request_rebuild {
                    self.rebuild_composition_text(project);
                }
            }
            TranslationPanel::TextDetector => {
                self.ensure_text_detection_storage_loaded(project);
                let has_pages = !project.pages.is_empty();
                let can_detect = match self.text_detector_options.algorithm {
                    TextDetectorAlgorithm::Classic => true,
                    TextDetectorAlgorithm::PaddleOcr => self.ai_enabled,
                    TextDetectorAlgorithm::Ai => {
                        self.ai_enabled && !matches!(self.ai_backend_torch_available(), Some(false))
                    }
                    TextDetectorAlgorithm::Surya => {
                        self.ai_enabled && !matches!(self.ai_backend_torch_available(), Some(false))
                    }
                };
                let can_ocr_current = self.ai_enabled
                    && self.ocr_controller.state() == OcrLoadState::Ready
                    && self.has_detected_blocks_on_page(canvas.current_page_idx());
                let can_ocr_all = self.ai_enabled
                    && self.ocr_controller.state() == OcrLoadState::Ready
                    && self.has_detected_blocks_any();
                let can_save =
                    !self.text_detector_results.is_empty() && !self.text_detection_storage_busy;
                let ocr_busy = self.textdetector_ocr_is_running();
                let torch_available = self.ai_backend_torch_available();
                let actions = draw_text_detector_panel(
                    ui,
                    &mut self.text_detector_options,
                    &self.text_detector_status,
                    self.text_detector_status_color,
                    self.text_detector_progress,
                    self.text_detector_controller.is_busy(),
                    ocr_busy,
                    has_pages,
                    can_detect,
                    torch_available,
                    can_ocr_current,
                    can_ocr_all,
                    can_save,
                    self.text_detector_edit_lines_mode,
                    self.text_detector_edit_mask_mode,
                );
                if actions.options_changed {
                    self.text_detector_settings_dirty = true;
                }
                if actions.toggle_edit_lines_mode {
                    self.set_text_detector_edit_lines_mode(!self.text_detector_edit_lines_mode);
                }
                if actions.toggle_edit_mask_mode {
                    self.set_text_detector_edit_mask_mode(!self.text_detector_edit_mask_mode);
                }
                if actions.save_results {
                    self.start_text_detection_storage_save(project);
                }
                if actions.clear_results {
                    self.text_detector_results.clear();
                    self.text_detector_mask_textures.clear();
                    self.clear_text_mask_model();
                    self.text_detector_progress = None;
                    self.clear_text_detector_line_edit_state();
                    self.set_text_detector_status("Результаты очищены", TEXT_DETECTOR_STATUS_OK);
                }
                if actions.detect_current {
                    self.start_text_detection_for_current_page(project, canvas);
                }
                if actions.detect_all {
                    self.start_text_detection_for_all_pages(project);
                }
                if actions.ocr_current {
                    self.start_text_detector_ocr_for_indices(
                        ctx,
                        project,
                        vec![canvas.current_page_idx()],
                    );
                }
                if actions.ocr_all {
                    self.start_text_detector_ocr_for_indices(
                        ctx,
                        project,
                        self.detected_page_indices(),
                    );
                }

                if !self.ai_enabled {
                    ui.separator();
                    ui.colored_label(
                        Color32::from_rgb(225, 180, 60),
                        "Распознавание по найденным блокам отключено флагом --no-ai.",
                    );
                } else if self.ocr_controller.state() != OcrLoadState::Ready {
                    ui.separator();
                    ui.small("Для распознавания по блокам сначала загрузите движок в Распознавании текста");
                }
            }
            _ => {
                ui.label("Раздел переносится из Python-версии.");
            }
        }
    }

    fn toggle_panel(&mut self, panel: TranslationPanel) {
        let prev = self.active_panel;
        let next = if self.active_panel == panel {
            TranslationPanel::None
        } else {
            panel
        };
        if next == TranslationPanel::Composition {
            self.composition_rebuild_requested = true;
        }
        if next != TranslationPanel::None && next != prev {
            self.expand_panel_on_show = true;
        }
        if prev == TranslationPanel::TextDetector && next != TranslationPanel::TextDetector {
            self.set_text_detector_edit_lines_mode(false);
            self.set_text_detector_edit_mask_mode(false);
        }
        self.active_panel = next;
    }

    fn ensure_character_names_loaded(&mut self, project: &ProjectData) {
        let chars_dir = project.paths.characters_dir.clone();
        let needs_load = match self.characters_loaded_for.as_ref() {
            Some(loaded) => *loaded != chars_dir,
            None => true,
        };
        if needs_load {
            self.reload_character_names(project);
        }
    }

    fn reload_character_names(&mut self, project: &ProjectData) {
        let loaded = load_character_names(project).unwrap_or_default();
        self.character_names = build_translation_character_names(loaded);
        self.characters_loaded_for = Some(project.paths.characters_dir.clone());
        self.characters_file_mtime = characters_file_mtime(project);
    }

    fn maybe_refresh_character_names_by_watch(&mut self, project: &ProjectData, now_s: f64) {
        if now_s - self.character_names_watch_last_check_s < CHARACTER_NAMES_WATCH_CHECK_SECS {
            return;
        }
        self.character_names_watch_last_check_s = now_s;
        let mtime = characters_file_mtime(project);
        if mtime != self.characters_file_mtime {
            self.pending_characters_refresh = true;
        }
    }

    /// Tracks bubble lifecycle and prunes footer caches, applying footer defaults to newly
    /// detected bubbles.
    ///
    /// Runs every frame from `draw_canvas_overlay_top_left`, so the expensive recompute (full
    /// bubble snapshot clone + id `HashSet` + recent-character history) is gated on
    /// `CanvasView::hook_bubbles_revision()`: when the revision is unchanged and footer tracking
    /// was already bootstrapped, the call returns early and reuses the cached footer state. A
    /// newly-created bubble (even one living only in `runtime_bubbles`) bumps the revision, so the
    /// "apply defaults for new bubble" path still fires. The first call always recomputes.
    fn sync_footer_tracking(&mut self, canvas: &CanvasView, project: &ProjectData) {
        let current_revision = canvas.hook_bubbles_revision();
        if !footer_tracking_should_recompute(
            self.footer_tracking_synced_revision,
            current_revision,
            self.footer_bootstrapped,
        ) {
            return;
        }
        self.footer_tracking_synced_revision = Some(current_revision);

        let live_bubbles = canvas.hook_bubbles_snapshot(project);
        let known_now = live_bubbles
            .iter()
            .map(|bubble| bubble.id)
            .collect::<HashSet<_>>();
        if !self.footer_bootstrapped {
            self.footer_known_ids = known_now;
            self.init_last_footer_values(&live_bubbles);
            self.footer_bootstrapped = true;
            return;
        }

        for bubble in &live_bubbles {
            if self.footer_known_ids.insert(bubble.id) {
                self.apply_defaults_for_new_bubble(bubble, canvas.state.auto_insert_last_character);
            }
        }

        self.footer_known_ids.retain(|bid| known_now.contains(bid));
        self.footer_overrides
            .retain(|bid, _| self.footer_known_ids.contains(bid));
        self.pending_footer_patches
            .retain(|bid, _| self.footer_known_ids.contains(bid));
        self.pending_footer_patch_changed_at
            .retain(|bid, _| self.footer_known_ids.contains(bid));
        self.footer_character_autocomplete
            .retain(|bid, _| self.footer_known_ids.contains(bid));
        self.recent_characters = collect_recent_character_history(project.bubbles.as_ref());
    }

    fn init_last_footer_values(&mut self, bubbles: &[Bubble]) {
        self.recent_characters = collect_recent_character_history(bubbles);
        let Some(last) = bubbles.iter().max_by_key(|bubble| bubble.id) else {
            return;
        };
        let state = bubble_footer_state_from_record(last);
        self.last_is_known_character = state.is_known_character;
        self.last_character_name = state.character_name;
        self.last_clarification = state.clarification;
        self.last_page_idx = last.img_idx as i64;
        self.last_bubble_order = state.bubble_order;
    }

    fn apply_defaults_for_new_bubble(&mut self, bubble: &Bubble, auto_insert_last_character: bool) {
        let mut state = bubble_footer_state_from_record(bubble);
        let mut patch = Map::new();
        if !bubble.extra.contains_key("translation_status") {
            patch.insert(
                "translation_status".to_string(),
                Value::String("untranslated".to_string()),
            );
        }
        if auto_insert_last_character && !bubble.extra.contains_key("is_known_character") {
            state.is_known_character = self.last_is_known_character;
            patch.insert(
                "is_known_character".to_string(),
                Value::Bool(self.last_is_known_character),
            );
        }
        if auto_insert_last_character && !bubble.extra.contains_key("character_name") {
            state.character_name = self.last_character_name.clone();
            patch.insert(
                "character_name".to_string(),
                Value::String(state.character_name.clone()),
            );
        }
        if auto_insert_last_character && !bubble.extra.contains_key("clarification") {
            state.clarification = self.last_clarification.clone();
            patch.insert(
                "clarification".to_string(),
                Value::String(state.clarification.clone()),
            );
        }
        if !bubble.extra.contains_key("bubble_order") {
            let order =
                if self.last_page_idx == bubble.img_idx as i64 && self.last_bubble_order >= 0 {
                    self.last_bubble_order.saturating_add(1)
                } else {
                    0
                };
            state.bubble_order = order;
            patch.insert("bubble_order".to_string(), Value::Number(order.into()));
        }
        if !patch.is_empty() {
            self.pending_footer_patches.insert(bubble.id, patch);
            self.footer_overrides.insert(bubble.id, state.clone());
        }

        if auto_insert_last_character
            || bubble.extra.contains_key("is_known_character")
            || bubble.extra.contains_key("character_name")
            || bubble.extra.contains_key("clarification")
        {
            self.last_is_known_character = state.is_known_character;
            self.last_character_name = state.character_name.clone();
            self.last_clarification = state.clarification.clone();
        }
        self.last_page_idx = bubble.img_idx as i64;
        self.last_bubble_order = state.bubble_order;
    }

    fn recent_character_entry_for_rank(&self, rank: usize) -> Option<&RecentCharacterEntry> {
        rank.checked_sub(1)
            .and_then(|idx| self.recent_characters.get(idx))
    }

    fn latest_bubble_id(&self, canvas: &CanvasView, project: &ProjectData) -> Option<i64> {
        canvas
            .hook_bubbles_snapshot(project)
            .into_iter()
            .max_by_key(|bubble| bubble.id)
            .map(|bubble| bubble.id)
    }

    fn apply_recent_character_rank_to_bubble(
        &mut self,
        canvas: &mut CanvasView,
        project: &ProjectData,
        bubble_id: i64,
        rank: usize,
    ) -> bool {
        let Some(entry) = self.recent_character_entry_for_rank(rank).cloned() else {
            return false;
        };
        let mut patch = Map::new();
        patch.insert(
            "is_known_character".to_string(),
            Value::Bool(entry.is_known_character),
        );
        patch.insert(
            "character_name".to_string(),
            Value::String(entry.character_name.clone()),
        );
        patch.insert(
            "clarification".to_string(),
            Value::String(entry.clarification.clone()),
        );
        if !canvas.patch_bubble_extra_fields(project, bubble_id, &patch) {
            return false;
        }
        self.footer_overrides.insert(
            bubble_id,
            BubbleFooterState {
                bubble_order: self.last_bubble_order.max(0),
                is_known_character: entry.is_known_character,
                character_name: entry.character_name.clone(),
                clarification: entry.clarification.clone(),
            },
        );
        self.last_is_known_character = entry.is_known_character;
        self.last_character_name = entry.character_name;
        self.last_clarification = entry.clarification;
        true
    }

    fn apply_recent_character_rank_to_latest_bubble(
        &mut self,
        canvas: &mut CanvasView,
        project: &ProjectData,
        rank: usize,
    ) -> bool {
        let Some(bubble_id) = self.latest_bubble_id(canvas, project) else {
            return false;
        };
        self.apply_recent_character_rank_to_bubble(canvas, project, bubble_id, rank)
    }

    pub fn create_bubble_at_pointer_shortcut(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        pointer_pos: Pos2,
    ) -> bool {
        let recent_rank = recent_character_rank_from_input(ctx);
        if !canvas.create_bubble_at_pointer_shortcut(pointer_pos) {
            return false;
        }
        if let Some(rank) = recent_rank {
            let _ = self.apply_recent_character_rank_to_latest_bubble(canvas, project, rank);
        }
        true
    }

    fn handle_image_bubble_hotkeys(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        let (q_down, any_modifier) = ctx.input(|input| {
            (
                input.key_down(egui::Key::Q),
                input.modifiers.shift
                    || input.modifiers.ctrl
                    || input.modifiers.command
                    || input.modifiers.alt,
            )
        });
        // Re-arm only once Q is fully released, so a Q still held after a Shift+Q crop (Shift may
        // release a frame earlier) cannot spawn another bubble.
        if !q_down {
            self.image_create_q_armed = true;
        }
        let crop_context = Self::image_crop_selection_mode_active(ctx)
            || self.image_crop_selection.is_some()
            || any_modifier;
        // Mark this Q-hold as belonging to a crop/modifier context; it must not create a plain
        // bubble until released and pressed again.
        if q_down && crop_context {
            self.image_create_q_armed = false;
        }

        // Always consume the plain-Q press event so it does not leak elsewhere.
        let q_pressed_event =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Q));
        let create = q_pressed_event
            && self.image_create_q_armed
            && q_down
            && !any_modifier
            && !crop_context
            && !ctx.wants_keyboard_input();
        if create {
            // One bubble per fresh press: block until Q is released and pressed again.
            self.image_create_q_armed = false;
            if let Some(pointer_pos) = ctx.pointer_latest_pos()
                && canvas.create_image_bubble_at_pointer_shortcut(ctx, pointer_pos)
            {
                canvas.flush_pending_bubble_upserts_now(project);
            }
        }
    }

    fn draw_recent_character_cards(&self, ctx: &egui::Context, canvas_rect: Rect) {
        if self.recent_characters.is_empty() {
            return;
        }
        let left = (canvas_rect.left() + RECENT_CHARACTER_CARDS_LEFT_MARGIN)
            .min(canvas_rect.right() - RECENT_CHARACTER_CARDS_RIGHT_MARGIN);
        let available_width =
            (canvas_rect.right() - left - RECENT_CHARACTER_CARDS_RIGHT_MARGIN).max(120.0);
        egui::Area::new("translation_recent_character_cards".into())
            .order(egui::Order::Foreground)
            .interactable(false)
            .fixed_pos(egui::pos2(
                left,
                canvas_rect.top() + RECENT_CHARACTER_CARDS_TOP_MARGIN,
            ))
            .show(ctx, |ui| {
                ui.set_max_width(available_width);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                    for (idx, entry) in self.recent_characters.iter().enumerate() {
                        egui::Frame::new()
                            .fill(Color32::from_rgba_unmultiplied(110, 110, 110, 170))
                            .corner_radius(egui::CornerRadius::same(8))
                            .stroke(Stroke::new(
                                1.0,
                                Color32::from_rgba_unmultiplied(255, 255, 255, 44),
                            ))
                            .inner_margin(egui::Margin::symmetric(10, 6))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "[ {} ] {}",
                                        idx + 1,
                                        entry.character_name
                                    ))
                                    .color(Color32::from_rgb(235, 235, 235)),
                                );
                            });
                    }
                });
            });
    }

    fn queue_footer_patch(&mut self, bubble_id: i64, field: &str, value: Value, now_s: f64) {
        self.pending_footer_patches
            .entry(bubble_id)
            .or_default()
            .insert(field.to_string(), value);
        self.pending_footer_patch_changed_at
            .insert(bubble_id, now_s);
    }

    fn flush_footer_patches(&mut self, canvas: &mut CanvasView, project: &ProjectData, now_s: f64) {
        if self.pending_footer_patches.is_empty() {
            return;
        }
        let mut done = Vec::new();
        for (bubble_id, patch) in &self.pending_footer_patches {
            let changed_at = self
                .pending_footer_patch_changed_at
                .get(bubble_id)
                .copied()
                .unwrap_or(0.0);
            if now_s - changed_at < FOOTER_PATCH_DEBOUNCE_SECS {
                continue;
            }
            if canvas.patch_bubble_extra_fields(project, *bubble_id, patch) {
                done.push(*bubble_id);
            }
        }
        for bubble_id in done {
            self.pending_footer_patches.remove(&bubble_id);
            self.pending_footer_patch_changed_at.remove(&bubble_id);
        }
        self.footer_overrides
            .retain(|bid, _| self.pending_footer_patches.contains_key(bid));
    }

    fn footer_state_for(&self, bubble: &Bubble) -> BubbleFooterState {
        footer_state_for_bubble(&self.footer_overrides, bubble)
    }

    fn bubble_has_character_for_status(&self, bubble: &Bubble) -> bool {
        let character_name = self.footer_state_for(bubble).character_name;
        let trimmed = character_name.trim();
        !trimmed.is_empty() && trimmed != FOOTER_NO_CHARACTER && trimmed != FOOTER_NO_CHARACTERS
    }

    fn poll_ocr_events(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        for event in self.ocr_controller.poll_events() {
            match event {
                OcrControllerEvent::StateChanged(state) => match state {
                    OcrLoadState::DownloadingModel => {
                        let engine = self
                            .ocr_loading_engine
                            .unwrap_or(self.ocr_panel_options.engine);
                        self.mark_ocr_model_download_started_for_engine(engine);
                        self.push_toast(
                            ctx,
                            "Скачивание модели...".to_string(),
                            Color32::GOLD,
                            2.2,
                        );
                    }
                    OcrLoadState::Loading => {
                        let engine = self
                            .ocr_loading_engine
                            .unwrap_or(self.ocr_panel_options.engine);
                        self.mark_ocr_load_requested_for_engine(engine);
                        self.push_toast(
                            ctx,
                            "Движок загружается...".to_string(),
                            Color32::GOLD,
                            2.2,
                        );
                    }
                    OcrLoadState::Error => {
                        let engine = self
                            .ocr_loading_engine
                            .take()
                            .unwrap_or(self.ocr_panel_options.engine);
                        self.set_ocr_state_for_engine(engine, OcrLoadState::Error);
                        self.push_toast(
                            ctx,
                            "Ошибка загрузки движка распознавания.".to_string(),
                            Color32::RED,
                            3.0,
                        );
                    }
                    OcrLoadState::Ready => {
                        let engine = self
                            .ocr_loading_engine
                            .take()
                            .unwrap_or(self.ocr_panel_options.engine);
                        self.set_ocr_state_for_engine(engine, OcrLoadState::Ready);
                    }
                    OcrLoadState::NotLoaded => {
                        self.ocr_loading_engine = None;
                    }
                },
                OcrControllerEvent::Recognized { request_id, result } => {
                    let manual_target = if self.manual_ocr_active_request_id == Some(request_id) {
                        self.manual_ocr_active_request_id = None;
                        self.manual_ocr_result_target.take()
                    } else {
                        None
                    };
                    let quick_manual =
                        manual_target == Some(ManualOcrResultTarget::ToastAndClipboard);
                    let manual_text =
                        build_bubble_original_text(&result, self.ocr_panel_options.join_newlines);
                    let adv_rec_applied = match manual_target {
                        Some(ManualOcrResultTarget::ToastAndClipboard) => {
                            self.advanced_recognition.apply_quick_recognition_result(
                                request_id,
                                manual_text.clone(),
                                "Распознавание выделения завершено.".to_string(),
                            )
                        }
                        _ => self
                            .advanced_recognition
                            .apply_recognition_result(request_id, manual_text.clone()),
                    };
                    if !quick_manual
                        && self.ocr_panel_options.copy_to_clipboard
                        && !result.text.trim().is_empty()
                    {
                        ctx.copy_text(result.text.clone());
                    }
                    if adv_rec_applied
                        && !quick_manual
                        && self.ocr_panel_options.copy_to_clipboard
                        && !manual_text.trim().is_empty()
                    {
                        ctx.copy_text(manual_text.clone());
                    }
                    if manual_target == Some(ManualOcrResultTarget::ToastAndClipboard) {
                        if !manual_text.trim().is_empty() {
                            ctx.copy_text(manual_text.clone());
                        }
                        let quick_msg = if manual_text.trim().is_empty() {
                            "Быстрое OCR: текст не найден".to_string()
                        } else {
                            format!("Быстрое OCR: {}", manual_text.trim())
                        };
                        self.push_toast(ctx, quick_msg, Color32::from_rgb(42, 168, 88), 3.0);
                    }
                    if let Some(pending_insert) = self.pending_bubble_inserts.remove(&request_id) {
                        let original_text =
                            build_bubble_original_text(&result, pending_insert.join_newlines);
                        if !original_text.trim().is_empty()
                            && canvas.create_bubble_with_original_text_at_page_uv_rect(
                                pending_insert.page_idx,
                                pending_insert.uv_rect,
                                original_text,
                            )
                        {
                            if let Some(rank) = pending_insert.recent_character_rank {
                                let _ = self.apply_recent_character_rank_to_latest_bubble(
                                    canvas, project, rank,
                                );
                            }
                            canvas.flush_pending_bubble_upserts_now(project);
                        }
                    }
                    if self.textdetector_ocr_active_request_id == Some(request_id) {
                        self.textdetector_ocr_active_request_id = None;
                        self.textdetector_ocr_active_task = None;
                        self.textdetector_ocr_retry_state = None;
                        self.textdetector_ocr_done = self.textdetector_ocr_done.saturating_add(1);
                        if !result.text.trim().is_empty() {
                            self.textdetector_ocr_recognized =
                                self.textdetector_ocr_recognized.saturating_add(1);
                        }
                        self.set_text_detector_status(
                            "Распознавание блоков...",
                            TEXT_DETECTOR_STATUS_WARN,
                        );
                    }
                    let msg = if result.lines.is_empty() {
                        "Распознавание: текст не найден".to_string()
                    } else {
                        format!("Распознавание: {} строк", result.lines.len())
                    };
                    self.push_toast(ctx, msg, Color32::from_rgb(42, 168, 88), 2.2);
                }
                OcrControllerEvent::RecognizeFailed { request_id, error } => {
                    let manual_target = if self.manual_ocr_active_request_id == Some(request_id) {
                        self.manual_ocr_active_request_id = None;
                        self.manual_ocr_result_target.take()
                    } else {
                        None
                    };
                    self.pending_bubble_inserts.remove(&request_id);
                    let adv_rec_error_applied = self
                        .advanced_recognition
                        .apply_recognition_error(request_id, error.clone());
                    if self.textdetector_ocr_active_request_id == Some(request_id) {
                        self.textdetector_ocr_active_request_id = None;
                        if let Some(task) = self.textdetector_ocr_active_task.take()
                            && task.retry_attempt == 0
                        {
                            let now_s = ctx.input(|i| i.time);
                            self.textdetector_ocr_retry_state = Some(TextDetectorOcrRetryState {
                                task: TextDetectorOcrTask {
                                    retry_attempt: 1,
                                    ..task
                                },
                                retry_at_s: now_s + TEXT_DETECTOR_OCR_RETRY_DELAY_SECS,
                            });
                            self.set_text_detector_status(
                                format!(
                                    "Ошибка распознавания, повтор блока через {:.0} c...",
                                    TEXT_DETECTOR_OCR_RETRY_DELAY_SECS
                                ),
                                TEXT_DETECTOR_STATUS_WARN,
                            );
                            self.push_toast(
                                ctx,
                                "Ошибка распознавания: повтор через несколько секунд".to_string(),
                                Color32::from_rgb(255, 172, 66),
                                2.8,
                            );
                            continue;
                        }
                        self.abort_textdetector_ocr(
                            ctx,
                            format!("Распознавание остановлено после повторной ошибки: {error}"),
                        );
                    }
                    if manual_target == Some(ManualOcrResultTarget::ToastAndClipboard) {
                        self.push_toast(ctx, format!("Быстрое OCR: {error}"), Color32::RED, 3.0);
                    } else if !adv_rec_error_applied {
                        self.push_toast(
                            ctx,
                            format!("ошибка распознавания: {error}"),
                            Color32::RED,
                            3.0,
                        );
                    }
                }
                OcrControllerEvent::AiApiKeyStored { service } => {
                    if self.ocr_panel_options.ai_api_service == service {
                        self.ocr_panel_options.ai_api_key_edit.clear();
                        self.ocr_panel_options.ai_api_key_configured = Some(true);
                        self.ocr_panel_options.ai_api_status =
                            format!("API key {} сохранен.", service.label());
                        self.ocr_controller.refresh_ai_api_metadata(service);
                    }
                    self.push_toast(
                        ctx,
                        format!("API key {} сохранен.", service.label()),
                        Color32::from_rgb(42, 168, 88),
                        2.2,
                    );
                }
                OcrControllerEvent::AiApiKeyCleared { service } => {
                    if self.ocr_panel_options.ai_api_service == service {
                        self.ocr_panel_options.ai_api_key_edit.clear();
                        self.ocr_panel_options.ai_api_key_configured = Some(false);
                        self.ocr_panel_options.ai_api_models.clear();
                        self.ocr_panel_options.ai_api_account_status =
                            "API key не задан".to_string();
                        self.ocr_panel_options.ai_api_status =
                            format!("API key {} удален.", service.label());
                    }
                }
                OcrControllerEvent::AiApiMetadataLoaded(metadata) => {
                    if self.ocr_panel_options.ai_api_service == metadata.service {
                        self.ocr_panel_options.ai_api_key_configured =
                            Some(metadata.key_configured);
                        self.ocr_panel_options.ai_api_models = metadata.models;
                        self.ocr_panel_options.ai_api_account_status = metadata.account_status;
                        if !self
                            .ocr_panel_options
                            .ai_api_models
                            .iter()
                            .any(|model| model == &self.ocr_panel_options.ai_api_model)
                            && let Some(model) = self.ocr_panel_options.ai_api_models.first()
                        {
                            self.ocr_panel_options.ai_api_model = model.clone();
                            self.ocr_settings_dirty = true;
                        }
                        self.ocr_panel_options.ai_api_status =
                            "AI API данные обновлены.".to_string();
                    }
                }
                OcrControllerEvent::AiApiMetadataFailed { service, error } => {
                    if self.ocr_panel_options.ai_api_service == service {
                        self.ocr_panel_options.ai_api_status = error.clone();
                    }
                    self.push_toast(ctx, format!("AI API: {error}"), Color32::RED, 3.0);
                }
            }
        }
    }

    fn poll_text_detector_events(&mut self, ctx: &egui::Context) {
        for event in self.text_detector_controller.poll_events() {
            match event {
                TextDetectorControllerEvent::ModelDownloadStarted => {
                    self.set_text_detector_status(
                        "Скачивание модели...",
                        TEXT_DETECTOR_STATUS_WARN,
                    );
                }
                TextDetectorControllerEvent::DetectStarted { total, replace } => {
                    if replace {
                        self.text_detector_results.clear();
                        self.text_detector_mask_textures.clear();
                        self.clear_text_mask_model();
                    }
                    self.text_detector_progress = Some((0, total));
                    self.set_text_detector_status(
                        self.text_detector_running_status(),
                        TEXT_DETECTOR_STATUS_WARN,
                    );
                }
                TextDetectorControllerEvent::PageDetected { page_idx, result } => {
                    self.text_detector_results.insert(page_idx, result);
                    self.text_detector_mask_textures.remove(&page_idx);
                    self.sync_text_mask_page_from_result(page_idx);
                }
                TextDetectorControllerEvent::PageFailed { page_idx, error } => {
                    self.push_toast(
                        ctx,
                        format!("Детектор: страница #{page_idx} пропущена ({error})"),
                        Color32::from_rgb(255, 172, 66),
                        2.8,
                    );
                }
                TextDetectorControllerEvent::DetectProgress { done, total } => {
                    self.text_detector_progress = Some((done, total));
                }
                TextDetectorControllerEvent::DetectFinished {
                    total_blocks,
                    failed_pages,
                } => {
                    self.text_detector_progress = None;
                    let status = if failed_pages == 0 {
                        format!("Готово. Найдено блоков: {total_blocks}")
                    } else {
                        format!("Готово. Блоков: {total_blocks}, страниц с ошибкой: {failed_pages}")
                    };
                    let color = if failed_pages == 0 {
                        TEXT_DETECTOR_STATUS_OK
                    } else {
                        TEXT_DETECTOR_STATUS_WARN
                    };
                    self.set_text_detector_status(status, color);
                }
                TextDetectorControllerEvent::DetectFailed { error } => {
                    self.text_detector_progress = None;
                    self.set_text_detector_status(
                        format!("Ошибка поиска текста: {error}"),
                        TEXT_DETECTOR_STATUS_ERR,
                    );
                }
            }
        }
    }

    fn ensure_text_detection_storage_loaded(&mut self, project: &ProjectData) {
        let project_dir = project.paths.project_dir.clone();
        if self
            .text_detection_storage_loaded_for
            .as_ref()
            .is_some_and(|loaded| *loaded == project_dir)
        {
            return;
        }
        if self.text_detection_storage_busy {
            return;
        }
        self.start_text_detection_storage_load(project);
    }

    fn start_text_detection_storage_load(&mut self, project: &ProjectData) {
        if self.text_detection_storage_busy {
            return;
        }
        let project_dir = project.paths.project_dir.clone();
        let storage_dir = project.paths.text_detection_dir.clone();
        let page_indices = project
            .pages
            .iter()
            .map(|page| page.idx)
            .collect::<Vec<_>>();
        let (tx, rx) = mpsc::channel::<TextDetectionStorageEvent>();
        self.text_detection_storage_busy = true;
        self.text_detection_storage_rx = Some(rx);
        self.set_text_detector_status("Загрузка сохранённой маски...", TEXT_DETECTOR_STATUS_WARN);

        thread::spawn(move || {
            let event = match load_text_detection_storage(&storage_dir, &page_indices) {
                Ok((pages, loaded, failed)) => TextDetectionStorageEvent::Loaded {
                    project_dir,
                    pages,
                    loaded,
                    failed,
                },
                Err(error) => TextDetectionStorageEvent::Failed { project_dir, error },
            };
            let _ = tx.send(event);
        });
    }

    fn start_text_detection_storage_save(&mut self, project: &ProjectData) {
        if self.text_detection_storage_busy {
            return;
        }
        if self.text_detector_results.is_empty() {
            self.set_text_detector_status(
                "Нет результатов для сохранения",
                TEXT_DETECTOR_STATUS_ERR,
            );
            return;
        }
        let project_dir = project.paths.project_dir.clone();
        let storage_dir = project.paths.text_detection_dir.clone();
        let pages = self
            .text_detector_results
            .iter()
            .map(|(page_idx, result)| (*page_idx, result.clone()))
            .collect::<Vec<_>>();
        let (tx, rx) = mpsc::channel::<TextDetectionStorageEvent>();
        self.text_detection_storage_busy = true;
        self.text_detection_storage_rx = Some(rx);
        self.set_text_detector_status("Сохранение маски...", TEXT_DETECTOR_STATUS_WARN);

        thread::spawn(move || {
            let event = match save_text_detection_storage(&storage_dir, &pages) {
                Ok((saved, failed)) => TextDetectionStorageEvent::Saved {
                    project_dir,
                    saved,
                    failed,
                },
                Err(error) => TextDetectionStorageEvent::Failed { project_dir, error },
            };
            let _ = tx.send(event);
        });
    }

    fn poll_text_detection_storage_events(&mut self, project: &ProjectData) {
        let Some(rx) = self.text_detection_storage_rx.as_ref() else {
            return;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.text_detection_storage_rx = None;
                self.text_detection_storage_busy = false;
                self.set_text_detector_status(
                    "Операция text_detection прервана",
                    TEXT_DETECTOR_STATUS_ERR,
                );
                return;
            }
        };
        self.text_detection_storage_rx = None;
        self.text_detection_storage_busy = false;

        let current_project_dir = project.paths.project_dir.clone();
        match event {
            TextDetectionStorageEvent::Loaded {
                project_dir,
                pages,
                loaded,
                failed,
            } => {
                if project_dir != current_project_dir {
                    return;
                }
                for (page_idx, result) in pages {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        self.text_detector_results.entry(page_idx)
                    {
                        entry.insert(result);
                        self.text_detector_mask_textures.remove(&page_idx);
                        self.sync_text_mask_page_from_result(page_idx);
                    }
                }
                self.text_detection_storage_loaded_for = Some(project_dir);
                let status = if failed == 0 {
                    format!("Загружено сохранённых страниц: {loaded}")
                } else {
                    format!("Загрузка: {loaded}, с ошибками: {failed}")
                };
                let color = if failed == 0 {
                    TEXT_DETECTOR_STATUS_OK
                } else {
                    TEXT_DETECTOR_STATUS_WARN
                };
                self.set_text_detector_status(status, color);
            }
            TextDetectionStorageEvent::Saved {
                project_dir,
                saved,
                failed,
            } => {
                if project_dir != current_project_dir {
                    return;
                }
                self.text_detection_storage_loaded_for = Some(project_dir);
                let status = if failed == 0 {
                    format!("Сохранено страниц: {saved}")
                } else {
                    format!("Сохранено: {saved}, с ошибками: {failed}")
                };
                let color = if failed == 0 {
                    TEXT_DETECTOR_STATUS_OK
                } else {
                    TEXT_DETECTOR_STATUS_WARN
                };
                self.set_text_detector_status(status, color);
            }
            TextDetectionStorageEvent::Failed { project_dir, error } => {
                if project_dir != current_project_dir {
                    return;
                }
                self.text_detection_storage_loaded_for = Some(project_dir);
                self.set_text_detector_status(
                    format!("Ошибка text_detection: {error}"),
                    TEXT_DETECTOR_STATUS_ERR,
                );
            }
        }
    }

    fn set_text_detector_status(&mut self, text: impl Into<String>, color: Color32) {
        self.text_detector_status = text.into();
        self.text_detector_status_color = color;
    }

    fn clear_text_detector_line_edit_state(&mut self) {
        self.text_detector_line_selection = None;
        self.text_detector_line_drag_state = None;
    }

    fn set_text_detector_edit_lines_mode(&mut self, enabled: bool) {
        self.text_detector_edit_lines_mode = enabled;
        if enabled {
            self.set_text_detector_edit_mask_mode(false);
            return;
        }
        self.clear_text_detector_line_edit_state();
    }

    fn set_text_detector_edit_mask_mode(&mut self, enabled: bool) {
        self.text_detector_edit_mask_mode = enabled;
        if enabled {
            self.text_detector_edit_lines_mode = false;
            self.clear_text_detector_line_edit_state();
            return;
        }
        self.text_detector_mask_stroke_state = None;
    }

    fn clear_text_mask_model(&mut self) {
        if let Some(model) = self.text_mask_model.as_ref()
            && let Ok(mut model) = model.lock()
        {
            model.clear_all();
        }
        self.text_mask_synced_revision = 0;
    }

    fn sync_text_mask_page_from_result(&mut self, page_idx: usize) {
        let Some(result) = self.text_detector_results.get(&page_idx) else {
            if let Some(model) = self.text_mask_model.as_ref()
                && let Ok(mut model) = model.lock()
            {
                model.remove_page(page_idx);
            }
            return;
        };
        if let Some(model) = self.text_mask_model.as_ref()
            && let Ok(mut model) = model.lock()
        {
            model.set_page(
                page_idx,
                result.source_size,
                result.mask_size,
                result.mask_alpha.clone(),
            );
        }
    }

    fn sync_text_mask_revision_cache(&mut self) {
        let Some(model) = self.text_mask_model.as_ref() else {
            return;
        };
        let revision = model.lock().map(|m| m.revision()).unwrap_or(0);
        if revision != self.text_mask_synced_revision {
            self.text_mask_synced_revision = revision;
            self.text_detector_mask_textures.clear();
        }
    }

    fn text_mask_page_snapshot(&self, page_idx: usize) -> Option<TextMaskPage> {
        let model = self.text_mask_model.as_ref()?;
        let guard = model.lock().ok()?;
        guard.page(page_idx).cloned()
    }

    fn materialize_text_mask_page_from_blocks_if_missing(&mut self, page_idx: usize) {
        if self
            .text_mask_page_snapshot(page_idx)
            .is_some_and(|page| !page.mask_alpha.is_empty())
        {
            return;
        }
        let Some(result) = self.text_detector_results.get(&page_idx) else {
            return;
        };
        if !result.mask_alpha.is_empty() || result.blocks.is_empty() {
            return;
        }

        let mask_size = result.source_size;
        let mask_w = usize::try_from(mask_size[0]).ok().unwrap_or(0);
        let mask_h = usize::try_from(mask_size[1]).ok().unwrap_or(0);
        if mask_w == 0 || mask_h == 0 {
            return;
        }
        let mut mask_alpha = vec![0u8; mask_w.saturating_mul(mask_h)];
        for rect in detector_blocks_with_options(result, &self.text_detector_options) {
            let x0 = rect.x1.floor().max(0.0) as usize;
            let y0 = rect.y1.floor().max(0.0) as usize;
            let x1 = rect.x2.ceil().min(mask_size[0] as f32) as usize;
            let y1 = rect.y2.ceil().min(mask_size[1] as f32) as usize;
            if x0 >= x1 || y0 >= y1 {
                continue;
            }
            for y in y0..y1 {
                let row = y.saturating_mul(mask_w);
                for x in x0..x1 {
                    mask_alpha[row + x] = 255;
                }
            }
        }
        if !mask_alpha.iter().any(|&px| px != 0) {
            return;
        }
        if let Some(model) = self.text_mask_model.as_ref()
            && let Ok(mut model) = model.lock()
        {
            model.set_page(page_idx, result.source_size, mask_size, mask_alpha);
        }
    }

    fn text_mask_target_sizes(
        &self,
        page_idx: usize,
        page_rect: Rect,
        zoom: f32,
    ) -> Option<([u32; 2], [u32; 2])> {
        if let Some(mask) = self.text_mask_page_snapshot(page_idx) {
            return Some((mask.source_size, mask.mask_size));
        }
        if let Some(result) = self.text_detector_results.get(&page_idx) {
            let source_size = result.source_size;
            let mask_size = if result.mask_alpha.is_empty() {
                source_size
            } else {
                result.mask_size
            };
            return Some((source_size, mask_size));
        }
        let safe_zoom = zoom.max(f32::EPSILON);
        let source_w = (page_rect.width() / safe_zoom).round().max(1.0) as u32;
        let source_h = (page_rect.height() / safe_zoom).round().max(1.0) as u32;
        if source_w == 0 || source_h == 0 {
            return None;
        }
        Some(([source_w, source_h], [source_w, source_h]))
    }

    fn text_detector_handle_mask_brush_wheel(&mut self, ui: &mut egui::Ui, hovered: bool) -> bool {
        if !hovered {
            return false;
        }
        let (mods, smooth_scroll, raw_scroll, primary_down) = ui.ctx().input(|input| {
            (
                input.modifiers,
                input.smooth_scroll_delta,
                input.raw_scroll_delta,
                input.pointer.primary_down(),
            )
        });
        if primary_down {
            return false;
        }
        let mut wheel_delta = smooth_scroll.y;
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = raw_scroll.y;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = smooth_scroll.x;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = raw_scroll.x;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            return false;
        }
        let changed = self
            .text_detector_mask_brush
            .handle_wheel(wheel_delta, mods);
        if !mods.shift {
            return false;
        }
        ui.ctx().input_mut(|input| {
            input.smooth_scroll_delta = egui::Vec2::ZERO;
            input.raw_scroll_delta = egui::Vec2::ZERO;
        });
        if changed {
            ui.ctx().request_repaint();
        }
        changed
    }

    fn text_detector_handle_mask_brush_hotkeys(
        &mut self,
        ui: &mut egui::Ui,
        hovered: bool,
    ) -> bool {
        if !hovered || ui.ctx().wants_keyboard_input() {
            return false;
        }
        let changed = self
            .text_detector_mask_brush
            .handle_size_shortcuts(ui.ctx());
        if changed {
            ui.ctx().request_repaint();
        }
        changed
    }

    // All parameters are distinct pixel-buffer or layout properties; grouping would obscure rendering intent.
    #[allow(clippy::too_many_arguments)]
    fn paint_text_mask_segment(
        &mut self,
        page_idx: usize,
        source_size: [u32; 2],
        mask_size: [u32; 2],
        page_rect: Rect,
        from_scene: Pos2,
        to_scene: Pos2,
        erase: bool,
    ) -> bool {
        let Some(src0) = scene_pos_to_source(
            page_rect,
            mask_size[0] as f32,
            mask_size[1] as f32,
            from_scene,
        ) else {
            return false;
        };
        let Some(src1) = scene_pos_to_source(
            page_rect,
            mask_size[0] as f32,
            mask_size[1] as f32,
            to_scene,
        ) else {
            return false;
        };
        let Some(model) = self.text_mask_model.as_ref().cloned() else {
            return false;
        };
        let brush = self.text_detector_mask_brush.clone();
        let brush_radius = brush.radius_px().max(1) as f32;
        let target_value = if erase { 0u8 } else { 255u8 };
        let mut dirty_rect: Option<(usize, usize, usize, usize)> = None;
        let mask_w = mask_size[0] as usize;
        let mask_h = mask_size[1] as usize;
        let mut changed = false;
        if let Ok(mut model) = model.lock() {
            changed = model.edit_page_mask(
                page_idx,
                source_size,
                mask_size,
                |mask_alpha, mask_w, mask_h| {
                    if mask_w == 0 || mask_h == 0 {
                        return false;
                    }
                    let min_x = (src0.x.min(src1.x) - brush_radius).floor().max(0.0) as usize;
                    let min_y = (src0.y.min(src1.y) - brush_radius).floor().max(0.0) as usize;
                    let max_x = (src0.x.max(src1.x) + brush_radius)
                        .ceil()
                        .min(mask_w.saturating_sub(1) as f32)
                        as usize;
                    let max_y = (src0.y.max(src1.y) + brush_radius)
                        .ceil()
                        .min(mask_h.saturating_sub(1) as f32)
                        as usize;
                    let mut has_delta = false;
                    for y in min_y..=max_y {
                        let row = y.saturating_mul(mask_w);
                        for x in min_x..=max_x {
                            if mask_alpha[row + x] != target_value {
                                has_delta = true;
                                break;
                            }
                        }
                        if has_delta {
                            break;
                        }
                    }
                    if !has_delta {
                        return false;
                    }
                    dirty_rect = Some((min_x, min_y, max_x, max_y));
                    brush.paint_binary_mask_segment(
                        mask_alpha,
                        mask_w,
                        mask_h,
                        src0.x.round() as i32,
                        src0.y.round() as i32,
                        src1.x.round() as i32,
                        src1.y.round() as i32,
                        erase,
                    );
                    true
                },
            );
            if changed {
                self.text_mask_synced_revision = model.revision();
                if let (Some(page), Some(dirty)) = (model.page(page_idx), dirty_rect)
                    && let Some(page_tex) = self.text_detector_mask_textures.get_mut(&page_idx)
                {
                    if page_tex.size == [mask_w, mask_h] {
                        update_text_detector_mask_texture_tiles(
                            page_tex,
                            page.mask_size.map(|v| v as usize),
                            &page.mask_alpha,
                            dirty,
                        );
                    } else {
                        self.text_detector_mask_textures.remove(&page_idx);
                    }
                }
            }
        }
        changed
    }

    fn has_detected_blocks_on_page(&self, page_idx: usize) -> bool {
        !self.collect_detected_block_rects_px(page_idx).is_empty()
    }

    fn has_detected_blocks_any(&self) -> bool {
        self.text_detector_results
            .keys()
            .copied()
            .any(|idx| self.has_detected_blocks_on_page(idx))
    }

    fn detected_page_indices(&self) -> Vec<usize> {
        let mut out = self
            .text_detector_results
            .keys()
            .copied()
            .filter(|idx| self.has_detected_blocks_on_page(*idx))
            .collect::<Vec<_>>();
        out.sort_unstable();
        out
    }

    fn collect_detected_block_rects_px(&self, page_idx: usize) -> Vec<TextDetectorRect> {
        let Some(result) = self.text_detector_results.get(&page_idx) else {
            return Vec::new();
        };
        detector_blocks_with_options(result, &self.text_detector_options)
    }

    fn textdetector_ocr_is_running(&self) -> bool {
        self.textdetector_ocr_active_request_id.is_some()
            || self.textdetector_ocr_retry_state.is_some()
            || !self.pending_textdetector_ocr_tasks.is_empty()
            || (self.textdetector_ocr_total > 0
                && self.textdetector_ocr_done < self.textdetector_ocr_total)
    }

    fn text_detector_run_mode(&self) -> Result<TextDetectorRunMode, String> {
        match self.text_detector_options.algorithm {
            TextDetectorAlgorithm::Classic => Ok(TextDetectorRunMode::Classic),
            TextDetectorAlgorithm::PaddleOcr => {
                if !self.ai_enabled {
                    return Err("PaddleOCR-детектор отключён флагом --no-ai.".to_string());
                }
                Ok(TextDetectorRunMode::PaddleOcr(
                    TextDetectorPaddleOcrOptions::default(),
                ))
            }
            TextDetectorAlgorithm::Ai => {
                if !self.ai_enabled {
                    return Err("ИИ-детектор отключён флагом --no-ai.".to_string());
                }
                if matches!(self.ai_backend_torch_available(), Some(false)) {
                    return Err("PyTorch не установлен".to_string());
                }
                Ok(TextDetectorRunMode::AiCtd(TextDetectorAiCtdOptions {
                    detect_size: self.text_detector_options.ai_detect_size,
                    det_rearrange_max_batches: self
                        .text_detector_options
                        .ai_det_rearrange_max_batches,
                    font_size_multiplier: self.text_detector_options.ai_font_size_multiplier,
                    font_size_max: self.text_detector_options.ai_font_size_max,
                    font_size_min: self.text_detector_options.ai_font_size_min,
                    mask_dilate_size: 0,
                }))
            }
            TextDetectorAlgorithm::Surya => {
                if !self.ai_enabled {
                    return Err("Surya-детектор отключён флагом --no-ai.".to_string());
                }
                if matches!(self.ai_backend_torch_available(), Some(false)) {
                    return Err("PyTorch не установлен".to_string());
                }
                Ok(TextDetectorRunMode::Surya(TextDetectorSuryaOptions))
            }
        }
    }

    fn text_detector_running_status(&self) -> &'static str {
        match self.text_detector_options.algorithm {
            TextDetectorAlgorithm::Classic => "Поиск текста...",
            TextDetectorAlgorithm::PaddleOcr => "Поиск текста (PaddleOCR)...",
            TextDetectorAlgorithm::Ai => "Поиск текста (ИИ)...",
            TextDetectorAlgorithm::Surya => "Поиск текста (Surya)...",
        }
    }

    fn start_text_detection_for_current_page(
        &mut self,
        project: &ProjectData,
        canvas: &CanvasView,
    ) {
        if self.text_detector_controller.is_busy() {
            return;
        }
        let page_idx = canvas.current_page_idx();
        let Some(page) = project.pages.iter().find(|page| page.idx == page_idx) else {
            self.set_text_detector_status("Текущая страница не найдена", TEXT_DETECTOR_STATUS_ERR);
            return;
        };
        let mode = match self.text_detector_run_mode() {
            Ok(mode) => mode,
            Err(error) => {
                self.set_text_detector_status(error, TEXT_DETECTOR_STATUS_ERR);
                return;
            }
        };
        match self.text_detector_controller.start_detection(
            vec![(page.idx, page.path.clone())],
            false,
            mode,
            self.text_detector_options.mask_dilate_size,
        ) {
            Ok(()) => {
                self.text_detector_progress = Some((0, 1));
                self.set_text_detector_status(
                    self.text_detector_running_status(),
                    TEXT_DETECTOR_STATUS_WARN,
                );
            }
            Err(error) => {
                self.set_text_detector_status(
                    format!("Ошибка запуска детектора: {error}"),
                    TEXT_DETECTOR_STATUS_ERR,
                );
            }
        }
    }

    fn start_text_detection_for_all_pages(&mut self, project: &ProjectData) {
        if self.text_detector_controller.is_busy() {
            return;
        }
        let pages = project
            .pages
            .iter()
            .map(|page| (page.idx, page.path.clone()))
            .collect::<Vec<_>>();
        let mode = match self.text_detector_run_mode() {
            Ok(mode) => mode,
            Err(error) => {
                self.set_text_detector_status(error, TEXT_DETECTOR_STATUS_ERR);
                return;
            }
        };
        let total = pages.len();
        match self.text_detector_controller.start_detection(
            pages,
            true,
            mode,
            self.text_detector_options.mask_dilate_size,
        ) {
            Ok(()) => {
                self.text_detector_progress = Some((0, total));
                self.set_text_detector_status(
                    self.text_detector_running_status(),
                    TEXT_DETECTOR_STATUS_WARN,
                );
            }
            Err(error) => {
                self.set_text_detector_status(
                    format!("Ошибка запуска детектора: {error}"),
                    TEXT_DETECTOR_STATUS_ERR,
                );
            }
        }
    }

    fn start_text_detector_ocr_for_indices(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        mut indices: Vec<usize>,
    ) {
        if !self.ai_enabled {
            self.push_toast(
                ctx,
                "OCR отключён флагом --no-ai.".to_string(),
                Color32::from_rgb(225, 180, 60),
                2.6,
            );
            return;
        }
        if let Some(error) = self.current_ocr_torch_requirement_error() {
            self.push_toast(ctx, error, Color32::RED, 2.6);
            return;
        }
        if self.ocr_controller.state() != OcrLoadState::Ready {
            self.set_text_detector_status(
                "Движок распознавания не загружен",
                TEXT_DETECTOR_STATUS_ERR,
            );
            return;
        }
        if self.textdetector_ocr_is_running() {
            return;
        }

        indices.sort_unstable();
        indices.dedup();

        let mut tasks = VecDeque::new();
        for page_idx in indices {
            let Some(result) = self.text_detector_results.get(&page_idx) else {
                continue;
            };
            let blocks = detector_blocks_with_options(result, &self.text_detector_options);
            if blocks.is_empty() {
                continue;
            }
            let source_w = result.source_size[0].max(1) as f32;
            let source_h = result.source_size[1].max(1) as f32;
            for rect in blocks {
                let u1 = (rect.x1 / source_w).clamp(0.0, 1.0);
                let v1 = (rect.y1 / source_h).clamp(0.0, 1.0);
                let u2 = (rect.x2 / source_w).clamp(0.0, 1.0);
                let v2 = (rect.y2 / source_h).clamp(0.0, 1.0);
                if (u2 - u1) <= 0.0001 || (v2 - v1) <= 0.0001 {
                    continue;
                }
                tasks.push_back(TextDetectorOcrTask {
                    page_idx,
                    uv_rect: [u1, v1, u2, v2],
                    retry_attempt: 0,
                });
            }
        }

        if tasks.is_empty() {
            self.set_text_detector_status("Нет блоков для распознавания", TEXT_DETECTOR_STATUS_ERR);
            self.text_detector_progress = None;
            return;
        }

        self.pending_textdetector_ocr_tasks = tasks;
        self.textdetector_ocr_active_request_id = None;
        self.textdetector_ocr_active_task = None;
        self.textdetector_ocr_retry_state = None;
        self.textdetector_ocr_total = self.pending_textdetector_ocr_tasks.len();
        self.textdetector_ocr_done = 0;
        self.textdetector_ocr_recognized = 0;
        self.text_detector_progress = Some((0, self.textdetector_ocr_total));
        self.set_text_detector_status("Распознавание блоков...", TEXT_DETECTOR_STATUS_WARN);
        self.maybe_dispatch_next_textdetector_ocr_request(ctx, project);
    }

    fn maybe_dispatch_next_textdetector_ocr_request(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
    ) {
        if self.textdetector_ocr_total == 0 {
            return;
        }

        if self.textdetector_ocr_active_request_id.is_some() {
            self.text_detector_progress = Some((
                self.textdetector_ocr_done,
                self.textdetector_ocr_total.max(1),
            ));
            return;
        }

        if let Some(retry_state) = self.textdetector_ocr_retry_state {
            let now_s = ctx.input(|i| i.time);
            if now_s < retry_state.retry_at_s {
                self.text_detector_progress = Some((
                    self.textdetector_ocr_done,
                    self.textdetector_ocr_total.max(1),
                ));
                return;
            }
            self.textdetector_ocr_retry_state = None;
            self.pending_textdetector_ocr_tasks
                .push_front(retry_state.task);
            self.set_text_detector_status("Распознавание блоков...", TEXT_DETECTOR_STATUS_WARN);
        }

        if self.ocr_controller.state().is_busy() {
            self.text_detector_progress = Some((
                self.textdetector_ocr_done,
                self.textdetector_ocr_total.max(1),
            ));
            return;
        }

        while let Some(task) = self.pending_textdetector_ocr_tasks.pop_front() {
            let Some(page) = project.pages.iter().find(|page| page.idx == task.page_idx) else {
                self.textdetector_ocr_done = self.textdetector_ocr_done.saturating_add(1);
                continue;
            };

            let request_id = self.next_ocr_request_id;
            self.next_ocr_request_id = self.next_ocr_request_id.saturating_add(1);
            let request = OcrRecognizeRequest {
                request_id,
                engine: self.ocr_panel_options.engine,
                options: build_ocr_runtime_options(&self.ocr_panel_options),
                page_path: page.path.clone(),
                uv_rect: task.uv_rect,
                image_override_png: None,
                join_newlines: self.ocr_panel_options.join_newlines,
                reflect_strings: self.ocr_panel_options.reflect_strings,
                char_replacements: self.ocr_panel_options.runtime_char_replacements(),
            };
            if self.ocr_panel_options.create_bubble {
                self.pending_bubble_inserts.insert(
                    request_id,
                    OcrPendingBubbleInsert {
                        page_idx: task.page_idx,
                        uv_rect: task.uv_rect,
                        join_newlines: self.ocr_panel_options.join_newlines,
                        recent_character_rank: None,
                    },
                );
            }

            self.ocr_controller.request_recognize(request);
            if self.ocr_controller.state().is_busy() {
                self.mark_ocr_load_requested_for_engine(self.ocr_panel_options.engine);
            }
            if self.ocr_controller.state() == OcrLoadState::Error {
                self.pending_bubble_inserts.remove(&request_id);
                if task.retry_attempt == 0 {
                    let now_s = ctx.input(|i| i.time);
                    self.textdetector_ocr_retry_state = Some(TextDetectorOcrRetryState {
                        task: TextDetectorOcrTask {
                            retry_attempt: 1,
                            ..task
                        },
                        retry_at_s: now_s + TEXT_DETECTOR_OCR_RETRY_DELAY_SECS,
                    });
                    self.set_text_detector_status(
                        format!(
                            "Распознавание недоступно, повтор блока через {:.0} c...",
                            TEXT_DETECTOR_OCR_RETRY_DELAY_SECS
                        ),
                        TEXT_DETECTOR_STATUS_WARN,
                    );
                    break;
                }
                self.abort_textdetector_ocr(
                    ctx,
                    "Распознавание остановлено после повторной ошибки запуска".to_string(),
                );
                break;
            }
            self.textdetector_ocr_active_request_id = Some(request_id);
            self.textdetector_ocr_active_task = Some(task);
            break;
        }

        self.text_detector_progress = Some((
            self.textdetector_ocr_done,
            self.textdetector_ocr_total.max(1),
        ));
        self.finish_textdetector_ocr_if_done(ctx, false);
    }

    fn finish_textdetector_ocr_if_done(&mut self, ctx: &egui::Context, force_error: bool) {
        if self.textdetector_ocr_total == 0
            || self.textdetector_ocr_active_request_id.is_some()
            || self.textdetector_ocr_retry_state.is_some()
            || !self.pending_textdetector_ocr_tasks.is_empty()
            || (!force_error && self.textdetector_ocr_done < self.textdetector_ocr_total)
        {
            return;
        }

        let total = self.textdetector_ocr_total;
        let recognized = self.textdetector_ocr_recognized;
        self.textdetector_ocr_total = 0;
        self.textdetector_ocr_done = 0;
        self.textdetector_ocr_recognized = 0;
        self.textdetector_ocr_active_request_id = None;
        self.textdetector_ocr_active_task = None;
        self.textdetector_ocr_retry_state = None;
        self.pending_textdetector_ocr_tasks.clear();
        self.text_detector_progress = None;

        if force_error {
            self.set_text_detector_status(
                "Движок распознавания не загружен",
                TEXT_DETECTOR_STATUS_ERR,
            );
            return;
        }

        if recognized > 0 {
            self.set_text_detector_status(
                format!("Распознавание готово. Распознано блоков: {recognized}/{total}"),
                TEXT_DETECTOR_STATUS_OK,
            );
            self.push_toast(
                ctx,
                format!("Распознавание по детектору: {recognized}/{total}"),
                Color32::from_rgb(42, 168, 88),
                2.4,
            );
        } else {
            self.set_text_detector_status(
                "Распознавание завершено, текст не найден",
                TEXT_DETECTOR_STATUS_WARN,
            );
        }
    }

    fn abort_textdetector_ocr(&mut self, ctx: &egui::Context, message: String) {
        self.textdetector_ocr_total = 0;
        self.textdetector_ocr_done = 0;
        self.textdetector_ocr_recognized = 0;
        self.textdetector_ocr_active_request_id = None;
        self.textdetector_ocr_active_task = None;
        self.textdetector_ocr_retry_state = None;
        self.pending_textdetector_ocr_tasks.clear();
        self.text_detector_progress = None;
        self.set_text_detector_status(message, TEXT_DETECTOR_STATUS_ERR);
        ctx.request_repaint();
    }

    fn draw_text_detector_mask_layer_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        page_rect: Rect,
        zoom: f32,
    ) {
        if self.active_panel != TranslationPanel::TextDetector {
            return;
        }
        self.sync_text_mask_revision_cache();
        let clip_rect = ui.clip_rect().intersect(page_rect);
        if !clip_rect.is_positive() {
            return;
        }
        let painter = ui.painter().with_clip_rect(clip_rect);

        if self.text_detector_edit_lines_mode {
            return;
        }
        if self.text_detector_edit_mask_mode {
            self.draw_text_detector_mask_edit_overlay_on_page(ui, page_idx, page_rect, zoom);
        }

        let mask_color = Color32::from_rgba_unmultiplied(255, 0, 0, 60);
        let options = self.text_detector_options.clone();
        let result = self.text_detector_results.get(&page_idx);
        let mask_page = self.text_mask_page_snapshot(page_idx);
        let has_mask_alpha = mask_page
            .as_ref()
            .is_some_and(|mask| !mask.mask_alpha.is_empty());
        if result.is_none() && !has_mask_alpha {
            return;
        }

        let show_mask = options.draw_mask || self.text_detector_edit_mask_mode;
        let needs_merged_blocks = show_mask && !has_mask_alpha;
        let merged = match (needs_merged_blocks, result) {
            (true, Some(result)) => {
                let source_w = result.source_size[0].max(1) as f32;
                let source_h = result.source_size[1].max(1) as f32;
                let expanded = detector_expand_blocks(
                    &result.blocks,
                    options.block_expand_px,
                    source_w,
                    source_h,
                );
                detector_merge_blocks(&expanded, options.merge_gap_px)
            }
            _ => Vec::new(),
        };

        if show_mask {
            if let Some(mask) = mask_page
                .as_ref()
                .filter(|mask| !mask.mask_alpha.is_empty())
            {
                let mask_textures = &mut self.text_detector_mask_textures;
                draw_text_detector_mask_overlay_on_page(TextDetectorMaskOverlayDrawParams {
                    textures: mask_textures,
                    ctx,
                    painter: &painter,
                    page_idx,
                    page_rect,
                    mask_size: mask.mask_size,
                    mask_alpha: &mask.mask_alpha,
                    current_frame: ctx.cumulative_frame_nr(),
                });
            } else if let Some(result) = result {
                let source_w = result.source_size[0].max(1) as f32;
                let source_h = result.source_size[1].max(1) as f32;
                for rect in &merged {
                    if let Some(scene_rect) =
                        source_rect_to_scene_rect(page_rect, source_w, source_h, *rect)
                    {
                        painter.rect_filled(scene_rect, 0.0, mask_color);
                    }
                }
            }
        }
    }

    fn draw_text_detector_additional_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        page_rect: Rect,
    ) {
        if self.active_panel != TranslationPanel::TextDetector {
            return;
        }
        let clip_rect = ui.clip_rect().intersect(page_rect);
        if !clip_rect.is_positive() {
            return;
        }
        let painter = ui.painter().with_clip_rect(clip_rect);

        if self.text_detector_edit_lines_mode {
            self.draw_text_detector_line_edit_overlay_on_page(ctx, &painter, page_idx, page_rect);
            return;
        }
        if self.text_detector_edit_mask_mode {
            return;
        }

        let raw_stroke = Stroke::new(1.2, Color32::from_rgb(0, 255, 0));
        let merged_stroke = Stroke::new(2.0, Color32::from_rgb(0, 160, 255));
        let options = self.text_detector_options.clone();
        let Some(result) = self.text_detector_results.get(&page_idx) else {
            return;
        };
        if !options.draw_lines {
            return;
        }
        let source_w = result.source_size[0].max(1) as f32;
        let source_h = result.source_size[1].max(1) as f32;
        let expanded =
            detector_expand_blocks(&result.blocks, options.block_expand_px, source_w, source_h);
        let merged = detector_merge_blocks(&expanded, options.merge_gap_px);
        for rect in &expanded {
            if let Some(scene_rect) =
                source_rect_to_scene_rect(page_rect, source_w, source_h, *rect)
            {
                painter.rect_stroke(scene_rect, 0.0, raw_stroke, egui::StrokeKind::Outside);
            }
        }
        for rect in &merged {
            if let Some(scene_rect) =
                source_rect_to_scene_rect(page_rect, source_w, source_h, *rect)
            {
                painter.rect_stroke(scene_rect, 0.0, merged_stroke, egui::StrokeKind::Outside);
            }
        }
    }

    fn draw_text_detector_line_edit_overlay_on_page(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        page_idx: usize,
        page_rect: Rect,
    ) {
        let Some(result) = self.text_detector_results.get(&page_idx) else {
            if self
                .text_detector_line_selection
                .is_some_and(|selection| selection.page_idx == page_idx)
            {
                self.text_detector_line_selection = None;
                self.text_detector_line_drag_state = None;
            }
            return;
        };
        let source_w = result.source_size[0].max(1) as f32;
        let source_h = result.source_size[1].max(1) as f32;
        let lines = result.blocks.clone();
        let selected_idx = self
            .text_detector_line_selection
            .filter(|selection| selection.page_idx == page_idx)
            .map(|selection| selection.line_idx);
        let pointer = ctx.input(|i| i.pointer.interact_pos());
        let primary_pressed = ctx.input(|i| i.pointer.primary_pressed());
        let primary_down = ctx.input(|i| i.pointer.primary_down());
        let primary_released = ctx.input(|i| i.pointer.primary_released());

        if primary_released
            && self
                .text_detector_line_drag_state
                .is_some_and(|drag| drag.selection.page_idx == page_idx)
        {
            self.text_detector_line_drag_state = None;
        }

        let mut clicked_handle: Option<(usize, usize)> = None;
        if let (Some(pointer_pos), Some(selection_idx)) = (pointer, selected_idx)
            && let Some(selected_rect) = lines.get(selection_idx).copied()
            && let Some(scene_rect) =
                source_rect_to_scene_rect(page_rect, source_w, source_h, selected_rect)
        {
            for (handle_idx, handle_pos) in text_detector_line_handle_points(scene_rect)
                .into_iter()
                .enumerate()
            {
                if handle_pos.distance(pointer_pos) <= 8.0 {
                    clicked_handle = Some((selection_idx, handle_idx));
                    break;
                }
            }
        }

        if primary_pressed {
            if let Some((line_idx, handle_idx)) = clicked_handle {
                if let Some(pointer_pos) = pointer
                    && let Some(src_pointer) =
                        scene_pos_to_source(page_rect, source_w, source_h, pointer_pos)
                    && let Some(start_rect) = lines.get(line_idx).copied()
                {
                    self.text_detector_line_selection =
                        Some(TextDetectorLineSelection { page_idx, line_idx });
                    self.text_detector_line_drag_state = Some(TextDetectorLineDragState {
                        selection: TextDetectorLineSelection { page_idx, line_idx },
                        start_pointer_src: src_pointer,
                        start_rect,
                        kind: TextDetectorLineDragKind::Resize { handle_idx },
                    });
                }
            } else if let Some(pointer_pos) = pointer {
                let hit = lines.iter().enumerate().rev().find_map(|(line_idx, rect)| {
                    source_rect_to_scene_rect(page_rect, source_w, source_h, *rect)
                        .filter(|scene_rect| scene_rect.contains(pointer_pos))
                        .map(|_| line_idx)
                });
                if let Some(line_idx) = hit {
                    if let Some(src_pointer) =
                        scene_pos_to_source(page_rect, source_w, source_h, pointer_pos)
                        && let Some(start_rect) = lines.get(line_idx).copied()
                    {
                        self.text_detector_line_selection =
                            Some(TextDetectorLineSelection { page_idx, line_idx });
                        self.text_detector_line_drag_state = Some(TextDetectorLineDragState {
                            selection: TextDetectorLineSelection { page_idx, line_idx },
                            start_pointer_src: src_pointer,
                            start_rect,
                            kind: TextDetectorLineDragKind::Move,
                        });
                    }
                } else {
                    self.text_detector_line_selection = None;
                    self.text_detector_line_drag_state = None;
                }
            }
        }

        if primary_down
            && let Some(drag) = self
                .text_detector_line_drag_state
                .filter(|drag| drag.selection.page_idx == page_idx)
            && let Some(pointer_pos) = pointer
            && let Some(pointer_src) =
                scene_pos_to_source(page_rect, source_w, source_h, pointer_pos)
            && let Some(current_result) = self.text_detector_results.get_mut(&page_idx)
            && drag.selection.line_idx < current_result.blocks.len()
        {
            let next_rect = match drag.kind {
                TextDetectorLineDragKind::Move => move_text_detector_rect(
                    drag.start_rect,
                    drag.start_pointer_src,
                    pointer_src,
                    source_w,
                    source_h,
                ),
                TextDetectorLineDragKind::Resize { handle_idx } => resize_text_detector_rect(
                    drag.start_rect,
                    pointer_src,
                    handle_idx,
                    source_w,
                    source_h,
                ),
            };
            current_result.blocks[drag.selection.line_idx] = next_rect;
        }

        for (line_idx, rect) in lines.iter().copied().enumerate() {
            if let Some(scene_rect) = source_rect_to_scene_rect(page_rect, source_w, source_h, rect)
            {
                let stroke = if Some(line_idx) == selected_idx {
                    Stroke::new(2.2, Color32::from_rgb(120, 255, 120))
                } else {
                    Stroke::new(1.2, Color32::from_rgb(0, 255, 0))
                };
                painter.rect_stroke(scene_rect, 0.0, stroke, egui::StrokeKind::Outside);
                if Some(line_idx) == selected_idx {
                    for handle_pos in text_detector_line_handle_points(scene_rect) {
                        painter.circle_filled(handle_pos, 4.0, Color32::from_rgb(120, 255, 120));
                        painter.circle_stroke(
                            handle_pos,
                            4.0,
                            Stroke::new(1.0, Color32::from_rgb(20, 90, 20)),
                        );
                    }
                }
            }
        }
    }

    fn draw_text_detector_mask_edit_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        page_rect: Rect,
        zoom: f32,
    ) {
        self.materialize_text_mask_page_from_blocks_if_missing(page_idx);
        let response = ui.interact(
            page_rect,
            egui::Id::new(("translation_text_detector_mask_edit", page_idx)),
            egui::Sense::click_and_drag(),
        );
        let hovered = response.hovered();
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }

        let pointer_pos = response.interact_pointer_pos();
        let hover_pos = response.hover_pos();
        let (primary_down, secondary_down, shift_down) = ui.ctx().input(|input| {
            (
                input.pointer.primary_down(),
                input.pointer.secondary_down(),
                input.modifiers.shift,
            )
        });
        let mode = if secondary_down || (primary_down && shift_down) {
            Some(true)
        } else if primary_down {
            Some(false)
        } else {
            None
        };

        if mode.is_none() {
            self.text_detector_mask_stroke_state = None;
        }

        let _ = self.text_detector_handle_mask_brush_wheel(ui, hovered);
        let _ = self.text_detector_handle_mask_brush_hotkeys(ui, hovered);

        if let (Some(erase), Some(pos)) = (mode, pointer_pos)
            && page_rect.contains(pos)
        {
            let start_pos = match self.text_detector_mask_stroke_state {
                Some(state) if state.page_idx == page_idx && state.erase == erase => {
                    state.last_scene_pos
                }
                _ => pos,
            };
            if let Some((source_size, mask_size)) =
                self.text_mask_target_sizes(page_idx, page_rect, zoom)
            {
                let _ = self.paint_text_mask_segment(
                    page_idx,
                    source_size,
                    mask_size,
                    page_rect,
                    start_pos,
                    pos,
                    erase,
                );
            }
            self.text_detector_mask_stroke_state = Some(TextDetectorMaskStrokeState {
                page_idx,
                erase,
                last_scene_pos: pos,
            });
        }

        if let Some(pointer) = hover_pos
            && page_rect.contains(pointer)
        {
            let mask_size = self
                .text_mask_target_sizes(page_idx, page_rect, zoom)
                .map(|(_, mask_size)| mask_size)
                .unwrap_or([1, 1]);
            self.text_detector_mask_brush.draw_circle_cursor_on_image(
                ui,
                page_rect,
                [mask_size[0].max(1) as usize, mask_size[1].max(1) as usize],
                pointer,
            );
        }
    }

    fn handle_text_detector_line_edit_hotkeys(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
    ) {
        if !self.text_detector_edit_lines_mode || ctx.wants_keyboard_input() {
            return;
        }
        let delete_pressed =
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Delete));
        if delete_pressed && let Some(selection) = self.text_detector_line_selection {
            if let Some(result) = self.text_detector_results.get_mut(&selection.page_idx)
                && selection.line_idx < result.blocks.len()
            {
                result.blocks.remove(selection.line_idx);
            }
            self.text_detector_line_selection = None;
            self.text_detector_line_drag_state = None;
        }

        let create_pressed = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::N));
        if create_pressed {
            let page_idx = canvas.current_page_idx();
            let _ = self.create_text_detector_line_at_uv(project, page_idx, egui::pos2(0.5, 0.5));
        }
    }

    fn create_text_detector_line_at_uv(
        &mut self,
        project: &ProjectData,
        page_idx: usize,
        uv: Pos2,
    ) -> Result<(), String> {
        let next_idx = {
            let result = self.ensure_text_detector_page_entry(project, page_idx)?;
            let source_w = result.source_size[0].max(1) as f32;
            let source_h = result.source_size[1].max(1) as f32;
            let center_x = uv.x.clamp(0.0, 1.0) * source_w;
            let center_y = uv.y.clamp(0.0, 1.0) * source_h;
            let half_w = (source_w * 0.08).clamp(20.0, 220.0);
            let half_h = (source_h * 0.03).clamp(12.0, 120.0);
            let rect = TextDetectorRect::from_xyxy(
                (center_x - half_w).clamp(0.0, source_w),
                (center_y - half_h).clamp(0.0, source_h),
                (center_x + half_w).clamp(0.0, source_w),
                (center_y + half_h).clamp(0.0, source_h),
            )
            .ok_or_else(|| "Не удалось создать строку: невалидные координаты.".to_string())?;
            result.blocks.push(rect);
            result.blocks.len().saturating_sub(1)
        };
        self.text_detector_line_selection = Some(TextDetectorLineSelection {
            page_idx,
            line_idx: next_idx,
        });
        self.text_detector_line_drag_state = None;
        Ok(())
    }

    fn ensure_text_detector_page_entry(
        &mut self,
        project: &ProjectData,
        page_idx: usize,
    ) -> Result<&mut TextDetectorPageResult, String> {
        if let std::collections::hash_map::Entry::Vacant(e) =
            self.text_detector_results.entry(page_idx)
        {
            let page = project
                .pages
                .iter()
                .find(|page| page.idx == page_idx)
                .ok_or_else(|| format!("Страница #{page_idx} не найдена."))?;
            let (w, h) = image::image_dimensions(&page.path).map_err(|err| {
                format!(
                    "Не удалось получить размер изображения для страницы #{} ({}): {err}",
                    page_idx,
                    page.path.display()
                )
            })?;
            e.insert(TextDetectorPageResult {
                source_size: [w.max(1), h.max(1)],
                blocks: Vec::new(),
                mask_size: [w.max(1), h.max(1)],
                mask_alpha: Vec::new(),
            });
        }
        self.text_detector_results
            .get_mut(&page_idx)
            .ok_or_else(|| format!("Не удалось подготовить детекцию для страницы #{page_idx}."))
    }

    fn poll_mt_events(&mut self, ctx: &egui::Context, canvas: &mut CanvasView) {
        for event in self.mt_controller.poll_events() {
            match event {
                MtControllerEvent::RunStarted { total } => {
                    // A fresh run clears any stale credit/limit notice from a previous run.
                    self.mt_stop_notice = None;
                    self.mt_progress = Some(MtPanelProgress {
                        total,
                        ..MtPanelProgress::default()
                    });
                }
                MtControllerEvent::ItemTranslated {
                    bubble_id,
                    translated_text,
                    original_text,
                } => {
                    let applied = if let Some(original_text) = original_text {
                        canvas.apply_machine_translation_result_with_original(
                            bubble_id,
                            original_text,
                            translated_text,
                        )
                    } else {
                        canvas.apply_machine_translation_result(bubble_id, translated_text)
                    };
                    if !applied {
                        self.push_toast(
                            ctx,
                            format!("Не удалось применить перевод для пузыря #{bubble_id}."),
                            Color32::from_rgb(255, 172, 66),
                            2.4,
                        );
                    }
                }
                MtControllerEvent::ItemAreasTranslated { bubble_id, areas } => {
                    if !canvas.apply_machine_translation_areas(bubble_id, areas) {
                        self.push_toast(
                            ctx,
                            format!(
                                "Не удалось применить перевод областей для пузыря #{bubble_id}."
                            ),
                            Color32::from_rgb(255, 172, 66),
                            2.4,
                        );
                    }
                }
                MtControllerEvent::ItemFailed { bubble_id, error } => {
                    eprintln!(
                        "[MT][ItemFailed] bubble_id={bubble_id} error={}",
                        error.replace('\n', " ")
                    );
                }
                MtControllerEvent::RunFinished { translated, errors } => {
                    self.mt_progress = None;
                    let color = if errors == 0 {
                        Color32::from_rgb(42, 168, 88)
                    } else {
                        Color32::from_rgb(255, 172, 66)
                    };
                    self.push_toast(
                        ctx,
                        format!(
                            "Маш. перевод: {translated}/{}, ошибок: {errors}",
                            translated + errors
                        ),
                        color,
                        2.8,
                    );
                }
                MtControllerEvent::RunCancelled { translated, errors } => {
                    self.mt_progress = None;
                    self.push_toast(
                        ctx,
                        format!("Маш. перевод отменён: готово {translated}, ошибок: {errors}"),
                        Color32::from_rgb(255, 172, 66),
                        2.8,
                    );
                }
                MtControllerEvent::RunFailed { error } => {
                    self.mt_progress = None;
                    eprintln!("[MT][RunFailed] {}", error.replace('\n', " "));
                    if is_probable_quota_or_limit_error(&error) {
                        // Stop quietly: replace the red error toast with a sticky yellow notice that
                        // keeps the full provider error available behind a toggle.
                        self.mt_stop_notice = Some(MtStopNotice {
                            full_error: error,
                            expanded: false,
                        });
                    } else {
                        self.push_toast(
                            ctx,
                            format!("Маш. перевод ошибка: {error}"),
                            Color32::RED,
                            3.2,
                        );
                    }
                }
                MtControllerEvent::Progress {
                    translated,
                    errors,
                    total,
                    context_used_chars,
                    context_budget_chars,
                    pruned_replicas,
                } => {
                    self.mt_progress = Some(MtPanelProgress {
                        translated,
                        errors,
                        total,
                        context_used_chars,
                        context_budget_chars,
                        pruned_replicas,
                    });
                    self.push_toast(
                        ctx,
                        format!(
                            "ИИ перевод: {translated}/{total}, ошибок: {errors}. Контекст: {}/{}. Обрезано реплик: {pruned_replicas}",
                            format_context_chars(context_used_chars),
                            format_context_chars(context_budget_chars),
                        ),
                        Color32::from_rgb(255, 172, 66),
                        2.4,
                    );
                }
                MtControllerEvent::AiApiKeyStored { service } => {
                    if self.mt_panel_options.ai_api_service == service {
                        self.mt_panel_options.ai_api_key_edit.clear();
                        self.mt_panel_options.ai_api_key_configured = Some(true);
                        self.mt_panel_options.ai_api_status =
                            format!("API key {} сохранен.", service.label());
                        self.mt_controller.refresh_ai_api_metadata(service);
                    }
                    self.push_toast(
                        ctx,
                        format!("API key {} сохранен.", service.label()),
                        Color32::from_rgb(42, 168, 88),
                        2.2,
                    );
                }
                MtControllerEvent::AiApiKeyCleared { service } => {
                    if self.mt_panel_options.ai_api_service == service {
                        self.mt_panel_options.ai_api_key_edit.clear();
                        self.mt_panel_options.ai_api_key_configured = Some(false);
                        self.mt_panel_options.ai_api_models.clear();
                        self.mt_panel_options.ai_api_account_status =
                            "API key не задан".to_string();
                        self.mt_panel_options.ai_api_status =
                            format!("API key {} удален.", service.label());
                    }
                }
                MtControllerEvent::AiApiMetadataLoaded(metadata) => {
                    if self.mt_panel_options.ai_api_service == metadata.service {
                        self.mt_panel_options.ai_api_key_configured = Some(metadata.key_configured);
                        self.mt_panel_options.ai_api_models = metadata.models;
                        self.mt_panel_options.ai_api_account_status = metadata.account_status;
                        if !self
                            .mt_panel_options
                            .ai_api_models
                            .iter()
                            .any(|model| model == &self.mt_panel_options.ai_api_model)
                            && let Some(model) = self.mt_panel_options.ai_api_models.first()
                        {
                            self.mt_panel_options.ai_api_model = model.clone();
                            self.mt_settings_dirty = true;
                        }
                        self.mt_panel_options.ai_api_status =
                            "AI API данные обновлены.".to_string();
                    }
                }
                MtControllerEvent::AiApiMetadataFailed { service, error } => {
                    if self.mt_panel_options.ai_api_service == service {
                        self.mt_panel_options.ai_api_status = error.clone();
                    }
                    self.push_toast(ctx, format!("AI API: {error}"), Color32::RED, 3.0);
                }
            }
        }
    }

    fn mt_has_active_or_pending(&self) -> bool {
        self.mt_controller.is_busy()
            || self.pending_mt_start_all
            || self.pending_mt_start_page
            || !self.pending_translate_actions.is_empty()
    }

    fn cancel_active_mt(&mut self, ctx: &egui::Context) {
        let had_pending = self.pending_mt_start_all
            || self.pending_mt_start_page
            || !self.pending_translate_actions.is_empty();
        self.pending_mt_start_all = false;
        self.pending_mt_start_page = false;
        self.pending_translate_actions.clear();

        if self.mt_controller.request_cancel() {
            self.push_toast(
                ctx,
                "Машинный перевод отменён.".to_string(),
                Color32::from_rgb(255, 172, 66),
                2.2,
            );
        } else if had_pending {
            self.push_toast(
                ctx,
                "Отложенный запуск машинного перевода отменён.".to_string(),
                Color32::from_rgb(255, 172, 66),
                2.2,
            );
        }
    }

    fn handle_pending_mt_actions(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
    ) {
        if self.pending_mt_start_all {
            self.pending_mt_start_all = false;
            self.start_mt_for_scope(ctx, canvas, project, false);
        }
        if self.pending_mt_start_page {
            self.pending_mt_start_page = false;
            self.start_mt_for_scope(ctx, canvas, project, true);
        }
        if self.mt_controller.is_busy() || self.pending_translate_actions.is_empty() {
            return;
        }
        let bubble_ids = std::mem::take(&mut self.pending_translate_actions);
        self.start_mt_for_ids(ctx, canvas, project, bubble_ids);
    }

    fn start_mt_for_scope(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        current_page_only: bool,
    ) {
        let items = self.collect_ai_mt_scope_items(canvas, project, current_page_only);
        if !items.iter().any(|item| item.needs_translation) {
            self.push_toast(
                ctx,
                "Нет пузырей для машинного перевода.".to_string(),
                Color32::from_rgb(225, 180, 60),
                2.3,
            );
            return;
        }
        self.start_mt_with_items(ctx, project, items);
    }

    /// Collects the MT items for a scope translation (whole project or current page) using the same
    /// rules as a real run: image-bubble inclusion, multimodal gating, and AI context mode that adds
    /// already-translated replicas as ordered read-only context. Shared by the translate action and
    /// the request preview so both see identical input.
    fn collect_ai_mt_scope_items(
        &self,
        canvas: &CanvasView,
        project: &ProjectData,
        current_page_only: bool,
    ) -> Vec<MtTranslateItem> {
        if self.ai_mt_imagebubble_mode_active() {
            return self.collect_ai_mt_imagebubble_items(canvas, project, current_page_only);
        }
        let current_page = canvas.current_page_idx();
        let mut items = Vec::new();
        let include_image_bubbles = self.ai_mt_can_include_image_bubbles();
        // When AI context mode is on, already-translated replicas are kept as ordered read-only
        // context so the model sees the correct reading order around the untranslated replicas.
        let context_mode = self.ai_mt_context_includes_translated();
        for bubble in project.bubbles.iter() {
            if current_page_only && bubble.img_idx != current_page {
                continue;
            }
            if is_image_bubble_record(bubble) && !include_image_bubbles {
                continue;
            }
            let image_input = if include_image_bubbles {
                mt_image_input_for_bubble(bubble)
            } else {
                None
            };
            let has_translation = !bubble.text.trim().is_empty();
            // An image bubble is translatable whenever it has no translation yet (the model infers
            // its source text); a text bubble also needs a non-empty source.
            let needs_translation = if image_input.is_some() {
                !has_translation
            } else {
                !has_translation && !bubble.original_text.trim().is_empty()
            };
            let is_context = !needs_translation && context_mode && has_translation;
            if !needs_translation && !is_context {
                continue;
            }
            items.push(MtTranslateItem {
                bubble_id: bubble.id,
                page_idx: bubble.img_idx,
                img_v: bubble.img_v,
                order: bubble_order_for_sort(bubble),
                character: character_for_bubble(
                    bubble,
                    self.mt_panel_options.ai_use_character_names,
                ),
                text: bubble.original_text.clone(),
                existing_translation: bubble.text.clone(),
                // Context replicas never carry an image binary; they exist only for ordering.
                image: if needs_translation { image_input } else { None },
                needs_translation,
            });
        }
        items
    }

    /// True when the AI per-ImageBubble mode is selected and usable (AI tab + multimodal model).
    fn ai_mt_imagebubble_mode_active(&self) -> bool {
        self.mt_panel_options.active_tab == MtPanelTab::AiApi
            && self.mt_panel_options.ai_image_mode == AiMtImageMode::ImagesOnly
            && is_likely_multimodal_model(&self.mt_panel_options.ai_api_model)
    }

    /// Collects items for the per-ImageBubble mode: every chapter bubble in reading order is included
    /// as ordered context (text only, no binary), and `needs_translation` is set only on the in-scope
    /// ImageBubbles that still lack a translation. The context spans the full chapter regardless of
    /// the page scope, so each translated ImageBubble sees everything before it; the page scope only
    /// restricts which ImageBubbles are actually translated.
    fn collect_ai_mt_imagebubble_items(
        &self,
        canvas: &CanvasView,
        project: &ProjectData,
        current_page_only: bool,
    ) -> Vec<MtTranslateItem> {
        let current_page = canvas.current_page_idx();
        let mut items = Vec::new();
        for bubble in project.bubbles.iter() {
            let is_image = is_image_bubble_record(bubble);
            let has_translation = !bubble.text.trim().is_empty();
            let in_target_scope = !current_page_only || bubble.img_idx == current_page;
            // Only still-untranslated ImageBubbles in scope are translated; everything else (text
            // bubbles, already-translated or out-of-scope images) is read-only ordered context.
            let needs_translation = is_image && !has_translation && in_target_scope;
            let image_input = if needs_translation {
                mt_image_input_for_bubble(bubble)
            } else {
                None
            };
            if needs_translation && image_input.is_none() {
                // An ImageBubble we cannot load a binary for falls back to plain context.
                continue;
            }
            items.push(MtTranslateItem {
                bubble_id: bubble.id,
                page_idx: bubble.img_idx,
                img_v: bubble.img_v,
                order: bubble_order_for_sort(bubble),
                character: character_for_bubble(
                    bubble,
                    self.mt_panel_options.ai_use_character_names,
                ),
                text: bubble.original_text.clone(),
                existing_translation: bubble.text.clone(),
                image: image_input,
                needs_translation,
            });
        }
        items
    }

    fn start_mt_for_ids(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        bubble_ids: Vec<i64>,
    ) {
        let mut ids = bubble_ids;
        ids.sort_unstable();
        ids.dedup();
        let mut items = Vec::new();
        let include_image_bubbles = self.ai_mt_can_include_image_bubbles();
        for bubble_id in ids {
            let project_bubble = project.bubbles.iter().find(|bubble| bubble.id == bubble_id);
            let source_text = canvas
                .bubble_original_text(bubble_id)
                .or_else(|| project_bubble.map(|bubble| bubble.original_text.clone()))
                .unwrap_or_default();
            let Some(bubble) = project_bubble else {
                continue;
            };
            if is_image_bubble_record(bubble) && !include_image_bubbles {
                continue;
            }
            let image_input = if include_image_bubbles {
                mt_image_input_for_bubble(bubble)
            } else {
                None
            };
            if source_text.trim().is_empty() && image_input.is_none() {
                continue;
            }
            items.push(MtTranslateItem {
                bubble_id,
                page_idx: bubble.img_idx,
                img_v: bubble.img_v,
                order: bubble_order_for_sort(bubble),
                character: character_for_bubble(
                    bubble,
                    self.mt_panel_options.ai_use_character_names,
                ),
                text: source_text,
                existing_translation: bubble.text.clone(),
                image: image_input,
                // Explicit per-id requests always translate the selected replicas.
                needs_translation: true,
            });
        }
        if items.is_empty() {
            return;
        }
        self.start_mt_with_items(ctx, project, items);
    }

    fn start_mt_with_items(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        items: Vec<MtTranslateItem>,
    ) {
        if !self.ai_enabled {
            self.push_toast(
                ctx,
                "Машинный перевод отключён флагом --no-ai.".to_string(),
                Color32::from_rgb(225, 180, 60),
                2.6,
            );
            return;
        }
        if self.mt_controller.is_busy() {
            self.push_toast(
                ctx,
                "Перевод уже выполняется.".to_string(),
                Color32::from_rgb(255, 172, 66),
                2.2,
            );
            return;
        }

        let request = MtTranslateRequest {
            service: self.mt_panel_options.service,
            source_lang: normalized_lang_input(&self.mt_panel_options.source_lang, "auto"),
            target_lang: normalized_lang_input(&self.mt_panel_options.target_lang, "ru"),
            items,
            ai_api: self
                .mt_panel_options
                .active_tab
                .eq(&MtPanelTab::AiApi)
                .then(|| self.current_ai_mt_options(project)),
        };

        if let Err(err) = self.mt_controller.start_translation(request) {
            eprintln!("[MT][StartFailed] {}", err.replace('\n', " "));
            self.push_toast(ctx, format!("Ошибка запуска: {err}"), Color32::RED, 3.0);
        } else {
            // Drop any previous credit/limit notice as soon as a new run is accepted.
            self.mt_stop_notice = None;
        }
    }

    fn push_toast(&mut self, ctx: &egui::Context, text: String, color: Color32, duration_s: f64) {
        let now = ctx.input(|i| i.time);
        self.ocr_toast = Some(OcrToast {
            text,
            color,
            hide_at_s: now + duration_s.max(0.2),
        });
    }

    /// Builds the AI MT options from the current panel state. `project` is cloned into the options
    /// because the worker thread needs an owned snapshot for image loading. Only meaningful when the
    /// AI API tab is active.
    fn current_ai_mt_options(&self, project: &ProjectData) -> AiMtOptions {
        AiMtOptions {
            service: self.mt_panel_options.ai_api_service,
            model: self.mt_panel_options.ai_api_model.clone(),
            system_instruction: self.mt_panel_options.ai_api_system_instruction.clone(),
            sort_mode: self.mt_panel_options.ai_sort_mode,
            use_character_names: self.mt_panel_options.ai_use_character_names,
            use_notes_prompt: self.mt_panel_options.ai_use_notes_prompt,
            include_characters: self.mt_panel_options.ai_include_characters,
            include_terms: self.mt_panel_options.ai_include_terms,
            batch_size: self.mt_panel_options.ai_batch_size,
            reasoning: self.mt_panel_options.ai_reasoning,
            context_limit_percent: self.mt_panel_options.ai_context_limit_percent,
            include_existing_translation: self.mt_panel_options.ai_include_existing_translation,
            image_detail: self.mt_panel_options.ai_image_detail,
            image_mode: self.mt_panel_options.ai_image_mode,
            image_context_source: self.mt_panel_options.ai_image_context_source,
            project: project.clone(),
        }
    }

    /// Debug entry point for "Отобразить полный запрос". Collects the same items a real scope
    /// translation would use and builds the first AI request on a background thread (image loading
    /// must not block the GUI). The result is consumed by `poll_ai_mt_request_preview`.
    fn start_ai_mt_request_preview(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        current_page_only: bool,
    ) {
        // Preview is meaningful only for the AI API path; plain translators have no request context.
        if self.mt_panel_options.active_tab != MtPanelTab::AiApi {
            return;
        }
        if !self.ai_enabled {
            self.push_toast(
                ctx,
                "Машинный перевод отключён флагом --no-ai.".to_string(),
                Color32::from_rgb(225, 180, 60),
                2.6,
            );
            return;
        }
        let items = self.collect_ai_mt_scope_items(canvas, project, current_page_only);
        if !items.iter().any(|item| item.needs_translation) {
            self.push_toast(
                ctx,
                "Нет пузырей для предпросмотра запроса.".to_string(),
                Color32::from_rgb(225, 180, 60),
                2.3,
            );
            return;
        }
        let options = self.current_ai_mt_options(project);
        let source_lang = normalized_lang_input(&self.mt_panel_options.source_lang, "auto");
        let target_lang = normalized_lang_input(&self.mt_panel_options.target_lang, "ru");
        let scope_label = if current_page_only {
            "текущая страница"
        } else {
            "весь проект"
        }
        .to_string();

        let (tx, rx) = mpsc::channel();
        self.mt_request_preview_rx = Some(rx);
        self.mt_request_preview = None;
        let ctx_clone = ctx.clone();
        thread::spawn(move || {
            let result = build_ai_mt_request_preview(&source_lang, &target_lang, items, &options)
                .map(|preview| (preview, scope_label));
            let _ = tx.send(result);
            // Wake the UI so the pending result is picked up even if the pointer is idle.
            ctx_clone.request_repaint();
        });
        self.push_toast(
            ctx,
            "Подготовка полного запроса...".to_string(),
            Color32::from_rgb(120, 180, 255),
            1.6,
        );
    }

    /// Consumes a finished background request-preview build, opening the debug window on success or
    /// surfacing the failure as a toast.
    fn poll_ai_mt_request_preview(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.mt_request_preview_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok((preview, scope_label))) => {
                self.mt_request_preview_rx = None;
                let image_count = preview
                    .parts
                    .iter()
                    .filter(|part| matches!(part, MtRequestPreviewPart::Image(_)))
                    .count();
                self.mt_request_preview = Some(MtRequestPreviewWindow {
                    preview,
                    scope_label,
                    open: true,
                    image_textures: vec![None; image_count],
                });
            }
            Ok(Err(error)) => {
                self.mt_request_preview_rx = None;
                self.push_toast(
                    ctx,
                    format!("Не удалось собрать запрос: {error}"),
                    Color32::RED,
                    3.5,
                );
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.mt_request_preview_rx = None;
            }
        }
    }

    /// Draws the debug "Полный запрос ИИ перевода" window: system prompt, then the first batch user
    /// message with image binaries rendered inline at their exact positions. Images are uploaded to
    /// GPU textures lazily on first display.
    fn draw_ai_mt_request_preview_window(&mut self, ctx: &egui::Context) {
        let Some(window) = self.mt_request_preview.as_mut() else {
            return;
        };
        let mut open = window.open;
        // Disjoint borrows of the window fields so the texture cache can be filled while reading the
        // immutable preview content during the same frame.
        let MtRequestPreviewWindow {
            preview,
            scope_label,
            image_textures,
            ..
        } = window;
        egui::Window::new("Полный запрос ИИ перевода")
            .open(&mut open)
            .resizable(true)
            .default_size([720.0, 640.0])
            .show(ctx, |ui| {
                ui.label(format!(
                    "Область: {scope_label} • батч 1/{} • перевод: {} • контекст: {} • items всего: {} • картинок: {} ({} KiB)",
                    preview.batch_total,
                    preview.translate_count,
                    preview.context_count,
                    preview.total_item_count,
                    preview.image_count,
                    preview.image_bytes / 1024,
                ));
                if preview.batch_total > 1 {
                    ui.colored_label(
                        Color32::from_rgb(225, 180, 60),
                        "Показан контекст только первого шага (батча). Остальные батчи уходят отдельными запросами.",
                    );
                }
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.heading("Системный промпт");
                        selectable_monospace(ui, &preview.system_prompt);
                        ui.add_space(10.0);
                        ui.separator();
                        ui.heading("Сообщение пользователя (первый батч)");
                        let mut image_idx = 0usize;
                        for part in &preview.parts {
                            match part {
                                MtRequestPreviewPart::Text(text) => {
                                    selectable_monospace(ui, text);
                                    ui.add_space(6.0);
                                }
                                MtRequestPreviewPart::Image(image) => {
                                    ui.label(format!(
                                        "[картинка пузыря #{}: {}x{}, PNG {} KiB]",
                                        image.bubble_id,
                                        image.width,
                                        image.height,
                                        image.png_byte_len / 1024,
                                    ));
                                    if let Some(slot) = image_textures.get_mut(image_idx) {
                                        let texture = slot.get_or_insert_with(|| {
                                            let dims = [
                                                usize::try_from(image.width).unwrap_or(0),
                                                usize::try_from(image.height).unwrap_or(0),
                                            ];
                                            let color = egui::ColorImage::from_rgba_unmultiplied(
                                                dims, &image.rgba,
                                            );
                                            ui.ctx().load_texture(
                                                format!(
                                                    "mt-request-preview-{}-{image_idx}",
                                                    image.bubble_id
                                                ),
                                                color,
                                                egui::TextureOptions::LINEAR,
                                            )
                                        });
                                        // Fit the long edge to a readable width, never upscaling.
                                        let max_w = ui.available_width().min(360.0);
                                        let w = image.width.max(1) as f32;
                                        let h = image.height.max(1) as f32;
                                        let scale = (max_w / w).min(1.0);
                                        let size = egui::vec2(w * scale, h * scale);
                                        ui.add(egui::Image::new((texture.id(), size)));
                                    }
                                    ui.add_space(8.0);
                                    image_idx += 1;
                                }
                            }
                        }
                    });
            });
        window.open = open;
        if !window.open {
            self.mt_request_preview = None;
        }
    }

    fn ai_mt_can_include_image_bubbles(&self) -> bool {
        self.mt_panel_options.active_tab == MtPanelTab::AiApi
            && self.mt_panel_options.ai_include_image_bubbles
            && is_likely_multimodal_model(&self.mt_panel_options.ai_api_model)
    }

    /// True when a scope translation should also send already-translated replicas as ordered
    /// read-only context. Only meaningful for the AI API path with "existing translation in
    /// context" enabled; the plain translators have no batch context.
    fn ai_mt_context_includes_translated(&self) -> bool {
        self.mt_panel_options.active_tab == MtPanelTab::AiApi
            && self.mt_panel_options.ai_include_existing_translation
    }

    fn draw_toast(&mut self, ctx: &egui::Context, canvas_rect: Rect, top_offset: f32) {
        if !canvas_rect.min.x.is_finite()
            || !canvas_rect.min.y.is_finite()
            || !canvas_rect.max.x.is_finite()
            || !canvas_rect.max.y.is_finite()
        {
            return;
        }
        let now = ctx.input(|i| i.time);
        let Some(toast) = self.ocr_toast.clone() else {
            return;
        };
        if now >= toast.hide_at_s {
            self.ocr_toast = None;
            return;
        }

        egui::Area::new("translation_ocr_toast".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center_top() + egui::vec2(-160.0, top_offset))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, toast.color))
                    .show(ui, |ui| {
                        ui.colored_label(toast.color, toast.text);
                    });
            });
    }

    fn draw_startup_page_load_toast(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        status: CanvasUiStatus,
    ) {
        if !canvas_rect.min.x.is_finite()
            || !canvas_rect.min.y.is_finite()
            || !canvas_rect.max.x.is_finite()
            || !canvas_rect.max.y.is_finite()
        {
            return;
        }
        if status.total_pages == 0 || status.loaded_pages >= status.total_pages {
            return;
        }

        let progress = (status.loaded_pages as f32 / status.total_pages as f32).clamp(0.0, 1.0);
        let title = format!(
            "Загрузка картинок: {}/{}",
            status.loaded_pages, status.total_pages
        );
        let details = if status.load_errors_count > 0 {
            format!("Ошибок загрузки: {}", status.load_errors_count)
        } else {
            "Подготовка страниц для холста...".to_string()
        };

        egui::Area::new("translation_startup_page_load_toast".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center_top() + egui::vec2(-160.0, 16.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, Color32::from_rgb(225, 180, 60)))
                    .show(ui, |ui| {
                        ui.set_width(320.0);
                        ui.label(
                            egui::RichText::new(title)
                                .color(Color32::from_rgb(255, 222, 142))
                                .strong(),
                        );
                        ui.add_space(4.0);
                        let progress_width = (ui.available_width() - 12.0).max(120.0);
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .desired_width(progress_width)
                                .show_percentage(),
                        );
                        ui.add_space(2.0);
                        ui.small(details);
                    });
            });
    }

    fn handle_image_crop_selection(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        let crop_mode_active = Self::image_crop_selection_mode_active(ctx);
        let selection_active = self.image_crop_selection.is_some();
        if !crop_mode_active && !selection_active {
            return;
        }

        egui::Area::new("translation_image_crop_selection_capture".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.min)
            .show(ctx, |ui| {
                ui.set_min_size(canvas_rect.size());
                let local_rect = Rect::from_min_size(Pos2::ZERO, canvas_rect.size());
                let sense = if crop_mode_active {
                    egui::Sense::click_and_drag()
                } else {
                    egui::Sense::hover()
                };
                let response = ui.interact(local_rect, ui.id().with("image_crop_drag"), sense);

                if response.drag_started()
                    && let Some(pos) = response.interact_pointer_pos()
                    && contains_any_page(canvas, project, pos)
                {
                    self.image_crop_selection = Some(ImageCropDragSelection {
                        start: pos,
                        current: pos,
                    });
                }

                if let Some(selection) = self.image_crop_selection.as_mut()
                    && let Some(pos) = ctx.input(|input| input.pointer.latest_pos())
                {
                    selection.current = pos;
                }

                let should_finish = self.image_crop_selection.is_some()
                    && (response.drag_stopped() || !crop_mode_active);
                if should_finish && let Some(selection) = self.image_crop_selection.take() {
                    let rect = selection.rect();
                    if rect.width() >= 4.0 && rect.height() >= 4.0 {
                        self.apply_image_crop_selection(ctx, canvas, project, rect);
                    }
                }
            });

        if let Some(selection) = self.image_crop_selection {
            let rect = selection.rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("translation_image_crop_selection_painter"),
            ));
            painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(220, 35, 35, 45));
            painter.rect_stroke(
                rect,
                0.0,
                Stroke::new(2.0, Color32::from_rgb(220, 35, 35)),
                egui::StrokeKind::Outside,
            );
        }
    }

    fn apply_image_crop_selection(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        selection_rect: Rect,
    ) {
        let Some((page_idx, crop_rect)) =
            build_image_crop_selection(canvas, project, selection_rect)
        else {
            return;
        };
        let live_bubbles = canvas.hook_bubbles_snapshot(project);
        let selected_image_id = canvas.selected_bubble_id().and_then(|selected_id| {
            live_bubbles
                .iter()
                .find(|bubble| {
                    bubble.id == selected_id
                        && bubble.bubble_class.as_deref().map(BubbleClass::from_str)
                            == Some(BubbleClass::Image)
                })
                .map(|bubble| bubble.id)
        });
        let bubble_id = if let Some(bubble_id) = selected_image_id {
            bubble_id
        } else {
            let Some(page_rect) = canvas.page_scene_rect(page_idx) else {
                return;
            };
            let center = egui::pos2(
                page_rect.left() + page_rect.width() * ((crop_rect[0] + crop_rect[2]) * 0.5),
                page_rect.top() + page_rect.height() * ((crop_rect[1] + crop_rect[3]) * 0.5),
            );
            let Some(new_id) = canvas.create_image_bubble_at_scene_pos(ctx, center) else {
                return;
            };
            new_id
        };

        let _ = canvas.set_bubble_class_for_bid(bubble_id, BubbleClass::Image);
        let mut patch = Map::new();
        patch.insert(
            "image_source_type".to_string(),
            Value::String("page_crop".to_string()),
        );
        patch.insert(
            "crop_page_idx".to_string(),
            Value::Number(u64::try_from(page_idx).unwrap_or(u64::MAX).into()),
        );
        patch.insert(
            "crop_rect".to_string(),
            Value::Array(
                crop_rect
                    .iter()
                    .map(|value| Value::from(f64::from(*value)))
                    .collect(),
            ),
        );
        // The image area rect (red) is owned by the canvas; keep `rect_coords` equal to the crop
        // region so the canvas red rect matches the selected crop and clamps text areas to it.
        patch.insert("rect_coords".to_string(), rect_coords_value(crop_rect));
        if canvas.patch_bubble_extra_fields(project, bubble_id, &patch) {
            canvas.flush_pending_bubble_upserts_now(project);
        }
    }

    fn handle_ocr_selection(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        canvas: &CanvasView,
        project: &ProjectData,
    ) {
        if !self.ai_enabled {
            self.ocr_selection = None;
            return;
        }
        if self.advanced_recognition.is_open() {
            self.ocr_selection = None;
            return;
        }
        if Self::image_crop_selection_mode_active(ctx) || self.image_crop_selection.is_some() {
            self.ocr_selection = None;
            return;
        }
        let advanced_mode_active = self.ocr_advanced_selection_mode_active();
        let quick_mode_active = self.ocr_quick_selection_mode_active();
        let selection_active = self.ocr_selection.is_some();
        if !advanced_mode_active && !quick_mode_active && !selection_active {
            return;
        }

        egui::Area::new("translation_ocr_selection_capture".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.min)
            .show(ctx, |ui| {
                ui.set_min_size(canvas_rect.size());
                let local_rect = Rect::from_min_size(Pos2::ZERO, canvas_rect.size());
                let sense = if advanced_mode_active || quick_mode_active {
                    egui::Sense::click_and_drag()
                } else {
                    egui::Sense::hover()
                };
                let response = ui.interact(local_rect, ui.id().with("ocr_selection_drag"), sense);

                if response.drag_started()
                    && let Some(pos) = response.interact_pointer_pos()
                    && contains_any_page(canvas, project, pos)
                {
                    let kind = if advanced_mode_active {
                        OcrSelectionKind::Advanced
                    } else if quick_mode_active {
                        OcrSelectionKind::Simple
                    } else {
                        return;
                    };
                    self.ocr_selection = Some(OcrDragSelection {
                        start: pos,
                        current: pos,
                        kind,
                        recent_character_rank: recent_character_rank_from_input(ctx),
                    });
                }

                if let Some(selection) = self.ocr_selection.as_mut()
                    && let Some(pos) = ctx.input(|i| i.pointer.latest_pos())
                {
                    selection.current = pos;
                    if let Some(rank) = recent_character_rank_from_input(ctx) {
                        selection.recent_character_rank = Some(rank);
                    }
                }

                let should_finish = self.ocr_selection.as_ref().is_some_and(|selection| {
                    response.drag_stopped()
                        || (selection.kind == OcrSelectionKind::Advanced && !advanced_mode_active)
                        || (selection.kind == OcrSelectionKind::Simple && !quick_mode_active)
                });
                if should_finish && let Some(selection) = self.ocr_selection.take() {
                    let rect = selection.rect();
                    if rect.width() >= 4.0 && rect.height() >= 4.0 {
                        match selection.kind {
                            OcrSelectionKind::Simple => {
                                self.start_ocr_for_scene_rect(
                                    ctx,
                                    canvas,
                                    project,
                                    rect,
                                    selection.recent_character_rank,
                                );
                            }
                            OcrSelectionKind::Advanced => {
                                self.open_advanced_recognition_for_scene_rect(
                                    ctx, canvas, project, rect,
                                );
                            }
                        }
                    }
                }
            });

        if let Some(selection) = &self.ocr_selection {
            let rect = selection.rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("translation_ocr_selection_painter"),
            ));
            painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(0, 160, 255, 60));
            painter.rect_stroke(
                rect,
                0.0,
                Stroke::new(2.0, Color32::from_rgb(0, 160, 255)),
                egui::StrokeKind::Outside,
            );
        }
    }

    fn open_advanced_recognition_for_scene_rect(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        scene_rect: Rect,
    ) {
        let request_id = self.next_ocr_request_id;
        let Some(built_request) = build_ocr_request(
            canvas,
            project,
            scene_rect,
            request_id,
            &self.ocr_panel_options,
        ) else {
            self.push_toast(
                ctx,
                "Выделение не пересекается со страницами.".to_string(),
                Color32::RED,
                2.6,
            );
            return;
        };

        let selection = AdvancedRecognitionSelection {
            page_idx: built_request.page_idx,
            uv_rect: built_request.request.uv_rect,
        };
        match self
            .advanced_recognition
            .open_selection(selection, built_request.request.page_path.clone())
        {
            Ok(()) => {
                self.advanced_recognition_request = Some(built_request);
            }
            Err(error) => {
                self.advanced_recognition_request = None;
                self.push_toast(ctx, error, Color32::RED, 3.0);
            }
        }
    }

    fn dispatch_manual_ocr_request(
        &mut self,
        ctx: &egui::Context,
        built_request: BuiltOcrRequest,
        pending_insert: Option<OcrPendingBubbleInsert>,
        result_target: ManualOcrResultTarget,
    ) {
        let request_id = built_request.request.request_id;
        if let Some(insert) = pending_insert {
            self.pending_bubble_inserts.insert(request_id, insert);
        }
        self.manual_ocr_active_request_id = Some(request_id);
        self.manual_ocr_result_target = Some(result_target);

        let was_ready = self.ocr_controller.state() == OcrLoadState::Ready;
        if let Some(error) = self.current_ocr_torch_requirement_error() {
            self.manual_ocr_active_request_id = None;
            self.manual_ocr_result_target = None;
            self.pending_bubble_inserts.remove(&request_id);
            self.push_toast(ctx, error, Color32::RED, 2.6);
            return;
        }
        self.ocr_controller.request_recognize(built_request.request);
        if self.ocr_controller.state().is_busy() {
            self.mark_ocr_load_requested_for_engine(self.ocr_panel_options.engine);
        }
        if self.ocr_controller.state() == OcrLoadState::Error {
            self.manual_ocr_active_request_id = None;
            self.manual_ocr_result_target = None;
            self.pending_bubble_inserts.remove(&request_id);
        }
        if !was_ready {
            self.push_toast(ctx, "Движок загружается...".to_string(), Color32::GOLD, 2.2);
        }
    }

    fn handle_advanced_recognition_window(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        let Some(action) = self.advanced_recognition.draw(ctx) else {
            return;
        };
        match action {
            AdvancedRecognitionAction::Recognize { image_override_png } => {
                if !self.ai_enabled {
                    self.push_toast(
                        ctx,
                        "OCR отключён флагом --no-ai.".to_string(),
                        Color32::from_rgb(225, 180, 60),
                        2.6,
                    );
                    return;
                }
                if ocr_engine_requires_backend_runtime(&self.ocr_panel_options)
                    && self.ai_backend_unavailable()
                {
                    self.push_toast(
                        ctx,
                        "ИИ бэкенд недоступен".to_string(),
                        Color32::from_rgb(240, 102, 102),
                        2.6,
                    );
                    return;
                }
                if let Some(error) = self.current_ocr_torch_requirement_error() {
                    self.push_toast(ctx, error, Color32::RED, 2.6);
                    return;
                }
                if self.manual_ocr_active_request_id.is_some() {
                    return;
                }
                let Some(mut built_request) = self.advanced_recognition_request.clone() else {
                    self.push_toast(
                        ctx,
                        "Не найдено выделение для распознавания.".to_string(),
                        Color32::RED,
                        2.6,
                    );
                    return;
                };
                let request_id = self.next_ocr_request_id;
                self.next_ocr_request_id = self.next_ocr_request_id.saturating_add(1);
                built_request.request.request_id = request_id;
                built_request.request.image_override_png = image_override_png;
                self.advanced_recognition.set_request_running(request_id);
                self.dispatch_manual_ocr_request(
                    ctx,
                    built_request,
                    None,
                    ManualOcrResultTarget::AdvancedWindowText,
                );
                if self.manual_ocr_active_request_id != Some(request_id) {
                    let error = self
                        .ocr_controller
                        .last_error()
                        .unwrap_or("Не удалось запустить распознавание.")
                        .to_string();
                    let _ = self
                        .advanced_recognition
                        .apply_recognition_error(request_id, error);
                }
            }
            AdvancedRecognitionAction::QuickRecognizeSelection { image_override_png } => {
                if !self.ai_enabled {
                    self.push_toast(
                        ctx,
                        "OCR отключён флагом --no-ai.".to_string(),
                        Color32::from_rgb(225, 180, 60),
                        2.6,
                    );
                    return;
                }
                if ocr_engine_requires_backend_runtime(&self.ocr_panel_options)
                    && self.ai_backend_unavailable()
                {
                    self.push_toast(
                        ctx,
                        "ИИ бэкенд недоступен".to_string(),
                        Color32::from_rgb(240, 102, 102),
                        2.6,
                    );
                    return;
                }
                if let Some(error) = self.current_ocr_torch_requirement_error() {
                    self.push_toast(ctx, error, Color32::RED, 2.6);
                    return;
                }
                if self.manual_ocr_active_request_id.is_some() {
                    return;
                }
                let Some(mut built_request) = self.advanced_recognition_request.clone() else {
                    self.push_toast(
                        ctx,
                        "Не найдено выделение для распознавания.".to_string(),
                        Color32::RED,
                        2.6,
                    );
                    return;
                };
                let request_id = self.next_ocr_request_id;
                self.next_ocr_request_id = self.next_ocr_request_id.saturating_add(1);
                built_request.request.request_id = request_id;
                built_request.request.image_override_png = Some(image_override_png);
                self.advanced_recognition.set_request_running(request_id);
                self.dispatch_manual_ocr_request(
                    ctx,
                    built_request,
                    None,
                    ManualOcrResultTarget::ToastAndClipboard,
                );
                if self.manual_ocr_active_request_id != Some(request_id) {
                    let error = self
                        .ocr_controller
                        .last_error()
                        .unwrap_or("Не удалось запустить распознавание.")
                        .to_string();
                    let _ = self
                        .advanced_recognition
                        .apply_recognition_error(request_id, error.clone());
                    self.push_toast(ctx, format!("Быстрое OCR: {error}"), Color32::RED, 3.0);
                }
            }
            AdvancedRecognitionAction::CreateBubble {
                page_idx,
                uv_rect,
                text,
            } => {
                if !text.trim().is_empty()
                    && canvas
                        .create_bubble_with_original_text_at_page_uv_rect(page_idx, uv_rect, text)
                {
                    canvas.flush_pending_bubble_upserts_now(project);
                }
                self.advanced_recognition_request = None;
            }
            AdvancedRecognitionAction::Close => {
                self.advanced_recognition_request = None;
            }
        }
    }

    fn start_ocr_for_scene_rect(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        scene_rect: Rect,
        recent_character_rank: Option<usize>,
    ) {
        if !self.ai_enabled {
            self.push_toast(
                ctx,
                "OCR отключён флагом --no-ai.".to_string(),
                Color32::from_rgb(225, 180, 60),
                2.6,
            );
            return;
        }
        if ocr_engine_requires_backend_runtime(&self.ocr_panel_options)
            && self.ai_backend_unavailable()
        {
            self.push_toast(
                ctx,
                "ИИ бэкенд недоступен".to_string(),
                Color32::from_rgb(240, 102, 102),
                2.6,
            );
            return;
        }
        if let Some(error) = self.current_ocr_torch_requirement_error() {
            self.push_toast(ctx, error, Color32::RED, 2.6);
            return;
        }
        let request_id = self.next_ocr_request_id;
        self.next_ocr_request_id = self.next_ocr_request_id.saturating_add(1);

        let Some(built_request) = build_ocr_request(
            canvas,
            project,
            scene_rect,
            request_id,
            &self.ocr_panel_options,
        ) else {
            self.push_toast(
                ctx,
                "Выделение не пересекается со страницами.".to_string(),
                Color32::RED,
                2.6,
            );
            return;
        };

        let pending_insert = if self.ocr_panel_options.create_bubble {
            Some(OcrPendingBubbleInsert {
                page_idx: built_request.page_idx,
                uv_rect: built_request.request.uv_rect,
                join_newlines: built_request.request.join_newlines,
                recent_character_rank,
            })
        } else {
            None
        };

        if self.manual_ocr_active_request_id.is_some() {
            self.push_toast(
                ctx,
                "OCR уже выполняется".to_string(),
                Color32::from_rgb(255, 172, 66),
                2.0,
            );
            return;
        }

        self.dispatch_manual_ocr_request(
            ctx,
            built_request,
            pending_insert,
            ManualOcrResultTarget::AdvancedWindowText,
        );
    }
}

impl TranslationTabState {
    fn build_image_bubble_footer_controls(
        &mut self,
        ui: &mut egui::Ui,
        project: &ProjectData,
        bubble: &Bubble,
        now_s: f64,
    ) {
        let bubble_id = bubble.id;
        let mut source_type = bubble_extra_string(&bubble.extra, "image_source_type");
        if source_type.is_empty() {
            source_type = "external".to_string();
        }
        let before_source_type = source_type.clone();
        WheelComboBox::from_id_salt(("translation_footer_image_source_type", bubble_id))
            .selected_text(if source_type == "page_crop" {
                "Вырезка из ленты"
            } else {
                "Сторонняя картинка"
            })
            .width(170.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut source_type,
                    "page_crop".to_string(),
                    "Вырезка из ленты",
                );
                ui.selectable_value(
                    &mut source_type,
                    "external".to_string(),
                    "Сторонняя картинка",
                );
            });
        if source_type != before_source_type {
            self.queue_footer_patch(
                bubble_id,
                "image_source_type",
                Value::String(source_type.clone()),
                now_s,
            );
            if source_type == "page_crop" && !bubble.extra.contains_key("crop_rect") {
                self.queue_footer_patch(
                    bubble_id,
                    "crop_rect",
                    Value::Array(default_image_crop_rect_values(project, bubble)),
                    now_s,
                );
                self.queue_footer_patch(
                    bubble_id,
                    "crop_page_idx",
                    Value::Number(u64::try_from(bubble.img_idx).unwrap_or(u64::MAX).into()),
                    now_s,
                );
            }
        }

        if source_type == "external" {
            if ui.small_button("Вставить картинку из буфера").clicked() {
                match save_clipboard_image_bubble(project, bubble_id) {
                    Ok(path) => self.queue_footer_patch(
                        bubble_id,
                        "image_path",
                        Value::String(project_relative_path(project, &path)),
                        now_s,
                    ),
                    Err(err) => self.push_toast(ui.ctx(), err, Color32::RED, 3.0),
                }
            }
            if ui.small_button("Выбрать файл").clicked()
                && let Some(path) = FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp"])
                    .pick_file()
            {
                match copy_external_image_bubble(project, bubble_id, &path) {
                    Ok(saved) => self.queue_footer_patch(
                        bubble_id,
                        "image_path",
                        Value::String(project_relative_path(project, &saved)),
                        now_s,
                    ),
                    Err(err) => self.push_toast(ui.ctx(), err, Color32::RED, 3.0),
                }
            }
        }
    }
}

impl CanvasHooks for TranslationTabState {
    fn wants_canvas_shift_drag_selection(&self, ctx: &egui::Context) -> bool {
        if Self::image_crop_selection_mode_active(ctx) || self.image_crop_selection.is_some() {
            return true;
        }
        if !self.ai_enabled {
            return false;
        }
        if self.advanced_recognition.is_open() {
            return false;
        }
        self.ocr_quick_selection_mode_active()
            || self.ocr_advanced_selection_mode_active()
            || self.ocr_selection.is_some()
    }

    fn canvas_scrollbar_marks(&mut self, ctx: &CanvasScrollbarContext<'_>) -> Vec<ScrollMark> {
        // Per-bubble translation status painted on the canvas scrollbar:
        // red while a bubble's translation is empty, blue once it is filled.
        // The display mode is a user setting on the ribbon settings tab.
        let mode = ctx.translation_status_display();
        if mode == TranslationStatusDisplay::None {
            return Vec::new();
        }

        let bubbles = ctx.bubbles();
        let mut entries: Vec<(f32, bool)> = bubbles
            .iter()
            .filter_map(|bubble| {
                ctx.content_y(bubble.img_idx, bubble.img_v)
                    .map(|content_y| (content_y, !bubble.text.trim().is_empty()))
            })
            .collect();
        if entries.is_empty() {
            return Vec::new();
        }
        entries.sort_by(|a, b| a.0.total_cmp(&b.0));

        let mark_color = |translated: bool| {
            if translated {
                egui::Color32::from_rgb(40, 132, 255)
            } else {
                egui::Color32::from_rgb(220, 60, 60)
            }
        };

        if mode == TranslationStatusDisplay::Marks {
            // Thin fixed-height stripe at each bubble's own position; it does not
            // stretch down to the next bubble.
            const MARK_HALF_HEIGHT_PX: f32 = 1.0;
            return entries
                .into_iter()
                .map(|(start, translated)| {
                    let color = mark_color(translated);
                    ScrollMark::custom(ScrollSpan::pixel_at(start), move |painter, _geom, cell| {
                        let center_y = cell.center().y;
                        let rect = egui::Rect::from_min_max(
                            egui::pos2(cell.left(), center_y - MARK_HALF_HEIGHT_PX),
                            egui::pos2(cell.right(), center_y + MARK_HALF_HEIGHT_PX),
                        );
                        painter.rect_filled(rect, 0.0, color);
                    })
                })
                .collect();
        }

        // TranslationStatusDisplay::UntilNext: each bubble paints a stripe from
        // itself down to the next bubble vertically.
        let content_end = ctx.content_size_y();
        // The last bubble has no following bubble, so its mark would otherwise run
        // to the very end of the content. Instead, let it extend just a couple
        // percent of the scrollbar past the bubble, leaving the rest unpainted.
        let tail_len = content_end * 0.02;
        let mut marks = Vec::with_capacity(entries.len());
        for index in 0..entries.len() {
            let (start, translated) = entries[index];
            // If another bubble follows, the mark runs down to it. The bottom-most
            // bubble's mark gets only a short tail instead of reaching the end.
            let end = entries
                .get(index + 1)
                .map_or_else(|| (start + tail_len).min(content_end), |next| next.0);
            if end <= start {
                continue;
            }
            marks.push(ScrollMark::fill(
                ScrollSpan::ContentPixels { start, end },
                MarkFill::Solid(mark_color(translated)),
            ));
        }
        marks
    }

    fn draw_canvas_mask_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) {
        self.draw_text_detector_mask_layer_on_page(ui, ctx, page_idx, image_rect, zoom);
    }

    fn draw_canvas_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        _zoom: f32,
    ) {
        self.draw_text_detector_additional_overlay_on_page(ui, ctx, page_idx, image_rect);
    }

    fn draw_canvas_overlay_top_left(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: egui::Rect,
        canvas: &mut CanvasView,
        project: &ProjectData,
        status: CanvasUiStatus,
    ) {
        let _ = (
            &project.image_dir,
            status.loaded_pages,
            status.total_pages,
            status.load_errors_count,
        );
        let now_s = ctx.input(|i| i.time);
        self.ensure_character_names_loaded(project);
        self.maybe_refresh_character_names_by_watch(project, now_s);
        if self.pending_characters_refresh {
            self.reload_character_names(project);
            self.pending_characters_refresh = false;
        }
        self.sync_footer_tracking(canvas, project);
        self.ensure_ocr_settings_loaded(project);
        self.ensure_mt_settings_loaded(project);
        self.ensure_composition_settings_loaded(project);
        self.ensure_text_detector_settings_loaded(project);
        self.flush_settings_save_if_needed(project);

        self.poll_text_detector_events(ctx);
        self.poll_text_detection_storage_events(project);
        self.poll_ocr_events(ctx, canvas, project);
        self.poll_mt_events(ctx, canvas);
        self.handle_image_bubble_hotkeys(ctx, canvas, project);
        self.handle_image_crop_selection(ctx, canvas_rect, canvas, project);
        self.handle_ocr_selection(ctx, canvas_rect, canvas, project);
        self.handle_advanced_recognition_window(ctx, canvas, project);
        self.maybe_dispatch_next_textdetector_ocr_request(ctx, project);
        self.handle_pending_mt_actions(ctx, canvas, project);
        self.flush_footer_patches(canvas, project, now_s);
        self.bubbles_panel.flush_text_updates(canvas, now_s);
        self.handle_text_detector_line_edit_hotkeys(ctx, canvas, project);
        self.draw_recent_character_cards(ctx, canvas_rect);

        egui::Area::new("translation_canvas_open_buttons".into())
            .fixed_pos(canvas_rect.right_bottom() + egui::vec2(0.0, -40.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    // Заголовок по центру
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("Инструменты").strong());
                    });

                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(self.open_button_label(TranslationPanel::Bubbles))
                            .clicked()
                        {
                            self.toggle_panel(TranslationPanel::Bubbles);
                        }
                        if ui
                            .button(self.open_button_label(TranslationPanel::Ocr))
                            .clicked()
                        {
                            self.toggle_panel(TranslationPanel::Ocr);
                        }
                        if ui
                            .button(self.open_button_label(TranslationPanel::Composition))
                            .clicked()
                        {
                            self.toggle_panel(TranslationPanel::Composition);
                        }
                        if ui
                            .button(self.open_button_label(TranslationPanel::MachineTranslation))
                            .clicked()
                        {
                            self.toggle_panel(TranslationPanel::MachineTranslation);
                        }
                        if ui
                            .button(self.open_button_label(TranslationPanel::TextDetector))
                            .clicked()
                        {
                            self.toggle_panel(TranslationPanel::TextDetector);
                        }
                    });
                });
            });

        let show_startup_load_toast =
            status.total_pages > 0 && status.loaded_pages < status.total_pages;
        let toast_top_offset = if show_startup_load_toast { 96.0 } else { 16.0 };
        self.draw_startup_page_load_toast(ctx, canvas_rect, status);
        self.draw_toast(ctx, canvas_rect, toast_top_offset);
        if self.text_detector_edit_lines_mode {
            let panel_pos = canvas
                .canvas_left_top_controls_rect()
                .map(|rect| rect.left_bottom() + egui::vec2(0.0, 8.0))
                .unwrap_or_else(|| canvas_rect.left_top() + egui::vec2(12.0, 112.0));
            egui::Area::new("translation_text_detector_line_edit_panel".into())
                .fixed_pos(panel_pos)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(Color32::from_rgb(26, 88, 40))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(88, 180, 110)))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("Режим изменения найденных строк")
                                    .color(Color32::from_rgb(210, 255, 220))
                                    .strong(),
                            );
                            if ui.button("Выйти").clicked() {
                                self.set_text_detector_edit_lines_mode(false);
                            }
                        });
                });
        }
        if self.text_detector_edit_mask_mode {
            let panel_pos = canvas
                .canvas_left_top_controls_rect()
                .map(|rect| rect.left_bottom() + egui::vec2(0.0, 8.0))
                .unwrap_or_else(|| canvas_rect.left_top() + egui::vec2(12.0, 112.0));
            egui::Area::new("translation_text_detector_mask_edit_panel".into())
                .fixed_pos(panel_pos)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(Color32::from_rgb(104, 28, 28))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(212, 72, 72)))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("Режим изменения маски текста")
                                    .color(Color32::from_rgb(255, 220, 220))
                                    .strong(),
                            );
                            let mut radius = self.text_detector_mask_brush.radius_px();
                            if ui
                                .add(WheelSlider::new(&mut radius, 1..=200).text("Кисть (px)"))
                                .changed()
                            {
                                self.text_detector_mask_brush.set_radius_px(radius);
                            }
                            ui.small("ЛКМ: рисовать");
                            ui.small("ПКМ или Shift+ЛКМ: стирать");
                            if ui.button("Выйти").clicked() {
                                self.set_text_detector_edit_mask_mode(false);
                            }
                        });
                });
        }
        if self.ai_enabled && self.active_panel == TranslationPanel::Ocr {
            ctx.request_repaint_after(Duration::from_millis(350));
        }
        if self.ocr_quick_selection_mode_active()
            || self.ocr_advanced_selection_mode_active()
            || self.ocr_selection.is_some()
            || self.image_crop_selection.is_some()
            || self.advanced_recognition.is_open()
            || self.ocr_toast.is_some()
            || self.ocr_controller.state().is_busy()
            || self.mt_controller.is_busy()
            || self.pending_mt_start_all
            || self.pending_mt_start_page
            || !self.pending_translate_actions.is_empty()
            || self.text_detector_controller.is_busy()
            || self.text_detection_storage_busy
            || self.textdetector_ocr_is_running()
            // Detector edit modes are NOT a repaint trigger on their own: an idle
            // edit mode with no active gesture has nothing to animate, and egui
            // already repaints on pointer movement (brush cursor / hover). Only a
            // live gesture needs forced frames — an in-progress mask brush stroke or
            // an active line drag — so painted pixels keep up between pointer events.
            || self.text_detector_mask_stroke_state.is_some()
            || self.text_detector_line_drag_state.is_some()
            || !self.pending_footer_patches.is_empty()
            || self.bubbles_panel.has_pending_text_updates()
            || (self.active_panel == TranslationPanel::Composition
                && self.composition_rebuild_requested)
        {
            ctx.request_repaint();
        }
    }

    fn build_bubble_header(&mut self, _ui: &mut egui::Ui, _bubble: &Bubble, _editable: bool) {}

    fn build_bubble_footer(
        &mut self,
        ui: &mut egui::Ui,
        project: &ProjectData,
        bubble: &Bubble,
        editable: bool,
    ) {
        if !editable {
            return;
        }
        let bubble_id = bubble.id;
        let now_s = ui.ctx().input(|i| i.time);
        let mut state = self.footer_state_for(bubble);

        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let mut bubble_order = state.bubble_order;
            let order_resp = ui
                .add(
                    WheelSpinBox::new(&mut bubble_order)
                        .range(0..=100_000)
                        .speed(0.25),
                )
                .on_hover_text("Номер реплики для упорядочивания");
            if order_resp.changed() {
                state.bubble_order = bubble_order.clamp(0, 100_000);
                self.queue_footer_patch(
                    bubble_id,
                    "bubble_order",
                    Value::Number(state.bubble_order.into()),
                    now_s,
                );
            }

            if bubble.bubble_class.as_deref().map(BubbleClass::from_str) == Some(BubbleClass::Image)
            {
                self.build_image_bubble_footer_controls(ui, project, bubble, now_s);
                return;
            }

            // Borrow `character_names` as a slice instead of cloning the whole Vec
            // every frame for every edited bubble. `character_names` and
            // `footer_character_autocomplete` are disjoint fields, so the immutable
            // suggestions borrow coexists with the mutable autocomplete entry borrow;
            // it ends at the `draw` call, before the later `queue_footer_patch` writes.
            let suggestions: &[String] = &self.character_names;
            let autocomplete = self
                .footer_character_autocomplete
                .entry(bubble_id)
                .or_insert_with(|| {
                    AutocompleteLine::new(("translation_footer_character", bubble_id))
                });
            autocomplete.set_max_suggestions(FOOTER_CHARACTER_AUTOCOMPLETE_MAX);
            autocomplete.set_hint_text("Кто говорит?");

            let field_resp = autocomplete.draw(ui, &mut state.character_name, suggestions);
            if field_resp.changed || field_resp.submitted {
                if !state.is_known_character {
                    state.is_known_character = true;
                    self.queue_footer_patch(
                        bubble_id,
                        "is_known_character",
                        Value::Bool(true),
                        now_s,
                    );
                }
                self.queue_footer_patch(
                    bubble_id,
                    "character_name",
                    Value::String(state.character_name.clone()),
                    now_s,
                );
                self.last_is_known_character = true;
                self.last_character_name = state.character_name.clone();
                self.last_clarification = state.clarification.clone();
            }
        });

        self.footer_overrides.insert(bubble_id, state);
    }

    fn bubble_status_style(
        &mut self,
        bubble: &Bubble,
        editable: bool,
        canvas: &CanvasView,
    ) -> Option<BubbleBorderStyle> {
        if !editable {
            return None;
        }
        evaluate_bubble_status_rules(
            &canvas.state.bubble_status_rules,
            BubbleStatusContext {
                translation_filled: !bubble.text.trim().is_empty(),
                original_filled: !bubble.original_text.trim().is_empty(),
                character_filled: self.bubble_has_character_for_status(bubble),
            },
        )
    }

    fn on_bubble_action(&mut self, action: BubbleAction, bubble_id: i64) {
        if action == BubbleAction::Translate {
            self.pending_translate_actions.push(bubble_id);
        }
    }

    fn draw_canvas_page_context_menu(
        &mut self,
        ui: &mut egui::Ui,
        project: &ProjectData,
        page_idx: usize,
        page_uv: Pos2,
    ) -> bool {
        if self.text_detector_edit_mask_mode {
            return true;
        }
        if !self.text_detector_edit_lines_mode {
            return false;
        }
        if ui.button("Создать строку").clicked() {
            let _ = self.create_text_detector_line_at_uv(project, page_idx, page_uv);
            ui.close();
        }
        true
    }

    fn suppress_canvas_page_context_menu(&self, _page_idx: usize) -> bool {
        self.text_detector_edit_mask_mode
    }
}

impl TranslationTabState {
    fn rebuild_composition_text(&mut self, project: &ProjectData) {
        self.composition_panel_options.normalize();
        self.composition_panel_state.composed_text =
            compose_translation_text(project, &self.composition_panel_options);
        self.composition_rebuild_requested = false;
    }

    fn ensure_ocr_settings_loaded(&mut self, project: &ProjectData) {
        let settings_path = project.paths.settings_file.clone();
        if self
            .ocr_settings_loaded_for
            .as_ref()
            .is_some_and(|loaded| *loaded == settings_path)
        {
            return;
        }

        let ocr = project.settings_data.get("OCR").and_then(Value::as_object);
        if let Some(ocr_obj) = ocr {
            if let Some(engine_raw) = ocr_obj.get("engine").and_then(Value::as_str) {
                self.ocr_panel_options.engine = parse_ocr_engine_key(engine_raw);
            }
            if let Some(join) = ocr_obj.get("join").and_then(Value::as_bool) {
                self.ocr_panel_options.join_newlines = join;
            }
            if let Some(reflect) = ocr_obj.get("reflect").and_then(Value::as_bool) {
                self.ocr_panel_options.reflect_strings = reflect;
            }
            if let Some(copy) = ocr_obj.get("copy").and_then(Value::as_bool) {
                self.ocr_panel_options.copy_to_clipboard = copy;
            }
            if let Some(bubbles) = ocr_obj.get("bubbles").and_then(Value::as_bool) {
                self.ocr_panel_options.create_bubble = bubbles;
            }
            if let Some(replace_chars) = ocr_obj.get("replace_chars").and_then(Value::as_bool) {
                self.ocr_panel_options.replace_chars_enabled = replace_chars;
            }
            if let Some(rules) = ocr_obj.get("char_replacements").and_then(Value::as_array) {
                self.ocr_panel_options.char_replacements = parse_char_replacement_rules(rules);
            }

            if let Some(params) = ocr_obj.get("params").and_then(Value::as_object) {
                let manga_obj = params
                    .get("mangaocr")
                    .or_else(|| params.get("manga"))
                    .and_then(Value::as_object);
                if let Some(manga) = manga_obj
                    && let Some(model) = manga.get("model").and_then(Value::as_str)
                {
                    let trimmed = model.trim();
                    if !trimmed.is_empty() {
                        self.ocr_panel_options.manga_model = trimmed.to_string();
                    }
                }
                let paddle_obj = params
                    .get("paddle")
                    .or_else(|| params.get("paddleocr"))
                    .and_then(Value::as_object);
                if let Some(paddle) = paddle_obj {
                    if let Some(lang) = parse_single_ocr_lang_setting(paddle.get("langs")) {
                        self.ocr_panel_options.paddle_lang = lang;
                    }
                    if let Some(show_full) = paddle.get("full_langs").and_then(Value::as_bool) {
                        self.ocr_panel_options.paddle_show_full_langs = show_full;
                    }
                }
                let paddle_vl_obj = params
                    .get("paddle_vl")
                    .or_else(|| params.get("paddleocrvl"))
                    .and_then(Value::as_object);
                if let Some(paddle_vl) = paddle_vl_obj
                    && let Some(script) = paddle_vl.get("script").and_then(Value::as_str)
                {
                    let trimmed = script.trim();
                    if !trimmed.is_empty() {
                        self.ocr_panel_options.paddle_vl_script = trimmed.to_ascii_lowercase();
                    }
                }
                let easy_obj = params
                    .get("easyocr")
                    .or_else(|| params.get("easy"))
                    .and_then(Value::as_object);
                if let Some(easy) = easy_obj {
                    if let Some(lang_text) = parse_ocr_lang_text_setting(easy.get("langs")) {
                        self.ocr_panel_options.easy_langs = lang_text;
                    }
                    if let Some(show_full) = easy.get("full_langs").and_then(Value::as_bool) {
                        self.ocr_panel_options.easy_show_full_langs = show_full;
                    }
                }
                let surya_obj = params
                    .get("surya")
                    .or_else(|| params.get("suryaocr"))
                    .and_then(Value::as_object);
                if let Some(surya) = surya_obj {
                    if let Some(task_name) = surya.get("task").and_then(Value::as_str) {
                        let trimmed = task_name.trim();
                        if !trimmed.is_empty() {
                            self.ocr_panel_options.surya_task_name = trimmed.to_string();
                        }
                    }
                    if let Some(recognize_math) =
                        surya.get("recognize_math").and_then(Value::as_bool)
                    {
                        self.ocr_panel_options.surya_recognize_math = recognize_math;
                    }
                    if let Some(sort_lines) = surya.get("sort_lines").and_then(Value::as_bool) {
                        self.ocr_panel_options.surya_sort_lines = sort_lines;
                    }
                    if let Some(drop_repeated_text) =
                        surya.get("drop_repeated_text").and_then(Value::as_bool)
                    {
                        self.ocr_panel_options.surya_drop_repeated_text = drop_repeated_text;
                    }
                    if let Some(max_sliding_window) =
                        surya.get("max_sliding_window").and_then(Value::as_u64)
                        && let Ok(value) = u32::try_from(max_sliding_window)
                    {
                        self.ocr_panel_options.surya_max_sliding_window = value;
                    }
                    if let Some(max_tokens) = surya.get("max_tokens").and_then(Value::as_u64)
                        && let Ok(value) = u32::try_from(max_tokens)
                    {
                        self.ocr_panel_options.surya_max_tokens = value;
                    }
                }
                let ai_api_obj = params
                    .get("ai_api")
                    .or_else(|| params.get("aiapi"))
                    .and_then(Value::as_object);
                if let Some(ai_api) = ai_api_obj {
                    if let Some(service) = ai_api.get("service").and_then(Value::as_str) {
                        self.ocr_panel_options.ai_api_service = AiApiService::from_key(service);
                    }
                    if let Some(model) = ai_api.get("model").and_then(Value::as_str) {
                        let trimmed = model.trim();
                        if !trimmed.is_empty() {
                            self.ocr_panel_options.ai_api_model = trimmed.to_string();
                        }
                    }
                    if let Some(system_instruction) =
                        ai_api.get("system_instruction").and_then(Value::as_str)
                    {
                        self.ocr_panel_options.ai_api_system_instruction =
                            system_instruction.to_string();
                    }
                }
            }
        }

        self.ocr_settings_loaded_for = Some(settings_path);
        self.ocr_settings_dirty = false;
    }

    fn ensure_mt_settings_loaded(&mut self, project: &ProjectData) {
        let settings_path = project.paths.settings_file.clone();
        if self
            .mt_settings_loaded_for
            .as_ref()
            .is_some_and(|loaded| *loaded == settings_path)
        {
            return;
        }

        if let Some(mt_obj) = project
            .settings_data
            .get("machine_translation")
            .and_then(Value::as_object)
        {
            if let Some(service_key) = mt_obj.get("service").and_then(Value::as_str)
                && let Some(service) = MtService::from_key(service_key)
            {
                self.mt_panel_options.service = service;
            }
            if let Some(source_lang) = mt_obj.get("source_lang").and_then(Value::as_str) {
                self.mt_panel_options.source_lang = source_lang.to_string();
            }
            if let Some(target_lang) = mt_obj.get("target_lang").and_then(Value::as_str) {
                self.mt_panel_options.target_lang = target_lang.to_string();
            }
            if let Some(active_tab) = mt_obj.get("active_tab").and_then(Value::as_str) {
                self.mt_panel_options.active_tab = if active_tab.eq_ignore_ascii_case("ai_api") {
                    MtPanelTab::AiApi
                } else {
                    MtPanelTab::Machine
                };
            }
            if let Some(ai_obj) = mt_obj.get("ai_api").and_then(Value::as_object) {
                if let Some(service) = ai_obj.get("service").and_then(Value::as_str) {
                    self.mt_panel_options.ai_api_service = AiApiService::from_key(service);
                }
                if let Some(model) = ai_obj.get("model").and_then(Value::as_str)
                    && !model.trim().is_empty()
                {
                    self.mt_panel_options.ai_api_model = model.trim().to_string();
                }
                if let Some(system_instruction) =
                    ai_obj.get("system_instruction").and_then(Value::as_str)
                {
                    self.mt_panel_options.ai_api_system_instruction =
                        system_instruction.to_string();
                }
                if let Some(sort_mode) = ai_obj.get("sort_mode").and_then(Value::as_str) {
                    self.mt_panel_options.ai_sort_mode = AiMtSortMode::from_key(sort_mode);
                }
                if let Some(value) = ai_obj.get("use_character_names").and_then(Value::as_bool) {
                    self.mt_panel_options.ai_use_character_names = value;
                }
                if let Some(value) = ai_obj.get("use_notes_prompt").and_then(Value::as_bool) {
                    self.mt_panel_options.ai_use_notes_prompt = value;
                }
                if let Some(value) = ai_obj.get("include_characters").and_then(Value::as_bool) {
                    self.mt_panel_options.ai_include_characters = value;
                }
                if let Some(value) = ai_obj.get("include_terms").and_then(Value::as_bool) {
                    self.mt_panel_options.ai_include_terms = value;
                }
                if let Some(value) = ai_obj
                    .get("include_existing_translation")
                    .and_then(Value::as_bool)
                {
                    self.mt_panel_options.ai_include_existing_translation = value;
                }
                if let Some(value) = ai_obj.get("include_image_bubbles").and_then(Value::as_bool) {
                    self.mt_panel_options.ai_include_image_bubbles = value;
                }
                if let Some(value) = ai_obj.get("image_detail").and_then(Value::as_str) {
                    self.mt_panel_options.ai_image_detail = AiMtImageDetail::from_key(value);
                } else if let Some(value) =
                    ai_obj.get("image_encoding_quality").and_then(Value::as_str)
                {
                    self.mt_panel_options.ai_image_detail = AiMtImageDetail::from_key(value);
                }
                if let Some(value) = ai_obj.get("image_mode").and_then(Value::as_str) {
                    self.mt_panel_options.ai_image_mode = AiMtImageMode::from_key(value);
                }
                if let Some(value) = ai_obj.get("image_context_source").and_then(Value::as_str) {
                    self.mt_panel_options.ai_image_context_source =
                        AiMtContextSource::from_key(value);
                }
                if let Some(value) = ai_obj.get("batch_size").and_then(Value::as_u64)
                    && let Ok(value) = usize::try_from(value)
                {
                    self.mt_panel_options.ai_batch_size = value.clamp(1, 100);
                }
                if let Some(value) = ai_obj.get("reasoning").and_then(Value::as_str) {
                    self.mt_panel_options.ai_reasoning = AiMtReasoning::from_key(value);
                }
                if let Some(value) = ai_obj.get("context_limit_percent").and_then(Value::as_u64)
                    && let Ok(value) = u8::try_from(value)
                {
                    self.mt_panel_options.ai_context_limit_percent = value.clamp(10, 100);
                }
            }
        }

        self.mt_panel_options.source_lang =
            normalized_lang_input(&self.mt_panel_options.source_lang, "auto");
        self.mt_panel_options.target_lang =
            normalized_lang_input(&self.mt_panel_options.target_lang, "ru");
        self.mt_settings_loaded_for = Some(settings_path);
        self.mt_settings_dirty = false;
    }

    fn ensure_composition_settings_loaded(&mut self, project: &ProjectData) {
        let settings_path = project.paths.settings_file.clone();
        if self
            .composition_settings_loaded_for
            .as_ref()
            .is_some_and(|loaded| *loaded == settings_path)
        {
            return;
        }

        if let Some(comp_obj) = project
            .settings_data
            .get("composition")
            .and_then(Value::as_object)
        {
            if let Some(raw_method) = comp_obj.get("method").and_then(Value::as_str) {
                self.composition_panel_options.sort_method =
                    CompositionSortMethod::from_key(raw_method);
            }
            if let Some(raw_source) = comp_obj.get("source_mode").and_then(Value::as_str) {
                self.composition_panel_options.source_mode =
                    CompositionSourceMode::from_key(raw_source);
            }
            if let Some(value) = comp_obj
                .get("ignore_translated_lines")
                .and_then(Value::as_bool)
            {
                self.composition_panel_options.ignore_translated_lines = value;
            }
            if let Some(value) = comp_obj
                .get("merge_same_character")
                .and_then(Value::as_bool)
            {
                self.composition_panel_options.merge_same_character = value;
            }
            if let Some(value) = comp_obj.get("sep_same_character").and_then(Value::as_str) {
                self.composition_panel_options.sep_same_character = value.to_string();
            }
            if let Some(value) = comp_obj.get("sep_between").and_then(Value::as_str) {
                self.composition_panel_options.sep_between = value.to_string();
            }
            if let Some(value) = comp_obj.get("replica_prefix").and_then(Value::as_str) {
                self.composition_panel_options.replica_prefix = value.to_string();
            }
            if let Some(value) = comp_obj.get("nl_replace").and_then(Value::as_str) {
                self.composition_panel_options.nl_replace = value.to_string();
            }
            if let Some(value) = comp_obj.get("nl_replace_enabled").and_then(Value::as_bool) {
                self.composition_panel_options.nl_replace_enabled = value;
            }
            if let Some(value) = comp_obj.get("wrap_with").and_then(Value::as_str) {
                self.composition_panel_options.wrap_with = normalize_wrap_with(value);
            }
            if let Some(value) = comp_obj.get("wrap_with_enabled").and_then(Value::as_bool) {
                self.composition_panel_options.wrap_with_enabled = value;
            }
            if let Some(value) = comp_obj.get("limit").and_then(Value::as_u64) {
                self.composition_panel_options.limit = value as usize;
            }
            if let Some(value) = comp_obj.get("limit_enabled").and_then(Value::as_bool) {
                self.composition_panel_options.limit_enabled = value;
            }
            if let Some(value) = comp_obj.get("use_character_names").and_then(Value::as_bool) {
                self.composition_panel_options.use_character_names = value;
            }
            if let Some(value) = comp_obj
                .get("include_image_bubbles")
                .and_then(Value::as_bool)
            {
                self.composition_panel_options.include_image_bubbles = value;
            }
            if let Some(value) = comp_obj.get("jinja2_enabled").and_then(Value::as_bool) {
                self.composition_panel_options.jinja2_enabled = value;
            }
            if let Some(value) = comp_obj.get("jinja2_template").and_then(Value::as_str) {
                self.composition_panel_options.jinja2_template = value.to_string();
            }
        }

        self.composition_panel_options.normalize();
        self.composition_settings_loaded_for = Some(settings_path);
        self.composition_settings_dirty = false;
        self.composition_rebuild_requested = true;
    }

    fn ensure_text_detector_settings_loaded(&mut self, project: &ProjectData) {
        let settings_path = project.paths.settings_file.clone();
        if self
            .text_detector_settings_loaded_for
            .as_ref()
            .is_some_and(|loaded| *loaded == settings_path)
        {
            return;
        }

        if let Some(det_obj) = project
            .settings_data
            .get("text_detector")
            .and_then(Value::as_object)
        {
            if let Some(value) = det_obj.get("algorithm").and_then(Value::as_str) {
                self.text_detector_options.algorithm = parse_text_detector_algorithm_key(value);
            }
            if let Some(value) = det_obj.get("draw_lines").and_then(Value::as_bool) {
                self.text_detector_options.draw_lines = value;
            }
            if let Some(value) = det_obj.get("draw_mask").and_then(Value::as_bool) {
                self.text_detector_options.draw_mask = value;
            }
            if let Some(value) = det_obj.get("block_expand_px").and_then(Value::as_i64) {
                self.text_detector_options.block_expand_px = (value as i32).clamp(0, 200);
            }
            if let Some(value) = det_obj.get("merge_gap_px").and_then(Value::as_i64) {
                self.text_detector_options.merge_gap_px = (value as i32).clamp(0, 200);
            }
            if let Some(value) = det_obj.get("mask_dilate_size").and_then(Value::as_i64) {
                self.text_detector_options.mask_dilate_size = (value as i32).clamp(0, 30);
            }
            if let Some(params_obj) = det_obj.get("params").and_then(Value::as_object) {
                if let Some(value) = params_obj.get("detect_size").and_then(Value::as_i64) {
                    self.text_detector_options.ai_detect_size = (value as i32).clamp(896, 2048);
                }
                if let Some(value) = params_obj
                    .get("det_rearrange_max_batches")
                    .and_then(Value::as_i64)
                {
                    self.text_detector_options.ai_det_rearrange_max_batches =
                        (value as i32).clamp(1, 64);
                }
                if let Some(value) = params_obj
                    .get("font size multiplier")
                    .and_then(Value::as_f64)
                {
                    self.text_detector_options.ai_font_size_multiplier =
                        (value as f32).clamp(0.1, 8.0);
                }
                if let Some(value) = params_obj.get("font size max").and_then(Value::as_f64) {
                    self.text_detector_options.ai_font_size_max = (value as f32).clamp(-1.0, 500.0);
                }
                if let Some(value) = params_obj.get("font size min").and_then(Value::as_f64) {
                    self.text_detector_options.ai_font_size_min = (value as f32).clamp(-1.0, 500.0);
                }
                if let Some(value) = params_obj.get("mask dilate size").and_then(Value::as_i64)
                    && det_obj.get("mask_dilate_size").is_none()
                {
                    self.text_detector_options.mask_dilate_size = (value as i32).clamp(0, 30);
                }
            }
        }

        self.text_detector_settings_loaded_for = Some(settings_path);
        self.text_detector_settings_dirty = false;
    }

    fn flush_settings_save_if_needed(&mut self, project: &ProjectData) {
        if !self.ocr_settings_dirty
            && !self.mt_settings_dirty
            && !self.composition_settings_dirty
            && !self.text_detector_settings_dirty
        {
            return;
        }
        let settings_file = project.paths.settings_file.clone();
        let request = TranslationSettingsSaveRequest {
            settings_file,
            ocr_options: self.ocr_panel_options.clone(),
            mt_options: self.mt_panel_options.clone(),
            composition_options: self.composition_panel_options.clone(),
            text_detector_options: self.text_detector_options.clone(),
        };
        if self.settings_save_tx.send(request).is_ok() {
            self.ocr_settings_dirty = false;
            self.mt_settings_dirty = false;
            self.composition_settings_dirty = false;
            self.text_detector_settings_dirty = false;
        }
    }
}

fn build_translation_character_names(base: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push_unique = |raw: &str| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        if seen.insert(trimmed.to_lowercase()) {
            out.push(trimmed.to_string());
        }
    };

    push_unique(FOOTER_NO_CHARACTER);
    for name in base {
        push_unique(&name);
    }
    for name in FOOTER_ADDITIONAL_CHARACTER_NAMES {
        push_unique(name);
    }
    out
}

fn recent_character_entry_from_state(state: &BubbleFooterState) -> Option<RecentCharacterEntry> {
    let trimmed = state.character_name.trim();
    if trimmed.is_empty() || trimmed == FOOTER_NO_CHARACTER || trimmed == FOOTER_NO_CHARACTERS {
        return None;
    }
    Some(RecentCharacterEntry {
        is_known_character: state.is_known_character,
        character_name: trimmed.to_string(),
        clarification: state.clarification.clone(),
    })
}

/// Decides whether `sync_footer_tracking` must run a full recompute this frame.
///
/// `cached` is the revision the last recompute was performed for (`None` before the first
/// recompute), `current` is the live `CanvasView::hook_bubbles_revision()` fingerprint, and
/// `bootstrapped` is whether footer tracking has already been initialized. Returns `true` when not
/// yet bootstrapped (the first frame must always recompute and bootstrap) or when the revision
/// differs from the cached one (the bubble set changed); returns `false` only when bootstrapped and
/// the revision is unchanged, so the cached footer state can be reused.
#[must_use]
fn footer_tracking_should_recompute(cached: Option<u64>, current: u64, bootstrapped: bool) -> bool {
    if !bootstrapped {
        return true;
    }
    cached != Some(current)
}

fn collect_recent_character_history(bubbles: &[Bubble]) -> VecDeque<RecentCharacterEntry> {
    let mut recent: VecDeque<RecentCharacterEntry> = VecDeque::new();
    let mut ordered = bubbles.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|bubble| bubble.id);
    for bubble in ordered.into_iter().rev() {
        let state = bubble_footer_state_from_record(bubble);
        let Some(entry) = recent_character_entry_from_state(&state) else {
            continue;
        };
        if recent
            .iter()
            .any(|item: &RecentCharacterEntry| item.character_name == entry.character_name)
        {
            continue;
        }
        recent.push_back(entry);
        if recent.len() >= RECENT_CHARACTER_HISTORY_LIMIT {
            break;
        }
    }
    recent
}

fn recent_character_rank_from_input(ctx: &egui::Context) -> Option<usize> {
    ctx.input(|input| {
        let from_pressed_event = input.events.iter().find_map(|event| match event {
            egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                repeat: false,
                ..
            } => physical_key
                .and_then(recent_character_rank_from_digit_key)
                .or_else(|| recent_character_rank_from_digit_key(*key)),
            egui::Event::Text(text) => recent_character_rank_from_text(text, input.modifiers.shift),
            _ => None,
        });
        from_pressed_event.or_else(|| {
            [
                egui::Key::Num1,
                egui::Key::Num2,
                egui::Key::Num3,
                egui::Key::Num4,
                egui::Key::Num5,
                egui::Key::Num6,
            ]
            .iter()
            .position(|key| input.key_down(*key))
            .map(|idx| idx + 1)
        })
    })
}

fn recent_character_rank_from_digit_key(key: egui::Key) -> Option<usize> {
    match key {
        egui::Key::Num1 => Some(1),
        egui::Key::Num2 => Some(2),
        egui::Key::Num3 => Some(3),
        egui::Key::Num4 => Some(4),
        egui::Key::Num5 => Some(5),
        egui::Key::Num6 => Some(6),
        _ => None,
    }
}

fn recent_character_rank_from_text(text: &str, shift_down: bool) -> Option<usize> {
    match text {
        "1" => Some(1),
        "2" => Some(2),
        "3" => Some(3),
        "4" => Some(4),
        "5" => Some(5),
        "6" => Some(6),
        "!" if shift_down => Some(1),
        "@" | "\"" if shift_down => Some(2),
        "#" | "№" if shift_down => Some(3),
        "$" | ";" if shift_down => Some(4),
        "%" if shift_down => Some(5),
        "^" | ":" if shift_down => Some(6),
        _ => None,
    }
}

fn characters_file_mtime(project: &ProjectData) -> Option<SystemTime> {
    let path = project.paths.characters_dir.join("characters.json");
    fs::metadata(path).ok()?.modified().ok()
}

fn detector_blocks_with_options(
    result: &TextDetectorPageResult,
    options: &TextDetectorPanelOptions,
) -> Vec<TextDetectorRect> {
    let source_w = result.source_size[0].max(1) as f32;
    let source_h = result.source_size[1].max(1) as f32;
    let expanded =
        detector_expand_blocks(&result.blocks, options.block_expand_px, source_w, source_h);
    detector_merge_blocks(&expanded, options.merge_gap_px)
}

fn detector_expand_blocks(
    blocks: &[TextDetectorRect],
    expand_px: i32,
    source_w: f32,
    source_h: f32,
) -> Vec<TextDetectorRect> {
    let exp = expand_px.max(0) as f32;
    let mut out = Vec::with_capacity(blocks.len());
    for rect in blocks {
        let x1 = (rect.x1 - exp).clamp(0.0, source_w);
        let y1 = (rect.y1 - exp).clamp(0.0, source_h);
        let x2 = (rect.x2 + exp).clamp(0.0, source_w);
        let y2 = (rect.y2 + exp).clamp(0.0, source_h);
        if let Some(next) = TextDetectorRect::from_xyxy(x1, y1, x2, y2) {
            out.push(next);
        }
    }
    out
}

fn detector_merge_blocks(blocks: &[TextDetectorRect], merge_gap_px: i32) -> Vec<TextDetectorRect> {
    let gap = merge_gap_px.max(0) as f32;
    let mut merged = Vec::<TextDetectorRect>::new();
    for rect in blocks {
        let mut cur = *rect;
        let mut i = 0usize;
        while i < merged.len() {
            if detector_rects_touch_or_near(cur, merged[i], gap) {
                let next = TextDetectorRect::from_xyxy(
                    cur.x1.min(merged[i].x1),
                    cur.y1.min(merged[i].y1),
                    cur.x2.max(merged[i].x2),
                    cur.y2.max(merged[i].y2),
                );
                merged.swap_remove(i);
                if let Some(value) = next {
                    cur = value;
                    continue;
                }
            }
            i += 1;
        }
        merged.push(cur);
    }

    merged.sort_by(|a, b| {
        a.y1.total_cmp(&b.y1)
            .then_with(|| a.x1.total_cmp(&b.x1))
            .then_with(|| a.y2.total_cmp(&b.y2))
            .then_with(|| a.x2.total_cmp(&b.x2))
    });
    merged
}

fn detector_rects_touch_or_near(a: TextDetectorRect, b: TextDetectorRect, gap: f32) -> bool {
    !(a.x2 + gap < b.x1 || a.x1 - gap > b.x2 || a.y2 + gap < b.y1 || a.y1 - gap > b.y2)
}

fn source_rect_to_scene_rect(
    page_rect: Rect,
    source_w: f32,
    source_h: f32,
    src: TextDetectorRect,
) -> Option<Rect> {
    if source_w <= 0.0 || source_h <= 0.0 {
        return None;
    }
    let x1 = page_rect.left() + page_rect.width() * (src.x1 / source_w).clamp(0.0, 1.0);
    let y1 = page_rect.top() + page_rect.height() * (src.y1 / source_h).clamp(0.0, 1.0);
    let x2 = page_rect.left() + page_rect.width() * (src.x2 / source_w).clamp(0.0, 1.0);
    let y2 = page_rect.top() + page_rect.height() * (src.y2 / source_h).clamp(0.0, 1.0);
    let rect = Rect::from_min_max(egui::pos2(x1, y1), egui::pos2(x2, y2));
    if rect.is_positive() { Some(rect) } else { None }
}

fn scene_pos_to_source(
    page_rect: Rect,
    source_w: f32,
    source_h: f32,
    scene_pos: Pos2,
) -> Option<Pos2> {
    if source_w <= 0.0 || source_h <= 0.0 || !page_rect.is_positive() {
        return None;
    }
    let u = ((scene_pos.x - page_rect.left()) / page_rect.width()).clamp(0.0, 1.0);
    let v = ((scene_pos.y - page_rect.top()) / page_rect.height()).clamp(0.0, 1.0);
    Some(egui::pos2(u * source_w, v * source_h))
}

fn text_detector_line_handle_points(scene_rect: Rect) -> [Pos2; 8] {
    [
        scene_rect.left_top(),
        egui::pos2(scene_rect.center().x, scene_rect.top()),
        scene_rect.right_top(),
        egui::pos2(scene_rect.right(), scene_rect.center().y),
        scene_rect.right_bottom(),
        egui::pos2(scene_rect.center().x, scene_rect.bottom()),
        scene_rect.left_bottom(),
        egui::pos2(scene_rect.left(), scene_rect.center().y),
    ]
}

fn move_text_detector_rect(
    start_rect: TextDetectorRect,
    start_pointer_src: Pos2,
    pointer_src: Pos2,
    source_w: f32,
    source_h: f32,
) -> TextDetectorRect {
    let width = (start_rect.x2 - start_rect.x1).max(1.0);
    let height = (start_rect.y2 - start_rect.y1).max(1.0);
    let dx = pointer_src.x - start_pointer_src.x;
    let dy = pointer_src.y - start_pointer_src.y;
    let max_x1 = (source_w - width).max(0.0);
    let max_y1 = (source_h - height).max(0.0);
    let x1 = (start_rect.x1 + dx).clamp(0.0, max_x1);
    let y1 = (start_rect.y1 + dy).clamp(0.0, max_y1);
    TextDetectorRect::from_xyxy(x1, y1, x1 + width, y1 + height).unwrap_or(start_rect)
}

fn resize_text_detector_rect(
    start_rect: TextDetectorRect,
    pointer_src: Pos2,
    handle_idx: usize,
    source_w: f32,
    source_h: f32,
) -> TextDetectorRect {
    let min_side = 4.0f32;
    let mut x1 = start_rect.x1;
    let mut y1 = start_rect.y1;
    let mut x2 = start_rect.x2;
    let mut y2 = start_rect.y2;

    let move_left = matches!(handle_idx, 0 | 6 | 7);
    let move_right = matches!(handle_idx, 2..=4);
    let move_top = matches!(handle_idx, 0..=2);
    let move_bottom = matches!(handle_idx, 4..=6);

    if move_left {
        x1 = pointer_src.x.clamp(0.0, (x2 - min_side).max(0.0));
    }
    if move_right {
        x2 = pointer_src.x.clamp((x1 + min_side).min(source_w), source_w);
    }
    if move_top {
        y1 = pointer_src.y.clamp(0.0, (y2 - min_side).max(0.0));
    }
    if move_bottom {
        y2 = pointer_src.y.clamp((y1 + min_side).min(source_h), source_h);
    }

    TextDetectorRect::from_xyxy(x1, y1, x2, y2).unwrap_or(start_rect)
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

fn draw_text_detector_mask_overlay_on_page(params: TextDetectorMaskOverlayDrawParams<'_>) {
    let TextDetectorMaskOverlayDrawParams {
        textures,
        ctx,
        painter,
        page_idx,
        page_rect,
        mask_size,
        mask_alpha,
        current_frame,
    } = params;
    if mask_alpha.is_empty() {
        return;
    }
    let mask_w = mask_size[0] as usize;
    let mask_h = mask_size[1] as usize;
    if mask_w == 0 || mask_h == 0 {
        return;
    }
    let expected_len = mask_w.saturating_mul(mask_h);
    if expected_len == 0 || expected_len != mask_alpha.len() {
        return;
    }

    let needs_rebuild = textures
        .get(&page_idx)
        .map(|page| page.size != [mask_w, mask_h])
        .unwrap_or(true);
    if needs_rebuild {
        let page_tex =
            build_text_detector_mask_texture_page(ctx, page_idx, [mask_w, mask_h], mask_alpha);
        textures.insert(page_idx, page_tex);
    }

    let Some(page_tex) = textures.get_mut(&page_idx) else {
        return;
    };
    page_tex.last_used_frame = current_frame;
    let src_w = page_tex.size[0] as f32;
    let src_h = page_tex.size[1] as f32;
    if src_w <= 0.0 || src_h <= 0.0 {
        return;
    }
    for tile in &page_tex.tiles {
        let ox = tile.origin_px[0] as f32;
        let oy = tile.origin_px[1] as f32;
        let tw = tile.size_px[0] as f32;
        let th = tile.size_px[1] as f32;
        if tw <= 0.0 || th <= 0.0 {
            continue;
        }
        let dst = Rect::from_min_size(
            egui::pos2(
                page_rect.left() + page_rect.width() * (ox / src_w),
                page_rect.top() + page_rect.height() * (oy / src_h),
            ),
            egui::vec2(
                page_rect.width() * (tw / src_w),
                page_rect.height() * (th / src_h),
            ),
        );
        painter.image(
            tile.texture.id(),
            dst,
            Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

fn build_text_detector_mask_texture_page(
    ctx: &egui::Context,
    page_idx: usize,
    size: [usize; 2],
    alpha: &[u8],
) -> TextDetectorMaskTexturePage {
    let w = size[0];
    let h = size[1];
    if w == 0 || h == 0 {
        return TextDetectorMaskTexturePage {
            size,
            tiles: Vec::new(),
            last_used_frame: 0,
        };
    }
    let mut tiles = Vec::new();
    let mut y = 0usize;
    while y < h {
        let mut x = 0usize;
        while x < w {
            let tw = (w - x).min(TEXT_DETECTOR_MASK_TILE_SIDE);
            let th = (h - y).min(TEXT_DETECTOR_MASK_TILE_SIDE);
            let tile_img = build_text_detector_mask_tile_image(size, alpha, x, y, tw, th);
            let texture = ctx.load_texture(
                format!("text-detector-mask-{page_idx}-{x}-{y}"),
                tile_img,
                TEXT_DETECTOR_MASK_TEXTURE_OPTIONS,
            );
            tiles.push(TextDetectorMaskTextureTile {
                texture,
                origin_px: [x, y],
                size_px: [tw, th],
            });
            x += TEXT_DETECTOR_MASK_TILE_SIDE;
        }
        y += TEXT_DETECTOR_MASK_TILE_SIDE;
    }
    TextDetectorMaskTexturePage {
        size,
        tiles,
        last_used_frame: 0,
    }
}

fn text_detector_mask_texture_page_estimated_bytes(page_tex: &TextDetectorMaskTexturePage) -> u64 {
    let bytes = page_tex
        .tiles
        .iter()
        .map(|tile| {
            tile.size_px[0]
                .saturating_mul(tile.size_px[1])
                .saturating_mul(4)
        })
        .fold(0usize, usize::saturating_add);
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

fn update_text_detector_mask_texture_tiles(
    page_tex: &mut TextDetectorMaskTexturePage,
    size: [usize; 2],
    alpha: &[u8],
    dirty_rect: (usize, usize, usize, usize),
) {
    let (min_x, min_y, max_x, max_y) = dirty_rect;
    if size[0] == 0 || size[1] == 0 || alpha.is_empty() || min_x > max_x || min_y > max_y {
        return;
    }
    let tx0 = min_x / TEXT_DETECTOR_MASK_TILE_SIDE;
    let ty0 = min_y / TEXT_DETECTOR_MASK_TILE_SIDE;
    let tx1 = max_x / TEXT_DETECTOR_MASK_TILE_SIDE;
    let ty1 = max_y / TEXT_DETECTOR_MASK_TILE_SIDE;
    for tile in &mut page_tex.tiles {
        let tile_x = tile.origin_px[0] / TEXT_DETECTOR_MASK_TILE_SIDE;
        let tile_y = tile.origin_px[1] / TEXT_DETECTOR_MASK_TILE_SIDE;
        if tile_x < tx0 || tile_x > tx1 || tile_y < ty0 || tile_y > ty1 {
            continue;
        }
        let tile_img = build_text_detector_mask_tile_image(
            size,
            alpha,
            tile.origin_px[0],
            tile.origin_px[1],
            tile.size_px[0],
            tile.size_px[1],
        );
        tile.texture
            .set(tile_img, TEXT_DETECTOR_MASK_TEXTURE_OPTIONS);
    }
}

fn build_text_detector_mask_tile_image(
    size: [usize; 2],
    alpha: &[u8],
    origin_x: usize,
    origin_y: usize,
    tile_w: usize,
    tile_h: usize,
) -> egui::ColorImage {
    let full_w = size[0];
    let mut raw = vec![0u8; tile_w * tile_h * 4];
    for ty in 0..tile_h {
        let sy = origin_y + ty;
        let row_off = sy * full_w;
        for tx in 0..tile_w {
            let sx = origin_x + tx;
            let src_idx = row_off + sx;
            let dst_idx = (ty * tile_w + tx) * 4;
            let src_alpha = alpha.get(src_idx).copied().unwrap_or(0);
            let a = ((src_alpha as u16 * TEXT_DETECTOR_MASK_VISUAL_ALPHA_MAX as u16) / 255) as u8;
            // ColorImage::from_rgba_premultiplied expects RGB already multiplied by alpha.
            // Keep transparent pixels fully colorless to avoid red tint in black mask regions.
            raw[dst_idx] = a;
            raw[dst_idx + 1] = 0;
            raw[dst_idx + 2] = 0;
            raw[dst_idx + 3] = a;
        }
    }
    egui::ColorImage::from_rgba_premultiplied([tile_w, tile_h], &raw)
}

fn text_detection_blocks_file_path(dir: &Path, page_idx: usize) -> PathBuf {
    dir.join(format!("{page_idx:05}_blocks.json"))
}

fn text_detection_mask_file_name(page_idx: usize) -> String {
    format!("{page_idx:05}_mask.png")
}

fn text_detection_mask_file_path(dir: &Path, page_idx: usize) -> PathBuf {
    dir.join(text_detection_mask_file_name(page_idx))
}

fn load_text_detection_storage(storage_dir: &Path, page_indices: &[usize]) -> DetectionLoadResult {
    if !storage_dir.exists() {
        return Ok((Vec::new(), 0, 0));
    }
    let mut out = Vec::<(usize, TextDetectorPageResult)>::new();
    let mut loaded = 0usize;
    let mut failed = 0usize;

    for page_idx in page_indices {
        let blocks_path = text_detection_blocks_file_path(storage_dir, *page_idx);
        if !blocks_path.exists() {
            continue;
        }
        match load_text_detection_page(storage_dir, *page_idx) {
            Ok(Some(result)) => {
                out.push((*page_idx, result));
                loaded = loaded.saturating_add(1);
            }
            Ok(None) => {}
            Err(_) => {
                failed = failed.saturating_add(1);
            }
        }
    }
    Ok((out, loaded, failed))
}

fn load_text_detection_page(
    storage_dir: &Path,
    page_idx: usize,
) -> Result<Option<TextDetectorPageResult>, String> {
    let blocks_path = text_detection_blocks_file_path(storage_dir, page_idx);
    let raw = fs::read_to_string(&blocks_path)
        .map_err(|err| format!("{}: {err}", blocks_path.display()))?;
    let data = serde_json::from_str::<Value>(&raw)
        .map_err(|err| format!("{}: {err}", blocks_path.display()))?;
    let Some(obj) = data.as_object() else {
        return Ok(None);
    };

    let source_size = parse_u32_pair(obj.get("source_size"))
        .ok_or_else(|| format!("{}: невалидный source_size", blocks_path.display()))?;
    let mut blocks = Vec::<TextDetectorRect>::new();
    if let Some(items) = obj.get("blocks").and_then(Value::as_array) {
        for item in items {
            let x1 = item.get("x1").and_then(Value::as_f64);
            let y1 = item.get("y1").and_then(Value::as_f64);
            let x2 = item.get("x2").and_then(Value::as_f64);
            let y2 = item.get("y2").and_then(Value::as_f64);
            let (Some(x1), Some(y1), Some(x2), Some(y2)) = (x1, y1, x2, y2) else {
                continue;
            };
            if let Some(rect) =
                TextDetectorRect::from_xyxy(x1 as f32, y1 as f32, x2 as f32, y2 as f32)
            {
                blocks.push(rect);
            }
        }
    }
    blocks.sort_by(|a, b| {
        a.y1.total_cmp(&b.y1)
            .then_with(|| a.x1.total_cmp(&b.x1))
            .then_with(|| a.y2.total_cmp(&b.y2))
            .then_with(|| a.x2.total_cmp(&b.x2))
    });

    let mask_size = parse_u32_pair(obj.get("mask_size")).unwrap_or(source_size);
    let mask_file = obj
        .get("mask_file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| storage_dir.join(s))
        .unwrap_or_else(|| text_detection_mask_file_path(storage_dir, page_idx));
    let mask_alpha = if mask_file.exists() {
        match image::open(&mask_file) {
            Ok(img) => {
                let gray = img.to_luma8();
                let w = gray.width();
                let h = gray.height();
                if w == mask_size[0] && h == mask_size[1] {
                    gray.into_raw()
                } else {
                    Vec::new()
                }
            }
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    Ok(Some(TextDetectorPageResult {
        source_size,
        blocks,
        mask_size,
        mask_alpha,
    }))
}

fn save_text_detection_storage(
    storage_dir: &Path,
    pages: &[(usize, TextDetectorPageResult)],
) -> Result<(usize, usize), String> {
    fs::create_dir_all(storage_dir).map_err(|err| format!("{}: {err}", storage_dir.display()))?;

    if let Ok(entries) = fs::read_dir(storage_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.ends_with("_blocks.json") || name.ends_with("_mask.png") {
                let _ = fs::remove_file(&path);
            }
        }
    }

    let mut saved = 0usize;
    let mut failed = 0usize;

    let mut sorted = pages.to_vec();
    sorted.sort_by_key(|(page_idx, _)| *page_idx);
    for (page_idx, result) in sorted {
        match save_text_detection_page(storage_dir, page_idx, &result) {
            Ok(()) => {
                saved = saved.saturating_add(1);
            }
            Err(_) => {
                failed = failed.saturating_add(1);
            }
        }
    }

    Ok((saved, failed))
}

fn save_text_detection_page(
    storage_dir: &Path,
    page_idx: usize,
    result: &TextDetectorPageResult,
) -> Result<(), String> {
    let mask_file_name = text_detection_mask_file_name(page_idx);
    let blocks_path = text_detection_blocks_file_path(storage_dir, page_idx);
    let mask_path = storage_dir.join(&mask_file_name);

    let has_mask = !result.mask_alpha.is_empty()
        && result.mask_size[0] > 0
        && result.mask_size[1] > 0
        && result.mask_alpha.len()
            == (result.mask_size[0] as usize).saturating_mul(result.mask_size[1] as usize);

    if has_mask {
        let img = image::GrayImage::from_vec(
            result.mask_size[0],
            result.mask_size[1],
            result.mask_alpha.clone(),
        )
        .ok_or_else(|| format!("Некорректная маска для страницы {page_idx}"))?;
        img.save_with_format(&mask_path, image::ImageFormat::Png)
            .map_err(|err| format!("{}: {err}", mask_path.display()))?;
    } else if mask_path.exists() {
        let _ = fs::remove_file(&mask_path);
    }

    let blocks = result
        .blocks
        .iter()
        .map(|rect| {
            serde_json::json!({
                "x1": rect.x1,
                "y1": rect.y1,
                "x2": rect.x2,
                "y2": rect.y2
            })
        })
        .collect::<Vec<_>>();
    let payload = serde_json::json!({
        "page_idx": page_idx,
        "source_size": result.source_size,
        "blocks": blocks,
        "mask_size": result.mask_size,
        "mask_file": if has_mask { mask_file_name } else { String::new() },
    });
    let raw = serde_json::to_string_pretty(&payload).map_err(|err| err.to_string())?;
    fs::write(&blocks_path, raw).map_err(|err| format!("{}: {err}", blocks_path.display()))?;
    Ok(())
}

fn parse_u32_pair(value: Option<&Value>) -> Option<[u32; 2]> {
    let arr = value?.as_array()?;
    if arr.len() < 2 {
        return None;
    }
    let w = arr[0].as_u64().and_then(|v| u32::try_from(v).ok())?;
    let h = arr[1].as_u64().and_then(|v| u32::try_from(v).ok())?;
    Some([w, h])
}

fn contains_any_page(canvas: &CanvasView, project: &ProjectData, pos: Pos2) -> bool {
    project.pages.iter().any(|page| {
        canvas
            .page_scene_rect(page.idx)
            .map(|rect| rect.contains(pos))
            .unwrap_or(false)
    })
}

/// Default page-crop rect centered on the bubble anchor, sized to a 256x256 source-pixel square.
///
/// The crop page dimensions are read from the image header (cheap, header-only) to convert 256 px
/// into normalized half-extents; if they are unavailable it falls back to a small UV box. This
/// avoids the previous `±0.05` UV default, which on tall ribbon pages produced a near-full-height
/// image area.
fn default_image_crop_rect_values(project: &ProjectData, bubble: &Bubble) -> Vec<Value> {
    const DEFAULT_IMAGE_CROP_SIDE_SRC_PX: f32 = 256.0;
    let half = DEFAULT_IMAGE_CROP_SIDE_SRC_PX * 0.5;
    let (u_half, v_half) = project
        .pages
        .iter()
        .find(|page| page.idx == bubble.img_idx)
        .and_then(|page| image::image_dimensions(&page.path).ok())
        .map(|(w, h)| (half / (w.max(1) as f32), half / (h.max(1) as f32)))
        .unwrap_or((0.05, 0.05));
    [
        bubble.img_u - u_half,
        bubble.img_v - v_half,
        bubble.img_u + u_half,
        bubble.img_v + v_half,
    ]
    .into_iter()
    .map(|value| Value::from(f64::from(value.clamp(0.0, 1.0))))
    .collect()
}

fn mt_image_input_for_bubble(bubble: &Bubble) -> Option<MtImageInput> {
    if !is_image_bubble_record(bubble) {
        return None;
    }
    let source_type = bubble_extra_string(&bubble.extra, "image_source_type");
    let description = bubble_extra_string(&bubble.extra, "description");
    if source_type == "page_crop" {
        let page_idx = bubble
            .extra
            .get("crop_page_idx")
            .and_then(Value::as_u64)
            .and_then(|raw| usize::try_from(raw).ok())
            .unwrap_or(bubble.img_idx);
        let crop_rect = bubble
            .extra
            .get("crop_rect")
            .and_then(Value::as_array)
            .and_then(|items| {
                if items.len() != 4 {
                    return None;
                }
                let mut rect = [0.0; 4];
                for (idx, item) in items.iter().enumerate() {
                    rect[idx] = item.as_f64()? as f32;
                }
                Some(normalize_uv_rect(rect))
            })
            .unwrap_or([
                bubble.img_u - 0.05,
                bubble.img_v - 0.05,
                bubble.img_u + 0.05,
                bubble.img_v + 0.05,
            ]);
        let crop_rect = normalize_uv_rect(crop_rect);
        return Some(MtImageInput {
            description,
            source: MtImageSource::PageCrop {
                page_idx,
                crop_rect,
            },
            areas: mt_image_areas_for_bubble(bubble, Some(crop_rect)),
        });
    }

    let image_path = bubble_extra_string(&bubble.extra, "image_path");
    (!image_path.trim().is_empty()).then(|| MtImageInput {
        description,
        source: MtImageSource::ExternalPath(image_path),
        areas: mt_image_areas_for_bubble(bubble, None),
    })
}

/// Builds the ordered text areas of an image bubble for AI translation.
///
/// Area 0's text is read from the legacy fields (`original_text` + `extra.description`); later
/// areas come from `extra["text_areas"]`. `crop` is the page-crop region (the sent image) used to
/// express each area's bounding box relative to that image; `None` (external images) omits the
/// positional hint.
fn mt_image_areas_for_bubble(bubble: &Bubble, crop: Option<[f32; 4]>) -> Vec<MtImageArea> {
    let description0 = bubble_extra_string(&bubble.extra, "description");
    let rel = |area_rect: [f32; 4]| -> Option<[f32; 4]> {
        let c = crop?;
        let cw = (c[2] - c[0]).max(1e-6);
        let ch = (c[3] - c[1]).max(1e-6);
        Some([
            ((area_rect[0] - c[0]) / cw).clamp(0.0, 1.0),
            ((area_rect[1] - c[1]) / ch).clamp(0.0, 1.0),
            ((area_rect[2] - c[0]) / cw).clamp(0.0, 1.0),
            ((area_rect[3] - c[1]) / ch).clamp(0.0, 1.0),
        ])
    };
    let read_rect = |entry: &Value| -> Option<[f32; 4]> {
        let arr = entry.get("rect").and_then(Value::as_array)?;
        if arr.len() != 4 {
            return None;
        }
        let mut rect = [0.0f32; 4];
        for (idx, item) in arr.iter().enumerate() {
            rect[idx] = item.as_f64()? as f32;
        }
        Some(rect)
    };
    let fallback_rect = crop.unwrap_or([0.0, 0.0, 1.0, 1.0]);
    match bubble.extra.get("text_areas").and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => arr
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let area_rect = read_rect(entry).unwrap_or(fallback_rect);
                let (description, original) = if idx == 0 {
                    (description0.clone(), bubble.original_text.clone())
                } else {
                    (
                        entry
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        entry
                            .get("original")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    )
                };
                MtImageArea {
                    description,
                    original,
                    rel_bbox: rel(area_rect),
                }
            })
            .collect(),
        _ => vec![MtImageArea {
            description: description0,
            original: bubble.original_text.clone(),
            rel_bbox: None,
        }],
    }
}

fn is_image_bubble_record(bubble: &Bubble) -> bool {
    bubble.bubble_class.as_deref().map(BubbleClass::from_str) == Some(BubbleClass::Image)
}

fn image_bubbles_dir(project: &ProjectData) -> PathBuf {
    project.paths.unsaved_image_bubbles_dir.clone()
}

fn save_clipboard_image_bubble(project: &ProjectData, bubble_id: i64) -> Result<PathBuf, String> {
    let clipboard_image = paste_image::read_image_from_clipboard()?;
    let width = u32::try_from(clipboard_image.width)
        .map_err(|_| "картинка из буфера слишком широкая".to_string())?;
    let height = u32::try_from(clipboard_image.height)
        .map_err(|_| "картинка из буфера слишком высокая".to_string())?;
    let Some(image) = image::RgbaImage::from_raw(width, height, clipboard_image.rgba) else {
        return Err("буфер картинки не соответствует ширине и высоте".to_string());
    };
    let dir = image_bubbles_dir(project);
    fs::create_dir_all(&dir)
        .map_err(|err| format!("не удалось создать каталог {}: {err}", dir.display()))?;
    let path = dir.join(format!("image_bubble_{bubble_id}.png"));
    image
        .save(&path)
        .map_err(|err| format!("не удалось сохранить {}: {err}", path.display()))?;
    Ok(path)
}

fn copy_external_image_bubble(
    project: &ProjectData,
    bubble_id: i64,
    source: &Path,
) -> Result<PathBuf, String> {
    if !source.is_file() {
        return Err(format!("файл не найден: {}", source.display()));
    }
    let dir = image_bubbles_dir(project);
    fs::create_dir_all(&dir)
        .map_err(|err| format!("не удалось создать каталог {}: {err}", dir.display()))?;
    let ext = source
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("png");
    let path = dir.join(format!("image_bubble_{bubble_id}.{ext}"));
    fs::copy(source, &path).map_err(|err| {
        format!(
            "не удалось скопировать {} в {}: {err}",
            source.display(),
            path.display()
        )
    })?;
    Ok(path)
}

fn project_relative_path(project: &ProjectData, path: &Path) -> String {
    path.strip_prefix(&project.paths.unsaved_dir)
        .or_else(|_| path.strip_prefix(&project.project_dir))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn build_image_crop_selection(
    canvas: &CanvasView,
    project: &ProjectData,
    selection_rect: Rect,
) -> Option<(usize, [f32; 4])> {
    let mut best_area = 0.0_f32;
    let mut best_page: Option<(usize, Rect)> = None;
    for page in &project.pages {
        let Some(page_rect) = canvas.page_scene_rect(page.idx) else {
            continue;
        };
        let intersection = page_rect.intersect(selection_rect);
        if !intersection.is_positive() {
            continue;
        }
        let area = intersection.width() * intersection.height();
        if area > best_area {
            best_area = area;
            best_page = Some((page.idx, page_rect));
        }
    }

    let (page_idx, page_rect) = best_page?;
    let crop_scene = page_rect.intersect(selection_rect);
    if !crop_scene.is_positive() {
        return None;
    }
    let page_w = page_rect.width().max(1.0);
    let page_h = page_rect.height().max(1.0);
    Some((
        page_idx,
        normalize_uv_rect([
            ((crop_scene.left() - page_rect.left()) / page_w).clamp(0.0, 1.0),
            ((crop_scene.top() - page_rect.top()) / page_h).clamp(0.0, 1.0),
            ((crop_scene.right() - page_rect.left()) / page_w).clamp(0.0, 1.0),
            ((crop_scene.bottom() - page_rect.top()) / page_h).clamp(0.0, 1.0),
        ]),
    ))
}

fn normalize_uv_rect(rect: [f32; 4]) -> [f32; 4] {
    [
        rect[0].min(rect[2]).clamp(0.0, 1.0),
        rect[1].min(rect[3]).clamp(0.0, 1.0),
        rect[0].max(rect[2]).clamp(0.0, 1.0),
        rect[1].max(rect[3]).clamp(0.0, 1.0),
    ]
}

/// Builds the canvas `rect_coords` extra value (`{p1:{img_u,img_v}, p2:{img_u,img_v}}`) from a
/// normalized `[x1,y1,x2,y2]` crop rect, so the canvas red image-area rect matches the crop region.
fn rect_coords_value(rect: [f32; 4]) -> Value {
    let rect = normalize_uv_rect(rect);
    let point = |u: f32, v: f32| {
        Value::Object(
            [
                ("img_u".to_string(), Value::from(f64::from(u))),
                ("img_v".to_string(), Value::from(f64::from(v))),
            ]
            .into_iter()
            .collect(),
        )
    };
    Value::Object(
        [
            ("p1".to_string(), point(rect[0], rect[1])),
            ("p2".to_string(), point(rect[2], rect[3])),
        ]
        .into_iter()
        .collect(),
    )
}

fn spawn_translation_settings_saver_thread()
-> (Sender<TranslationSettingsSaveRequest>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<TranslationSettingsSaveRequest>();
    let handle = thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            if first.settings_file.as_os_str().is_empty() {
                break;
            }
            let mut latest = first;
            while let Ok(next) = rx.try_recv() {
                if next.settings_file.as_os_str().is_empty() {
                    return;
                }
                latest = next;
            }
            if let Err(err) = save_translation_settings_to_project_file(
                &latest.settings_file,
                &latest.ocr_options,
                &latest.mt_options,
                &latest.composition_options,
                &latest.text_detector_options,
            ) {
                eprintln!(
                    "failed to persist translation settings {}: {err}",
                    latest.settings_file.display()
                );
            }
        }
    });
    (tx, handle)
}

fn save_translation_settings_to_project_file(
    settings_file: &Path,
    ocr_options: &OcrPanelOptions,
    mt_options: &MtPanelOptions,
    composition_options: &CompositionPanelOptions,
    text_detector_options: &TextDetectorPanelOptions,
) -> Result<(), String> {
    let mut root = if settings_file.exists() {
        match fs::read_to_string(settings_file) {
            Ok(raw) => {
                serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
            }
            Err(_) => Value::Object(Map::new()),
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("object ensured");

    let mut ocr_obj = root_obj
        .get("OCR")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    ocr_obj.insert(
        "engine".to_string(),
        Value::String(ocr_engine_to_project_key(ocr_options.engine).to_string()),
    );
    ocr_obj.insert("join".to_string(), Value::Bool(ocr_options.join_newlines));
    ocr_obj.insert(
        "reflect".to_string(),
        Value::Bool(ocr_options.reflect_strings),
    );
    ocr_obj.insert(
        "copy".to_string(),
        Value::Bool(ocr_options.copy_to_clipboard),
    );
    ocr_obj.insert(
        "bubbles".to_string(),
        Value::Bool(ocr_options.create_bubble),
    );
    ocr_obj.insert(
        "replace_chars".to_string(),
        Value::Bool(ocr_options.replace_chars_enabled),
    );
    ocr_obj.insert(
        "char_replacements".to_string(),
        Value::Array(
            ocr_options
                .char_replacements
                .iter()
                .map(|rule| {
                    let mut rule_obj = Map::new();
                    rule_obj.insert("enabled".to_string(), Value::Bool(rule.enabled));
                    rule_obj.insert(
                        "targets".to_string(),
                        Value::String(rule.targets_raw.clone()),
                    );
                    rule_obj.insert(
                        "replacement".to_string(),
                        Value::String(rule.replacement.clone()),
                    );
                    Value::Object(rule_obj)
                })
                .collect(),
        ),
    );

    let mut params_obj = ocr_obj
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut manga_obj = params_obj
        .get("mangaocr")
        .or_else(|| params_obj.get("manga"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    manga_obj.insert(
        "model".to_string(),
        Value::String(ocr_options.manga_model.clone()),
    );
    params_obj.insert("mangaocr".to_string(), Value::Object(manga_obj));
    let mut easy_obj = params_obj
        .get("easyocr")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    easy_obj.insert(
        "langs".to_string(),
        Value::String(ocr_options.easy_langs.clone()),
    );
    easy_obj.insert(
        "full_langs".to_string(),
        Value::Bool(ocr_options.easy_show_full_langs),
    );
    if !easy_obj.contains_key("gpu") {
        easy_obj.insert("gpu".to_string(), Value::Bool(false));
    }
    params_obj.insert("easyocr".to_string(), Value::Object(easy_obj));
    let mut paddle_obj = params_obj
        .get("paddle")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    paddle_obj.insert(
        "langs".to_string(),
        Value::String(ocr_options.paddle_lang.clone()),
    );
    paddle_obj.insert(
        "full_langs".to_string(),
        Value::Bool(ocr_options.paddle_show_full_langs),
    );
    if !paddle_obj.contains_key("gpu") {
        paddle_obj.insert("gpu".to_string(), Value::Bool(false));
    }
    params_obj.insert("paddle".to_string(), Value::Object(paddle_obj));
    let mut paddle_vl_obj = params_obj
        .get("paddle_vl")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    paddle_vl_obj.insert(
        "script".to_string(),
        Value::String(ocr_options.paddle_vl_script.clone()),
    );
    params_obj.insert("paddle_vl".to_string(), Value::Object(paddle_vl_obj));
    let mut surya_obj = params_obj
        .get("surya")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    surya_obj.insert(
        "task".to_string(),
        Value::String(ocr_options.surya_task_name.clone()),
    );
    surya_obj.insert(
        "recognize_math".to_string(),
        Value::Bool(ocr_options.surya_recognize_math),
    );
    surya_obj.insert(
        "sort_lines".to_string(),
        Value::Bool(ocr_options.surya_sort_lines),
    );
    surya_obj.insert(
        "drop_repeated_text".to_string(),
        Value::Bool(ocr_options.surya_drop_repeated_text),
    );
    surya_obj.insert(
        "max_sliding_window".to_string(),
        Value::Number(serde_json::Number::from(u64::from(
            ocr_options.surya_max_sliding_window,
        ))),
    );
    surya_obj.insert(
        "max_tokens".to_string(),
        Value::Number(serde_json::Number::from(u64::from(
            ocr_options.surya_max_tokens,
        ))),
    );
    params_obj.insert("surya".to_string(), Value::Object(surya_obj));
    let mut ai_api_obj = params_obj
        .get("ai_api")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    ai_api_obj.insert(
        "service".to_string(),
        Value::String(ocr_options.ai_api_service.key().to_string()),
    );
    ai_api_obj.insert(
        "model".to_string(),
        Value::String(ocr_options.ai_api_model.clone()),
    );
    ai_api_obj.insert(
        "system_instruction".to_string(),
        Value::String(ocr_options.ai_api_system_instruction.clone()),
    );
    params_obj.insert("ai_api".to_string(), Value::Object(ai_api_obj));
    ocr_obj.insert("params".to_string(), Value::Object(params_obj));
    root_obj.insert("OCR".to_string(), Value::Object(ocr_obj));

    let mut mt_obj = root_obj
        .get("machine_translation")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    mt_obj.insert(
        "service".to_string(),
        Value::String(mt_options.service.key().to_string()),
    );
    mt_obj.insert(
        "source_lang".to_string(),
        Value::String(normalized_lang_input(&mt_options.source_lang, "auto")),
    );
    mt_obj.insert(
        "target_lang".to_string(),
        Value::String(normalized_lang_input(&mt_options.target_lang, "ru")),
    );
    mt_obj.insert(
        "active_tab".to_string(),
        Value::String(
            match mt_options.active_tab {
                MtPanelTab::Machine => "machine_translation",
                MtPanelTab::AiApi => "ai_api",
            }
            .to_string(),
        ),
    );
    let mut mt_ai_obj = mt_obj
        .get("ai_api")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    mt_ai_obj.insert(
        "service".to_string(),
        Value::String(mt_options.ai_api_service.key().to_string()),
    );
    mt_ai_obj.insert(
        "model".to_string(),
        Value::String(mt_options.ai_api_model.clone()),
    );
    mt_ai_obj.insert(
        "system_instruction".to_string(),
        Value::String(mt_options.ai_api_system_instruction.clone()),
    );
    mt_ai_obj.insert(
        "sort_mode".to_string(),
        Value::String(mt_options.ai_sort_mode.key().to_string()),
    );
    mt_ai_obj.insert(
        "use_character_names".to_string(),
        Value::Bool(mt_options.ai_use_character_names),
    );
    mt_ai_obj.insert(
        "use_notes_prompt".to_string(),
        Value::Bool(mt_options.ai_use_notes_prompt),
    );
    mt_ai_obj.insert(
        "include_characters".to_string(),
        Value::Bool(mt_options.ai_include_characters),
    );
    mt_ai_obj.insert(
        "include_terms".to_string(),
        Value::Bool(mt_options.ai_include_terms),
    );
    mt_ai_obj.insert(
        "include_existing_translation".to_string(),
        Value::Bool(mt_options.ai_include_existing_translation),
    );
    mt_ai_obj.insert(
        "include_image_bubbles".to_string(),
        Value::Bool(mt_options.ai_include_image_bubbles),
    );
    mt_ai_obj.insert(
        "image_detail".to_string(),
        Value::String(mt_options.ai_image_detail.key().to_string()),
    );
    mt_ai_obj.insert(
        "image_mode".to_string(),
        Value::String(mt_options.ai_image_mode.key().to_string()),
    );
    mt_ai_obj.insert(
        "image_context_source".to_string(),
        Value::String(mt_options.ai_image_context_source.key().to_string()),
    );
    mt_ai_obj.insert(
        "batch_size".to_string(),
        Value::Number(serde_json::Number::from(
            mt_options.ai_batch_size.clamp(1, 100),
        )),
    );
    mt_ai_obj.insert(
        "reasoning".to_string(),
        Value::String(mt_options.ai_reasoning.key().to_string()),
    );
    mt_ai_obj.insert(
        "context_limit_percent".to_string(),
        Value::Number(serde_json::Number::from(
            mt_options.ai_context_limit_percent.clamp(10, 100),
        )),
    );
    mt_obj.insert("ai_api".to_string(), Value::Object(mt_ai_obj));
    mt_obj.remove("threads");
    mt_obj.remove("params");
    root_obj.insert("machine_translation".to_string(), Value::Object(mt_obj));

    let mut composition_obj = root_obj
        .get("composition")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    composition_obj.insert(
        "method".to_string(),
        Value::String(composition_options.sort_method.key().to_string()),
    );
    composition_obj.insert(
        "source_mode".to_string(),
        Value::String(composition_options.source_mode.key().to_string()),
    );
    composition_obj.insert(
        "ignore_translated_lines".to_string(),
        Value::Bool(composition_options.ignore_translated_lines),
    );
    composition_obj.insert(
        "merge_same_character".to_string(),
        Value::Bool(composition_options.merge_same_character),
    );
    composition_obj.insert(
        "sep_same_character".to_string(),
        Value::String(composition_options.sep_same_character.clone()),
    );
    composition_obj.insert(
        "sep_between".to_string(),
        Value::String(composition_options.sep_between.clone()),
    );
    composition_obj.insert(
        "replica_prefix".to_string(),
        Value::String(composition_options.replica_prefix.clone()),
    );
    composition_obj.insert(
        "nl_replace".to_string(),
        Value::String(if composition_options.nl_replace.is_empty() {
            " ".to_string()
        } else {
            composition_options.nl_replace.clone()
        }),
    );
    composition_obj.insert(
        "nl_replace_enabled".to_string(),
        Value::Bool(composition_options.nl_replace_enabled),
    );
    composition_obj.insert(
        "wrap_with".to_string(),
        Value::String(normalize_wrap_with(&composition_options.wrap_with)),
    );
    composition_obj.insert(
        "wrap_with_enabled".to_string(),
        Value::Bool(composition_options.wrap_with_enabled),
    );
    composition_obj.insert(
        "limit".to_string(),
        Value::Number((composition_options.limit.clamp(100, 100_000) as u64).into()),
    );
    composition_obj.insert(
        "limit_enabled".to_string(),
        Value::Bool(composition_options.limit_enabled),
    );
    composition_obj.insert(
        "use_character_names".to_string(),
        Value::Bool(composition_options.use_character_names),
    );
    composition_obj.insert(
        "include_image_bubbles".to_string(),
        Value::Bool(composition_options.include_image_bubbles),
    );
    composition_obj.insert(
        "jinja2_enabled".to_string(),
        Value::Bool(composition_options.jinja2_enabled),
    );
    composition_obj.insert(
        "jinja2_template".to_string(),
        Value::String(composition_options.jinja2_template.clone()),
    );
    root_obj.insert("composition".to_string(), Value::Object(composition_obj));

    let mut text_detector_obj = root_obj
        .get("text_detector")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    text_detector_obj.insert(
        "algorithm".to_string(),
        Value::String(text_detector_options.algorithm.key().to_string()),
    );
    text_detector_obj.insert(
        "draw_lines".to_string(),
        Value::Bool(text_detector_options.draw_lines),
    );
    text_detector_obj.insert(
        "draw_mask".to_string(),
        Value::Bool(text_detector_options.draw_mask),
    );
    text_detector_obj.insert(
        "block_expand_px".to_string(),
        Value::Number((text_detector_options.block_expand_px.clamp(0, 200) as i64).into()),
    );
    text_detector_obj.insert(
        "merge_gap_px".to_string(),
        Value::Number((text_detector_options.merge_gap_px.clamp(0, 200) as i64).into()),
    );
    let mut text_detector_params = text_detector_obj
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    text_detector_params.remove("device");
    text_detector_params.insert(
        "detect_size".to_string(),
        Value::Number((text_detector_options.ai_detect_size.clamp(896, 2048) as i64).into()),
    );
    text_detector_params.insert(
        "det_rearrange_max_batches".to_string(),
        Value::Number(
            (text_detector_options
                .ai_det_rearrange_max_batches
                .clamp(1, 64) as i64)
                .into(),
        ),
    );
    text_detector_params.insert(
        "font size multiplier".to_string(),
        serde_json::Number::from_f64(
            text_detector_options
                .ai_font_size_multiplier
                .clamp(0.1, 8.0) as f64,
        )
        .map(Value::Number)
        .unwrap_or_else(|| Value::Number(serde_json::Number::from(1))),
    );
    text_detector_params.insert(
        "font size max".to_string(),
        serde_json::Number::from_f64(
            text_detector_options.ai_font_size_max.clamp(-1.0, 500.0) as f64
        )
        .map(Value::Number)
        .unwrap_or_else(|| Value::Number(serde_json::Number::from(-1))),
    );
    text_detector_params.insert(
        "font size min".to_string(),
        serde_json::Number::from_f64(
            text_detector_options.ai_font_size_min.clamp(-1.0, 500.0) as f64
        )
        .map(Value::Number)
        .unwrap_or_else(|| Value::Number(serde_json::Number::from(-1))),
    );
    text_detector_params.insert(
        "mask dilate size".to_string(),
        Value::Number((text_detector_options.mask_dilate_size.clamp(0, 30) as i64).into()),
    );
    text_detector_obj.insert(
        "mask_dilate_size".to_string(),
        Value::Number((text_detector_options.mask_dilate_size.clamp(0, 30) as i64).into()),
    );
    text_detector_obj.insert("params".to_string(), Value::Object(text_detector_params));
    root_obj.insert(
        "text_detector".to_string(),
        Value::Object(text_detector_obj),
    );

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(settings_file, payload).map_err(|err| err.to_string())?;
    Ok(())
}

fn normalized_lang_input(raw: &str, fallback: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Renders read-only monospace text that wraps to the panel width. egui labels support text
/// selection, so the exact prompt text stays copyable in the AI request-preview window.
fn selectable_monospace(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).monospace().small()).wrap());
}

fn format_context_chars(chars: usize) -> String {
    if chars >= 1_000 {
        format!("{}.{:01}k", chars / 1_000, (chars % 1_000) / 100)
    } else {
        chars.to_string()
    }
}

fn parse_text_detector_algorithm_key(raw: &str) -> TextDetectorAlgorithm {
    match raw.trim().to_ascii_lowercase().as_str() {
        "paddleocr" | "paddle_ocr" | "paddle-ocr" | "paddle" | "onnx" => {
            TextDetectorAlgorithm::PaddleOcr
        }
        "ai" | "ai_ctd" | "ctd" | "ml" => TextDetectorAlgorithm::Ai,
        "surya" | "suryaocr" | "surya_ocr" | "surya-det" | "surya_det" => {
            TextDetectorAlgorithm::Surya
        }
        _ => TextDetectorAlgorithm::Classic,
    }
}

fn parse_ocr_engine_key(engine: &str) -> OcrEngine {
    let key = engine.trim().to_ascii_lowercase();
    match key.as_str() {
        "easyocr" | "easy" => OcrEngine::EasyOcr,
        "paddle" | "paddleocr" => OcrEngine::PaddleOcr,
        "paddle_vl" | "paddlevl" | "paddleocr_vl" | "paddleocrvl" | "paddleocr-vl" => {
            OcrEngine::PaddleVl
        }
        "surya" | "suryaocr" | "surya_ocr" => OcrEngine::Surya,
        "aiapi" | "ai_api" | "ai-api" | "genai" => OcrEngine::AiApi,
        "paddle_onnx" | "paddleonnx" | "paddle-onnx" | "onnx" => OcrEngine::PaddleOcr,
        "mangaocr" | "manga_ocr" | "manga" | "mocr" => OcrEngine::MangaOcr,
        _ => OcrEngine::MangaOcr,
    }
}

fn ocr_engine_to_project_key(engine: OcrEngine) -> &'static str {
    match engine {
        OcrEngine::MangaOcr => "mangaocr",
        OcrEngine::EasyOcr => "easyocr",
        OcrEngine::PaddleOcr => "paddle",
        OcrEngine::PaddleVl => "paddle_vl",
        OcrEngine::Surya => "surya",
        OcrEngine::AiApi => "ai_api",
    }
}

fn parse_ocr_lang_text_setting(value: Option<&Value>) -> Option<String> {
    let raw = value?;
    match raw {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Array(arr) => {
            let collected = arr
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            if collected.is_empty() {
                None
            } else {
                Some(collected.join(", "))
            }
        }
        _ => None,
    }
}

fn parse_single_ocr_lang_setting(value: Option<&Value>) -> Option<String> {
    let text = parse_ocr_lang_text_setting(value)?;
    let first = text.split(',').next().map(str::trim).unwrap_or("");
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

fn ocr_engine_requires_backend_runtime(options: &OcrPanelOptions) -> bool {
    options.engine.requires_backend()
}

fn build_ocr_runtime_options(ocr_options: &OcrPanelOptions) -> OcrRuntimeOptions {
    let surya_is_active = ocr_options.engine == OcrEngine::Surya;
    OcrRuntimeOptions {
        manga_model: ocr_options.manga_model.clone(),
        paddle_lang: ocr_options.paddle_lang.clone(),
        paddle_vl_script: ocr_options.paddle_vl_script.clone(),
        easy_langs: ocr_options.easy_langs.clone(),
        surya_task_name: if surya_is_active {
            "ocr_without_boxes".to_string()
        } else {
            ocr_options.surya_task_name.clone()
        },
        surya_recognize_math: if surya_is_active {
            false
        } else {
            ocr_options.surya_recognize_math
        },
        surya_sort_lines: if surya_is_active {
            false
        } else {
            ocr_options.surya_sort_lines
        },
        surya_drop_repeated_text: if surya_is_active {
            false
        } else {
            ocr_options.surya_drop_repeated_text
        },
        surya_max_sliding_window: if surya_is_active {
            0
        } else {
            ocr_options.surya_max_sliding_window
        },
        surya_max_tokens: if surya_is_active {
            0
        } else {
            ocr_options.surya_max_tokens
        },
        ai_api_service: ocr_options.ai_api_service,
        ai_api_model: ocr_options.ai_api_model.clone(),
        ai_api_system_instruction: ocr_options.ai_api_system_instruction.clone(),
    }
}

fn build_ocr_request(
    canvas: &CanvasView,
    project: &ProjectData,
    selection_rect: Rect,
    request_id: u64,
    ocr_options: &OcrPanelOptions,
) -> Option<BuiltOcrRequest> {
    let mut best_area = 0.0_f32;
    let mut best_page: Option<(&Page, Rect)> = None;

    for page in &project.pages {
        let Some(page_rect) = canvas.page_scene_rect(page.idx) else {
            continue;
        };
        let intersection = page_rect.intersect(selection_rect);
        if !intersection.is_positive() {
            continue;
        }
        let area = intersection.width() * intersection.height();
        if area > best_area {
            best_area = area;
            best_page = Some((page, page_rect));
        }
    }

    let (page, page_rect) = best_page?;
    let crop_scene = page_rect.intersect(selection_rect);
    if !crop_scene.is_positive() {
        return None;
    }

    let page_w = page_rect.width().max(1.0);
    let page_h = page_rect.height().max(1.0);
    let u1 = ((crop_scene.left() - page_rect.left()) / page_w).clamp(0.0, 1.0);
    let v1 = ((crop_scene.top() - page_rect.top()) / page_h).clamp(0.0, 1.0);
    let u2 = ((crop_scene.right() - page_rect.left()) / page_w).clamp(0.0, 1.0);
    let v2 = ((crop_scene.bottom() - page_rect.top()) / page_h).clamp(0.0, 1.0);

    Some(BuiltOcrRequest {
        request: OcrRecognizeRequest {
            request_id,
            engine: ocr_options.engine,
            options: build_ocr_runtime_options(ocr_options),
            page_path: page.path.clone(),
            uv_rect: [u1, v1, u2, v2],
            image_override_png: None,
            join_newlines: ocr_options.join_newlines,
            reflect_strings: ocr_options.reflect_strings,
            char_replacements: ocr_options.runtime_char_replacements(),
        },
        page_idx: page.idx,
    })
}

/// Parses the persisted `char_replacements` array into editable UI rules.
///
/// Each element must be an object with `enabled` (bool, default `true`),
/// `targets` (string), and `replacement` (string). Non-object entries are
/// skipped so a malformed settings file cannot abort project loading.
fn parse_char_replacement_rules(rules: &[Value]) -> Vec<CharReplacementRuleUi> {
    rules
        .iter()
        .filter_map(Value::as_object)
        .map(|rule| CharReplacementRuleUi {
            enabled: rule.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            targets_raw: rule
                .get("targets")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            replacement: rule
                .get("replacement")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .collect()
}

fn build_bubble_original_text(
    result: &crate::tabs::translation::ocr::OcrRecognizeResult,
    join_newlines: bool,
) -> String {
    let lines = result
        .lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return result.text.trim().to_string();
    }
    let separator = if join_newlines { "\n" } else { " " };
    lines.join(separator)
}

#[cfg(test)]
mod tests {
    use super::footer_tracking_should_recompute;

    #[test]
    fn footer_tracking_not_bootstrapped_always_recomputes() {
        // Before bootstrap the first frame must recompute regardless of the cached revision.
        assert!(footer_tracking_should_recompute(None, 7, false));
        assert!(footer_tracking_should_recompute(Some(7), 7, false));
    }

    #[test]
    fn footer_tracking_changed_revision_recomputes() {
        // A new bubble bumps `hook_bubbles_revision`, so a differing revision forces a recompute.
        assert!(footer_tracking_should_recompute(Some(7), 8, true));
        assert!(footer_tracking_should_recompute(None, 8, true));
    }

    #[test]
    fn footer_tracking_equal_revision_when_bootstrapped_skips() {
        // Unchanged bubble set on a bootstrapped tab: skip the per-frame recompute.
        assert!(!footer_tracking_should_recompute(Some(8), 8, true));
    }
}
