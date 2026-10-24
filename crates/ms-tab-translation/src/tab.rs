/*
FILE OVERVIEW: crates/ms-tab-translation/src/tab.rs
Translation tab state and orchestration for the tools, OCR, machine translation,
text detector, and footer metadata synchronization with CanvasView/BubblesModel.

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
- `TextDetectionStorageEvent`: async load/save result for `text_detection` disk storage.
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

TranslationTabState public API:
- `begin_input_frame` / `end_input_frame`: the host hands the frame's input (and binding labels)
  to the state for its draw; `input_contexts`, `lines_edit_active`, `input_labels` feed the host.
- `sync_with_project_settings`: lazy-loads OCR/MT/composition/text-detector settings.

Tools (child module `tab/tool_panels.rs`):
- `restore_tool_toggles` (host, first frame: the on-set persisted in the dock),
  `prepare_tool_frame` (host, before the canvas draw: current page size, health / route caches),
  `toggle_tool` / `set_tool_on` and the `toggle_*_panel_hotkey` entry points,
  `apply_tool_dock_requests` (focus + persistence, before `PanelDock::begin`),
  `draw_tools_bar_body` / `tools_tab_sizes` (the «Инструменты» toolbar tab) and
  `draw_tool_body` + one `draw_*_tool_body` per tool.

Panel/UI flow:
- `draw_startup_page_load_toast`: renders startup image-loading progress toast on canvas.
- `draw_toast`: renders toast popup.
- `draw_text_detector_mask_layer_on_page`: draws detector mask and mask-edit interaction on canvas mask layer.
- `draw_text_detector_additional_overlay_on_page`: draws detector boxes/line-edit overlay on additional-elements layer.
  Both draw while `detector_overlay_visible()` (`tab/detector_region.rs`): Detector tool on,
  or the selected OCR engine's "create mask" parameter on.
- `draw_canvas_overlay_top_left` (CanvasHooks): frame orchestrator — polling (including the MT
  request preview), flushes, the pending Composition rebuild, then the panel dock.

Panel dock (child module `tab/dock.rs`):
- `draw_panel_dock`: runs this tab's `PanelDock` frame — the canvas' «Лента» plus this tab's own
  «Последние персонажи» (`TRANSLATION_RECENT_CHARACTERS_TAB`), the «Инструменты» toolbar and the
  five tool tabs — at the end of
  `draw_canvas_overlay_top_left`. It must run on every frame the tab is active —
  `PanelDock::end` is what keeps the dock's detached sub-window viewports alive.
- `TranslationDockCx`: the per-frame dock context (the whole tab state, the canvas, the project);
  `draw_recent_characters_tab_body` plus the toolbar and tool bodies are this tab's own bodies.
- `ribbon_panel_rect`: last frame's MAIN-WINDOW rect of the panel holding «Лента», `None` when the
  tab is hidden or detached; `text_detector_edit_panel_pos` places the two detector edit boxes
  under it and falls back to a fixed canvas offset.
- `translation_default_dock_layout`: this tab's OWN default arrangement, since a default layout is
  the dictionary stored tab keys are resolved against. Registered under `AppTab::Translation.key()`
  both in `studio.rs::restore_panel_dock` and in `draw_panel_dock`'s `ensure_default_layout`.

Characters/footer sync:
- `ensure_character_names_loaded`, `reload_character_names`: load character names cache.
- `maybe_refresh_character_names_by_watch`: throttled mtime-watch `characters.json` with auto-refresh.
- `SessionCapabilities::title_documents` (`set_session_capabilities`): false in a single-image
  session (no title) — the roster is then empty without any I/O and the watch never probes.
- `sync_chapter_character_names`, `rebuild_character_names`: chapter-typed half of the footer
  autocomplete base and the reassembly of the combined list.
- `sync_footer_tracking`: tracks bubble lifecycle and prunes footer caches.
- `init_last_footer_values`: bootstraps last-used footer defaults from latest bubble.
- `apply_defaults_for_new_bubble`: applies footer defaults to newly detected bubbles.
- `queue_footer_patch`: buffers one footer field patch with timestamp.
- `flush_footer_patches`: debounced flush of footer patches to `CanvasView::patch_bubble_extra_fields`.
- `footer_state_for`: resolves runtime footer state with overrides.
- `build_bubble_footer` (CanvasHooks): footer editor UI + patch scheduling; the replica-order spin
  box is shared, then the bubble class selects the rest of the row. Its character field is fed
  `footer_autocomplete_candidates(&character_names)` — the sentinel-free slice.
- `build_image_bubble_footer_controls`: image-class footer half (source type, paste/choose, crop).
- `build_hint_bubble_footer_controls`: hint-class footer half (one visibility checkbox).
- `build_bubble_header` (CanvasHooks): currently no-op hook.

OCR flow:
- `poll_ocr_events`: consumes OCR controller events, toasts, optional bubble insert, retry logic.
- `drive_canvas_selection` (`tab/canvas_input.rs`): the canvas-wide rect selections, mouse and
  finger alike, from `InputFrame::claimed`; an OCR selection on an unavailable route is refused
  with a toast at its start.
- `open_advanced_recognition_for_scene_rect`: opens advanced-recognition window for selected scene rect.

Text detector flow:
- `poll_text_detector_events`: consumes detector events and updates status/progress/results.
- `ensure_text_detection_storage_loaded`: lazy auto-load persisted detector results.
- `start_text_detection_storage_load`, `start_text_detection_storage_save`: async IO jobs.
- `SessionCapabilities::chapter_storage`: false in a single-image session (scratch deleted on
  exit): no lazy load, no Save button, a stray save request is refused.
- `poll_text_detection_storage_events`: applies load/save completion results.
- `tab/detector_region.rs`: an OCR selection's region detection (PaddleOCR / Surya / NeuroLate
  with that engine's own "create mask" parameter; started from `start_ocr_for_scene_rect` and the
  advanced window's Create bubble, answered through `poll_text_detector_events`, or for a quick
  NeuroLate selection by the OCR answer itself, `Recognized::region` -> `receive_ocr_region`,
  awaited in `pending_ocr_regions`) merged into both detector
  stores (held in `pending_region_detections` while a storage job runs), and
  `detection_pages_for_save`, the Save snapshot of the SHOWN masks (shared model, brush edits
  included), taken under the model lock with only the written masks copied.
- `apply_loaded_detection_pages`: fills empty pages from the stored detection.
- `rasterize_resolved_blocks`: the display raster of a legacy blocks-only page.
- `set_text_detector_status`: updates detector status text and color.
- `materialize_text_mask_page_from_blocks_if_missing`: legacy fallback that converts
  current detector blocks into a real editable mask if stored results have no `mask_alpha`.
- `has_detected_blocks_on_page`, `has_detected_blocks_any`, `detected_page_indices`:
  availability helpers for detector OCR actions.
- `collect_detected_block_rects_px`: resolved detector blocks with current panel options.
- `textdetector_ocr_is_running`: aggregate detector-OCR busy state.
- `text_detector_run_mode`, `text_detector_running_status`: detector mode/status selection
  (the NeuroLate mode carries `neurolate_params`, see `tab/neurolate.rs`).
- `start_text_detection_for_current_page`, `start_text_detection_for_all_pages`:
  detector run entry points.
- `start_text_detector_ocr_for_indices`: queues OCR tasks for detected blocks.
- `maybe_dispatch_next_textdetector_ocr_request`: scheduler for sequential detector OCR.
- `finish_textdetector_ocr_if_done`: finalize detector OCR batch.
- `abort_textdetector_ocr`: hard-reset detector OCR state after a fatal condition (error status) or a user stop (neutral status).
- `create_text_detector_line_at_uv`: adds a line (page menu item "create line", lines edit).
- `tab/canvas_input.rs`: the lines editor and the mask brush on the page (their gestures and
  the lines delete / new commands come from the input frame, `crate::input`).

Machine translation flow:
- `poll_mt_events`: consumes MT controller events and applies translated text to canvas.
- `mt_has_active_or_pending`: checks running/queued machine-translation work.
- `cancel_active_mt`: cancels queued MT start requests and active worker run.
- `handle_pending_mt_actions`: dispatches deferred MT requests (single bubble/page/all).
- `start_mt_for_scope`: prepares MT items for current page or whole project.
- `start_mt_for_ids`: prepares MT items for explicit bubble ids.
- `start_mt_with_items`: validates options and starts MT controller run.
- `on_bubble_action` (CanvasHooks): queues translate action for bubble button.

Browser-extension MT transport (MT panel):
- `browser_extension`: the `tab/browser_extension.rs` controller; `draw_mt_tool_body` lends its
  view and applies the transport / install / open-folder clicks, `poll_browser_extension` turns
  its worker failures into toasts.

AI API connection (OCR and MT panels):
- `ocr_ai_api_tasks`, `mt_ai_api_tasks`: one `ms_ai_api::AiApiTaskRunner` per panel (key save /
  delete, metadata refresh), fed from the panels' `AiApiConnectionActions`.
- `poll_ai_api_events`: applies their events; a replaced model marks that panel's settings dirty,
  notices become toasts.

Composition/settings handling:
- `rebuild_composition_text`: rebuilds composed text using composition options.
- `ensure_ocr_settings_loaded`, `ensure_mt_settings_loaded`,
  `ensure_composition_settings_loaded`, `ensure_text_detector_settings_loaded`:
  one-time settings hydration per project settings path.
- `flush_settings_save_if_needed`: sends coalesced async save request when any settings are dirty.

Module-level utility functions:
- Character names / detector geometry:
  `build_translation_character_names`, `footer_autocomplete_candidates`,
  `collect_chapter_character_names`,
  `detector_blocks_with_options`, `detector_expand_blocks`,
  `detector_merge_blocks`, `detector_rects_touch_or_near`, `detector_rects_to_blocks_px`,
  `source_rect_to_scene_rect`.
- Detector mask rendering:
  `draw_text_detector_mask_overlay_on_page`, `build_text_detector_mask_texture_page`,
  `build_text_detector_mask_tile_image`.
- Detector storage paths/IO:
  `text_detection_blocks_file_path`, `text_detection_mask_file_name`,
  `text_detection_mask_file_path`, `load_text_detection_storage`,
  `load_text_detection_page`, `save_text_detection_storage`, `save_text_detection_page`.
- Misc helpers:
  `parse_u32_pair`.
- Async settings persistence:
  `spawn_translation_settings_saver_thread`, `save_translation_settings_to_project_file`.
- Settings parsing/conversion:
  `normalized_lang_input`, `parse_text_detector_algorithm_key`, `parse_ocr_engine_key`,
  `ocr_engine_to_project_key`, `parse_ocr_lang_text_setting`, `parse_single_ocr_lang_setting`.
- OCR request/text helpers:
  `build_ocr_request`, `build_bubble_original_text`.

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
  `text_detector_options`, `composition_panel_options`, `composition_panel_state`.
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
  `footer_tracking_synced_revision`, `footer_known_ids`,
  `footer_overrides`, `pending_footer_patches`, `pending_footer_patch_changed_at`,
  `footer_character_autocomplete`,
  `last_*` defaults.
- Settings persistence: dirty flags, `*_settings_loaded_for`, `settings_save_tx`,
  `settings_save_thread`.
*/

use ms_widgets::bubble_status::{BubbleBorderStyle, BubbleStatusContext, evaluate_bubble_status_rules};
use ms_canvas::{
    BubbleAction, BubbleClass, CanvasHooks, CanvasScrollbarContext,
    CanvasUiStatus, CanvasView, TranslationStatusDisplay,
};
use ms_input::{InputFrame, InputRegistry};
use crate::input::{ContextFacts, TranslationInputLabels};
use ms_memory::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, select_eviction_candidates,
};
use ms_models::SessionCapabilities;
use ms_models::text_mask_model::{TextMaskModel, TextMaskPage};
use ms_sysprobe::paste_image;
use ms_project::{Bubble, ComicType, Page, ProjectData};
use ms_tabs_simple::characters::load_character_names;
use crate::adv_rec::{
    AdvancedRecognitionAction, AdvancedRecognitionSelection, AdvancedRecognitionWindow,
};
use ms_local_ai::backend_health::{AiBackendHealthSnapshot, AiBackendProbeCommand, paddle_vl_ready_for};
use ms_local_ai::local_ai::{AiRouteKind, LocalAiAvailability, LocalAiOff};
use crate::machine_translation::{
    AiMtContextSource, AiMtImageDetail, AiMtImageMode, AiMtOptions, AiMtReasoning, AiMtSortMode,
    MtControllerEvent, MtImageArea, MtImageInput, MtImageSource, MtRequestPreview,
    MtRequestPreviewPart, MtService, MtTranslateItem, MtTranslateRequest, TranslationMtController,
    bubble_order_for_sort, build_ai_mt_request_preview, character_for_bubble, whole_minutes_ceil,
};
use crate::ocr::{
    OcrControllerEvent, OcrEngine, OcrLoadState, OcrRecognizeRequest, OcrRuntimeOptions,
    TranslationOcrController, external_model_specs,
};
use crate::model_download_status::ModelDownloadStatus;
use crate::ocr_model_download::OcrModelDownloadController;
use crate::panels::ocr_model_download::ModelDownloadAction;
use ms_sysprobe::ai_models::external::ExternalModelSpec;
use ms_sysprobe::ai_models::external_catalog::PaddleVlVariant;
use ms_ai_api::{AiApiService, AiApiTaskRunner, GenerationSnapshot, ImageInputSupport, image_input_support, is_probable_quota_or_limit_error};
use crate::panels::bubbles::{
    BubbleFooterState, BubblesPanelState, ImageBubbleFilePick, bubble_extra_bool,
    bubble_extra_string, bubble_footer_state_from_record, footer_no_character,
    footer_no_characters,
    footer_state_for_bubble,
};
use crate::panels::composition::{
    CompositionPanelOptions, CompositionPanelState, CompositionSortMethod, CompositionSourceMode,
    compose_translation_text, normalize_wrap_with,
};
use crate::panels::machine_translation::{
    MtPanelOptions, MtPanelProgress, MtPanelTab, MtStopNotice,
};
use crate::panels::ocr::{
    CharReplacementRuleUi, OcrPanelOptions, selected_mode_requirement,
};
use crate::panels::text_detector::{
    DetectorUnavailable, TextDetectorAlgorithm, TextDetectorPanelOptions,
    TextDetectorPlanNoticeCache,
};
use crate::neurolate::NeurolateSettings;
use ms_ai_api::neurolate::NeurolateKeyPanel;
use crate::text_detector::{
    RegionDetection, SURYA_DETECTOR_SPECS, TextDetectorControllerEvent, TextDetectorPageResult, TextDetectorRect, TextDetectorRunMode,
    TranslationTextDetectorController,
};
use ms_tools::MaskBrush;
use ms_widgets::panel_dock::PanelDockState;
use ms_widgets::{
    AiRequirement, AutocompleteLine, MarkFill, ScrollMark, ScrollSpan, WheelComboBox, WheelSlider,
    WheelSpinBox,
};
use eframe::egui;
use egui::{Color32, Pos2, Rect, Stroke};
use ms_theme::Severity;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use ms_thread::{self as thread, JoinHandle};
use web_time::Duration;

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
// The MT panel's browser-extension block: its state and the bridge start / install workers
// (`pub(crate)`: the MT panel draws its view types).
pub(crate) mod browser_extension;

use canvas_input::CanvasSelection;
pub use dock::translation_default_dock_layout;
use tool_panels::{ToolToggles, TranslationTool};

/// (detected results per page, total loaded, total failed)
type DetectionLoadResult = Result<(Vec<(usize, TextDetectorPageResult)>, usize, usize), String>;

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
    /// Where (page-normalized) the lines edit page menu was opened: its "create line" item adds
    /// the line there.
    text_detector_line_menu_uv: Option<Pos2>,
    text_detector_mask_stroke_state: Option<TextDetectorMaskStrokeState>,
    text_detector_mask_brush: MaskBrush,
    text_detection_storage_loaded_for: Option<PathBuf>,
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
    // Last `CanvasView::hook_bubbles_revision()` for which `sync_footer_tracking` ran a full
    // recompute. Used to skip the per-frame snapshot/clone/sort when the bubble set is unchanged.
    footer_tracking_synced_revision: Option<u64>,
    footer_known_ids: HashSet<i64>,
    footer_overrides: HashMap<i64, BubbleFooterState>,
    /// The one open «choose file» dialog of an external image bubble, started by the canvas
    /// footer or a bubbles-panel card and resolved by [`Self::poll_image_bubble_pick`].
    image_bubble_pick: Option<ImageBubbleFilePick>,
    pending_footer_patches: HashMap<i64, Map<String, Value>>,
    pending_footer_patch_changed_at: HashMap<i64, f64>,
    footer_character_autocomplete: HashMap<i64, AutocompleteLine>,
    bubbles_panel: BubblesPanelState,
    composition_panel_options: CompositionPanelOptions,
    composition_panel_state: CompositionPanelState,
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
            text_detector_line_menu_uv: None,
            text_detector_mask_stroke_state: None,
            text_detector_mask_brush: MaskBrush::default(),
            text_detection_storage_loaded_for: None,
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
            footer_tracking_synced_revision: None,
            footer_known_ids: HashSet::new(),
            footer_overrides: HashMap::new(),
            image_bubble_pick: None,
            pending_footer_patches: HashMap::new(),
            pending_footer_patch_changed_at: HashMap::new(),
            footer_character_autocomplete: HashMap::new(),
            bubbles_panel: BubblesPanelState::default(),
            composition_panel_options: CompositionPanelOptions::default(),
            composition_panel_state: CompositionPanelState::default(),
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

impl Drop for TranslationTabState {
    fn drop(&mut self) {
        let _ = self.settings_save_tx.send(TranslationSettingsSaveRequest {
            settings_file: PathBuf::new(),
            ocr_options: OcrPanelOptions::default(),
            mt_options: MtPanelOptions::default(),
            composition_options: CompositionPanelOptions::default(),
            text_detector_options: TextDetectorPanelOptions::default(),
            neurolate: NeurolateSettings::default(),
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
            OcrEngine::Baberu => 6,
            OcrEngine::NeuroLate => 7,
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
        // Per variant: a resident engine of another PaddleOCR-VL variant is not "ready".
        let paddle_vl_ready = paddle_vl_ready_for(snapshot, self.ocr_panel_options.paddle_vl_model.key());
        let surya_ready = snapshot.ocr_surya_ready;
        let baberu_ready = snapshot.ocr_baberu_ready;
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
        sync_engine(self, OcrEngine::Baberu, baberu_ready);
    }

    /// The external model bundle of the OCR panel's current selection
    /// (`ocr::external_model_specs`); empty for engines without one.
    fn selected_ocr_model_specs(&self) -> Vec<&'static ExternalModelSpec> {
        let options = &self.ocr_panel_options;
        external_model_specs(options.engine, options.paddle_vl_model, &options.easy_langs)
    }

    /// Applies the OCR panel's external-model outcome of one frame: a requested download /
    /// cancel, a fresh status probe when the selected bundle changed (engine, PaddleOCR-VL
    /// variant or the EasyOCR languages' script; an earlier "installed" may be stale), and
    /// `NotLoaded` for PaddleOCR-VL after a variant switch (the loaded weights are the old
    /// variant's). `specs_before` is the bundle selected at the start of the frame.
    fn apply_ocr_model_panel_changes(
        &mut self,
        specs_before: &[&'static ExternalModelSpec],
        variant_before: PaddleVlVariant,
        action: ModelDownloadAction,
    ) {
        let specs = self.selected_ocr_model_specs();
        if self.ocr_panel_options.paddle_vl_model != variant_before {
            self.set_ocr_state_for_engine(OcrEngine::PaddleVl, OcrLoadState::NotLoaded);
        }
        if specs != specs_before {
            self.ocr_model_downloads.request_bundle_status(&specs);
        }
        match action {
            ModelDownloadAction::Download if !specs.is_empty() => self.ocr_model_downloads.start_download(&specs),
            ModelDownloadAction::Verify if !specs.is_empty() => self.ocr_model_downloads.start_verify(&specs),
            ModelDownloadAction::Cancel if !specs.is_empty() => self.ocr_model_downloads.cancel(),
            ModelDownloadAction::None | ModelDownloadAction::Download | ModelDownloadAction::Verify | ModelDownloadAction::Cancel => {}
        }
    }

    /// Applies the detector panel's Surya-checkpoint download / verify request of one frame.
    fn apply_detector_model_panel_action(&mut self, action: ModelDownloadAction) {
        match action {
            ModelDownloadAction::Download => self.ocr_model_downloads.start_download(&SURYA_DETECTOR_SPECS),
            ModelDownloadAction::Verify => self.ocr_model_downloads.start_verify(&SURYA_DETECTOR_SPECS),
            ModelDownloadAction::Cancel => self.ocr_model_downloads.cancel(),
            ModelDownloadAction::None => {}
        }
    }

    /// Per-frame poll of the external OCR model controller; keeps repainting while a
    /// download or status probe is in flight so progress stays live.
    fn poll_ocr_model_downloads(&mut self, ctx: &egui::Context) {
        self.ocr_model_downloads.poll();
        if self.ocr_model_downloads.is_active() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn request_ai_backend_health_check(&mut self, ctx: &egui::Context, force: bool) {
        if self.local_ai.backend.is_err() {
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

    fn ocr_quick_selection_mode_active(&self) -> bool {
        self.input_mode_active(crate::input::OCR_QUICK_MODE)
    }

    fn ocr_advanced_selection_mode_active(&self) -> bool {
        self.input_mode_active(crate::input::OCR_ADVANCED_MODE)
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

    /// Whether the backend is known unusable: off for the process (`local_ai.backend`), or
    /// checked and not connected. A not-yet-probed backend counts as usable (optimistic).
    fn ai_backend_unavailable(&self) -> bool {
        if self.local_ai.backend.is_err() {
            return true;
        }
        let snapshot = self.ai_backend_health_snapshot();
        snapshot.checked_at.is_some() && !snapshot.connected
    }

    /// The routing inputs (effective `AiRuntime` + provider-scoped SIGILL load guard) the
    /// route gates use, evaluated through `gate`: against the background-refreshed snapshot
    /// (see `refresh_ocr_route_inputs_cache`), or — before the first read landed —
    /// optimistically over the possible inputs (`route_inputs::gate_over_inputs`), so nothing
    /// is refused that the real inputs might allow. Never reads disk.
    #[cfg(not(target_arch = "wasm32"))]
    fn gate_over_ocr_route_inputs<T, E>(
        &self,
        gate: impl Fn((ms_config::AiRuntime, ms_config::OrtLoadDecision)) -> Result<T, E>,
    ) -> Result<T, E> {
        route_inputs::gate_over_inputs(self.ocr_route_inputs.snapshot(), gate)
    }

    /// Web build: no native runtime, so every local route is a backend route.
    #[cfg(target_arch = "wasm32")]
    fn gate_over_ocr_route_inputs<T, E>(
        &self,
        gate: impl Fn((ms_config::AiRuntime, ms_config::OrtLoadDecision)) -> Result<T, E>,
    ) -> Result<T, E> {
        gate((ms_config::AiRuntime::Backend, ms_config::OrtLoadDecision::Safe))
    }

    /// Route-aware gate for the selected OCR engine/model: what it runs on, or why it cannot
    /// run in this process (`crate::ocr::ocr_route_availability`, the single owner). An
    /// `Ok(AiRouteKind::Backend)` additionally needs a connected backend
    /// ([`Self::ai_backend_unavailable`]); AI API is always `Ok(Cloud)`, even under `--no-ai`.
    /// Uses the cached route inputs (no disk I/O); `&mut self` callers refresh them first
    /// through [`Self::current_ocr_route_availability`].
    fn selected_ocr_route_availability(&self) -> Result<AiRouteKind, LocalAiOff> {
        self.gate_over_ocr_route_inputs(|(runtime, guard)| {
            crate::ocr::ocr_route_availability(
                self.ocr_panel_options.engine,
                &self.ocr_panel_options.manga_model,
                runtime,
                guard,
                &self.local_ai,
            )
        })
    }

    /// [`Self::selected_ocr_route_availability`] after refreshing the throttled route-input
    /// cache, for the OCR triggers and the panel.
    fn current_ocr_route_availability(&mut self) -> Result<AiRouteKind, LocalAiOff> {
        #[cfg(not(target_arch = "wasm32"))]
        self.refresh_ocr_route_inputs_cache();
        self.selected_ocr_route_availability()
    }

    /// Whether the selected OCR engine/model runs on the Python backend in this process.
    fn ocr_requires_backend_runtime(&mut self) -> bool {
        matches!(self.current_ocr_route_availability(), Ok(AiRouteKind::Backend))
    }

    /// Why an OCR request for the selected engine/model must be refused now, or `None`: the
    /// route's local runtime is off (its reason), or the route needs the backend and the
    /// backend is unavailable. The OCR triggers toast it as an error. Before the first
    /// route-inputs read landed a native-capable selection is not refused for the backend
    /// (see [`Self::gate_over_ocr_route_inputs`]); the OCR worker routes with the real inputs.
    fn ocr_request_refusal(&mut self) -> Option<String> {
        match self.current_ocr_route_availability() {
            Err(off) => Some(off.user_message()),
            Ok(AiRouteKind::Backend) if self.ai_backend_unavailable() => {
                Some(t!("translation.tab.backend_unavailable_status").to_string())
            }
            Ok(AiRouteKind::Backend | AiRouteKind::Native | AiRouteKind::Cloud) => None,
        }
    }

    /// The text detector's availability for the selected algorithm: the PaddleOCR route from
    /// the cached route inputs (`detector_native_route`, the rule the detector worker applies),
    /// checked against `local_ai` and the backend's PyTorch report.
    fn text_detector_availability(&self) -> Result<(), DetectorUnavailable> {
        self.detector_algorithm_availability(self.text_detector_options.algorithm)
    }

    /// [`Self::text_detector_availability`] for any `algorithm` (an OCR selection's region
    /// detection runs the OCR engine's detector, not necessarily the panel's).
    fn detector_algorithm_availability(&self, algorithm: TextDetectorAlgorithm) -> Result<(), DetectorUnavailable> {
        let torch_available = self.ai_backend_torch_available();
        self.gate_over_ocr_route_inputs(|(runtime, guard)| {
            algorithm.availability(
                crate::text_detector::detector_native_route(runtime, guard),
                &self.local_ai,
                torch_available,
            )
        })
    }

    /// Non-blocking refresh step of the cached native-runtime routing inputs: adopts a
    /// background read that landed and, at most once per `OCR_ROUTE_INPUTS_CACHE_TTL` with
    /// none in flight, starts the next one (`route_inputs::spawn_route_inputs_read`). Never
    /// touches the disk on the calling (GUI) thread, so the per-frame gate calls it freely.
    /// Each landed snapshot republishes the native-ORT capability slot.
    #[cfg(not(target_arch = "wasm32"))]
    fn refresh_ocr_route_inputs_cache(&mut self) {
        let landed = self.ocr_route_inputs.poll(
            web_time::Instant::now(),
            OCR_ROUTE_INPUTS_CACHE_TTL,
            route_inputs::spawn_route_inputs_read,
        );
        if let Some((runtime, guard)) = landed {
            // Publish native onnxruntime availability for UI gating. Native ORT is
            // "armed" — considered available even before its dylib loads — when native ONNX
            // is allowed in this process (`local_ai.native_onnx`: not `--no-ai`, and the
            // platform has it), the effective runtime is Native and the SIGILL load guard is
            // Safe. Otherwise native is not the ORT path, so this slot is false and ORT
            // availability falls to the backend slot (backend connected + providers).
            let native_armed = self.local_ai.native_onnx.is_ok()
                && runtime == ms_config::AiRuntime::Native
                && guard == ms_config::OrtLoadDecision::Safe;
            ms_sysprobe::ai_backend_capabilities::set_native_ort_available(Some(native_armed));
        }
    }

    fn ai_backend_torch_available(&self) -> Option<bool> {
        self.ai_backend_health_snapshot()
            .is_torch_available
            .or_else(ms_sysprobe::ai_backend_capabilities::torch_available)
    }

    /// Whether the selected OCR engine+model needs PyTorch. Delegates to the single owner of the
    /// selected mode's runtime requirement, `panels::ocr::selected_mode_requirement`.
    fn selected_ocr_mode_requires_torch(&self) -> bool {
        matches!(selected_mode_requirement(&self.ocr_panel_options), Some(AiRequirement::Torch))
    }

    fn current_ocr_torch_requirement_error(&self) -> Option<String> {
        if !self.selected_ocr_mode_requires_torch() {
            return None;
        }
        matches!(self.ai_backend_torch_available(), Some(false))
            .then(|| t!("translation.common.pytorch_not_installed_status").to_string())
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

    /// Re-reads the project character roster from `characters.json` and rebuilds the footer
    /// autocomplete base around it. The chapter-collected names are kept as they are: they are
    /// derived from bubbles, not from the roster file.
    fn reload_character_names(&mut self, project: &ProjectData) {
        // A single-image session has no title, hence no roster: reading it would probe (and the
        // characters loader may create) a `characters/` folder inside the scratch directory.
        // The footer still offers the built-in and chapter-collected names.
        if !self.session_caps.title_documents {
            self.character_names_project = Vec::new();
            self.rebuild_character_names();
            self.characters_loaded_for = Some(project.paths.characters_dir.clone());
            self.characters_file_signature = None;
            return;
        }
        // A read failure must be visible: the assembled list still looks populated (built-ins plus
        // the names typed in this chapter), so a silently empty roster is easy to miss in the UI.
        // Not per-frame spam — this runs only on a project/chapter switch, on an observed
        // `characters.json` mtime change, or on an explicit `notify_characters_changed`.
        self.character_names_project = match load_character_names(project) {
            Ok(names) => names,
            Err(error) => {
                ms_log::runtime_log::log_warn(format!(
                    "[translation] could not read the character roster; the footer autocomplete \
                     falls back to built-in and chapter-collected names only. Dir: {} Error: {}",
                    project.paths.characters_dir.display(),
                    error
                ));
                Vec::new()
            }
        };
        self.rebuild_character_names();
        self.characters_loaded_for = Some(project.paths.characters_dir.clone());
        self.characters_file_signature = characters_file_signature(project);
    }

    /// Reassembles `character_names` from its two inputs (`characters.json` roster and the names
    /// typed in this chapter). Called only when one of them actually changed — both consumers
    /// (the bubbles panel whole, the footer via `footer_autocomplete_candidates`) borrow the list
    /// as a slice every frame, so rebuilding it per frame would be pure waste.
    fn rebuild_character_names(&mut self) {
        self.character_names = build_translation_character_names(
            &self.character_names_project,
            &self.character_names_chapter,
        );
    }

    /// Refreshes the chapter-collected part of the footer autocomplete base from `bubbles`.
    ///
    /// `bubbles` must be the WHOLE chapter (all pages), not the current page: a name typed on any
    /// page is offered on every other one.
    ///
    /// Cost, precisely: the full chapter scan ALWAYS runs when this is called — only the
    /// reassembly of `character_names` is skipped, and only when the collected list is equal to
    /// the stored one. The caller's revision gate is what keeps the scan off the per-frame path;
    /// note that the revision moves on every footer patch flush, i.e. roughly every
    /// `FOOTER_PATCH_DEBOUNCE_SECS` while a name is being typed, so the scan must stay linear and
    /// allocation-light (see `collect_chapter_character_names`).
    ///
    /// A name just typed into the footer reaches the model through that same debounced
    /// `pending_footer_patches` queue, so it enters the base one flush later, not on the
    /// keystroke. That lag is accepted deliberately: the collection runs off the bubble revision,
    /// and the flush is what moves it.
    fn sync_chapter_character_names(&mut self, bubbles: &[Bubble]) {
        let collected = collect_chapter_character_names(bubbles);
        if collected == self.character_names_chapter {
            return;
        }
        self.character_names_chapter = collected;
        self.rebuild_character_names();
    }

    fn maybe_refresh_character_names_by_watch(&mut self, project: &ProjectData, now_s: f64) {
        if !self.session_caps.title_documents
            || now_s - self.character_names_watch_last_check_s < CHARACTER_NAMES_WATCH_CHECK_SECS
        {
            return;
        }
        self.character_names_watch_last_check_s = now_s;
        let signature = characters_file_signature(project);
        if signature != self.characters_file_signature {
            self.pending_characters_refresh = true;
        }
    }

    /// Tracks bubble lifecycle and prunes footer caches, applying footer defaults to newly
    /// detected bubbles.
    ///
    /// Also refreshes the chapter-collected half of the footer character autocomplete base
    /// (`sync_chapter_character_names`), which is derived from the same snapshot.
    ///
    /// Runs every frame from `draw_canvas_overlay_top_left`, so the expensive recompute (full
    /// bubble snapshot clone + id `HashSet` + recent-character history + chapter names) is gated on
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
        // Reuse the snapshot that is already built here — it is the whole chapter plus the
        // runtime-only bubbles, and `hook_bubbles_snapshot` clones the entire vector, so a second
        // call for the character names would double the cost of every revision change.
        //
        // Deliberate asymmetry with `collect_recent_character_history` below, which reads
        // `project.bubbles` (no runtime-only bubbles): a name on a just-created bubble that has
        // not reached the model yet is offered by the autocomplete but is not yet a card in
        // «Последние персонажи». Suggestions should be as eager as possible; the six ranked cards
        // are a digit-addressed history and should not reshuffle around a bubble that may still
        // vanish before it is ever flushed.
        self.sync_chapter_character_names(&live_bubbles);
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

    /// Creates a text bubble under the pointer and, with `recent_rank` (the recent-character
    /// parameter of the create command), seeds it with that recent character. Returns whether a
    /// bubble was created.
    pub fn create_bubble_at_pointer_shortcut(
        &mut self,
        canvas: &mut CanvasView,
        project: &ProjectData,
        pointer_pos: Pos2,
        recent_rank: Option<usize>,
    ) -> bool {
        if !canvas.create_bubble_at_pointer_shortcut(pointer_pos) {
            return false;
        }
        if let Some(rank) = recent_rank {
            let _ = self.apply_recent_character_rank_to_latest_bubble(canvas, project, rank);
        }
        true
    }

    /// Creates a hint bubble under the pointer; returns whether one was created.
    ///
    /// Unlike [`Self::create_bubble_at_pointer_shortcut`] there is no recent-character seeding:
    /// a hint carries no character. Executed for `crate::input::CREATE_HINT_BUBBLE`.
    pub fn create_hint_bubble_at_pointer_shortcut(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        pointer_pos: Pos2,
    ) -> bool {
        canvas.create_hint_bubble_at_pointer_shortcut(ctx, pointer_pos)
    }

    /// Resolves the image-bubble «choose file» dialog once it closed: copies the picked file into
    /// the staging image-bubble folder and queues the `image_path` footer patch. A cancel is a
    /// no-op; a dialog or copy failure becomes an error toast.
    fn poll_image_bubble_pick(&mut self, ctx: &egui::Context, project: &ProjectData, now_s: f64) {
        let Some(pick) = self.image_bubble_pick.as_mut() else {
            return;
        };
        let Some(result) = pick.poll() else {
            return;
        };
        let bubble_id = pick.bubble_id();
        self.image_bubble_pick = None;
        let source = match result {
            Ok(Some(picked)) => match picked.into_file() {
                Some(file) => file.path,
                // `OpenFile` always answers with one file (`Picked` contract); anything else is
                // a platform-layer bug, logged and treated as a cancel.
                None => {
                    ms_log::runtime_log::log_error(format!("[translation] image bubble {bubble_id}: file dialog returned no file for an OpenFile request"));
                    return;
                }
            },
            Ok(None) => return,
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[translation] image bubble {bubble_id}: file dialog failed: {err:?}"));
                self.push_toast(ctx, err.user_message(), Severity::Error, 3.0);
                return;
            }
        };
        match copy_external_image_bubble(project, bubble_id, &source) {
            Ok(saved) => self.queue_footer_patch(bubble_id, "image_path", Value::String(project_relative_path(project, &saved)), now_s),
            Err(err) => self.push_toast(ctx, err, Severity::Error, 3.0),
        }
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
        !trimmed.is_empty() && trimmed != footer_no_character() && trimmed != footer_no_characters()
    }

    fn poll_ocr_events(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        for event in self.ocr_controller.poll_events() {
            match event {
                OcrControllerEvent::ExternalModelMissing(specs) => {
                    // The gate's error names the button of the disk state it just probed;
                    // re-probe so the panel draws that same button (it may still show a
                    // stale `Installed` or an older state).
                    self.ocr_model_downloads.request_bundle_status(&specs);
                }
                OcrControllerEvent::StateChanged(state) => {
                    // Any other state ends the command's download (finished, cancelled, failed).
                    if state != OcrLoadState::DownloadingModel {
                        self.ocr_model_download.clear();
                    }
                    match state {
                        OcrLoadState::DownloadingModel => {
                            let engine = self
                                .ocr_loading_engine
                                .unwrap_or(self.ocr_panel_options.engine);
                            self.mark_ocr_model_download_started_for_engine(engine);
                            self.push_toast(
                                ctx,
                                t!("translation.common.downloading_model_status").to_string(),
                                Severity::Info,
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
                                t!("translation.tab.engine_loading_status").to_string(),
                                Severity::Info,
                                2.2,
                            );
                        }
                        OcrLoadState::Error => {
                            let engine = self
                                .ocr_loading_engine
                                .take()
                                .unwrap_or(self.ocr_panel_options.engine);
                            self.set_ocr_state_for_engine(engine, OcrLoadState::Error);
                            // A load error of an external-model engine may come from its files
                            // (deleted, damaged): refresh the panel's install state too.
                            let options = &self.ocr_panel_options;
                            let specs = external_model_specs(engine, options.paddle_vl_model, &options.easy_langs);
                            self.ocr_model_downloads.request_bundle_status(&specs);
                            self.push_toast(
                                ctx,
                                t!("translation.tab.engine_load_error").to_string(),
                                Severity::Error,
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
                        // A load whose model download was cancelled: the engine is not loaded
                        // (the next request loads it again and resumes the staged bytes).
                        OcrLoadState::NotLoaded => {
                            if let Some(engine) = self.ocr_loading_engine.take() {
                                self.set_ocr_state_for_engine(engine, OcrLoadState::NotLoaded);
                            }
                            self.push_toast(
                                ctx,
                                t!("translation.tab.model_download_cancelled_status").to_string(),
                                Severity::Info,
                                2.2,
                            );
                        }
                    }
                }
                // The state already reads `DownloadingModel`; the panel draws the latest event.
                OcrControllerEvent::ModelDownloadProgress(event) => {
                    self.ocr_model_download.record(event);
                    ctx.request_repaint();
                }
                OcrControllerEvent::Recognized { request_id, result, region } => {
                    self.receive_ocr_region(ctx, request_id, region);
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
                                t!("translation.tab.selection_recognition_done_status").to_string(),
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
                            t!("translation.tab.quick_ocr_no_text_status").to_string()
                        } else {
                            tf!("translation.tab.quick_ocr_result_status", manual_text = manual_text.trim())
                        };
                        self.push_toast(ctx, quick_msg, Severity::Success, 3.0);
                    }
                    if let Some(pending_insert) = self.pending_bubble_inserts.remove(&request_id) {
                        self.insert_recognized_bubble(canvas, project, &pending_insert, &result);
                    }
                    if self.textdetector_ocr_active_request_id == Some(request_id) {
                        self.textdetector_ocr_active_request_id = None;
                        self.textdetector_ocr_active_task = None;
                        self.textdetector_ocr_active_engine = None;
                        self.textdetector_ocr_retry_state = None;
                        self.textdetector_ocr_done = self.textdetector_ocr_done.saturating_add(1);
                        if !result.text.trim().is_empty() {
                            self.textdetector_ocr_recognized =
                                self.textdetector_ocr_recognized.saturating_add(1);
                        }
                        self.set_text_detector_status(
                            t!("translation.tab.block_recognition_status"),
                            Severity::Info,
                        );
                    }
                    let msg = if result.lines.is_empty() {
                        t!("translation.tab.recognition_no_text_status").to_string()
                    } else {
                        tf!("translation.tab.recognition_lines_status", lines = result.lines.len())
                    };
                    self.push_toast(ctx, msg, Severity::Success, 2.2);
                }
                OcrControllerEvent::RecognizeFailed { request_id, error } => {
                    self.drop_ocr_region(request_id, tf!("translation.tab.region_detect_error", error = error), Severity::Error);
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
                        let engine = self.textdetector_ocr_active_engine.take();
                        if let Some(task) = self.textdetector_ocr_active_task.take().and_then(|task| block_ocr_retry_after_failure(task, engine)) {
                            let now_s = ctx.input(|i| i.time);
                            self.textdetector_ocr_retry_state = Some(TextDetectorOcrRetryState {
                                task,
                                retry_at_s: now_s + TEXT_DETECTOR_OCR_RETRY_DELAY_SECS,
                            });
                            self.set_text_detector_status(
                                tf!(
                                    "translation.tab.recognition_error_retry_status",
                                    sec = format!("{:.0}", TEXT_DETECTOR_OCR_RETRY_DELAY_SECS)
                                ),
                                Severity::Warning,
                            );
                            self.push_toast(
                                ctx,
                                t!("translation.tab.recognition_retry_status").to_string(),
                                Severity::Warning,
                                2.8,
                            );
                            continue;
                        }
                        self.abort_textdetector_ocr(
                            ctx,
                            tf!("translation.tab.recognition_stopped_error", error = error),
                            Severity::Error,
                        );
                    }
                    if manual_target == Some(ManualOcrResultTarget::ToastAndClipboard) {
                        self.push_toast(ctx, tf!("translation.tab.quick_ocr_error", error = error), Severity::Error, 3.0);
                    } else if !adv_rec_error_applied {
                        self.push_toast(
                            ctx,
                            tf!("translation.tab.recognition_error", error = error),
                            Severity::Error,
                            3.0,
                        );
                    }
                }
                // A user stop: drop the request's pending targets like a failure, but report it
                // neutrally (no error toast, no detector retry).
                OcrControllerEvent::RecognizeCancelled { request_id } => {
                    self.drop_ocr_region(request_id, t!("translation.tab.recognition_cancelled_status").to_string(), Severity::Info);
                    if self.manual_ocr_active_request_id == Some(request_id) {
                        self.manual_ocr_active_request_id = None;
                        self.manual_ocr_result_target = None;
                    }
                    self.pending_bubble_inserts.remove(&request_id);
                    let stopped = t!("translation.tab.recognition_cancelled_status").to_string();
                    self.advanced_recognition.apply_recognition_error(request_id, stopped.clone());
                    if self.textdetector_ocr_active_request_id == Some(request_id) {
                        self.abort_textdetector_ocr(ctx, stopped.clone(), Severity::Info);
                    }
                    self.push_toast(ctx, stopped, Severity::Info, 2.2);
                }
            }
        }
    }

    /// The OCR panel's generation "Stop" on the generation `shown`: aborts it if still in
    /// flight and, when its request (`shown.request_tag` = OCR request id) is the detector
    /// block batch's active one, stops the whole batch (neutral status).
    fn stop_ocr_generation(&mut self, ctx: &egui::Context, shown: GenerationSnapshot) {
        self.ocr_controller.cancel_generation(shown.run_id);
        if self.textdetector_ocr_active_request_id == Some(shown.request_tag) {
            self.abort_textdetector_ocr(ctx, t!("translation.tab.recognition_cancelled_status").to_string(), Severity::Info);
        }
    }

    fn poll_text_detector_events(&mut self, ctx: &egui::Context, canvas: &mut CanvasView, project: &ProjectData) {
        // Regions held back by a storage job that has ended since.
        self.replay_pending_region_detections(ctx);
        for event in self.text_detector_controller.poll_events() {
            // Pages and the batch end follow a finished (or stopped) download.
            if !matches!(event, TextDetectorControllerEvent::ModelDownloadStarted | TextDetectorControllerEvent::ModelDownloadProgress(_)) {
                self.text_detector_model_download.clear();
            }
            match event {
                TextDetectorControllerEvent::ModelDownloadStarted => {
                    self.set_text_detector_status(
                        t!("translation.common.downloading_model_status"),
                        Severity::Info,
                    );
                }
                TextDetectorControllerEvent::DetectStarted { total, replace } => {
                    if replace {
                        self.clear_detector_result_stores();
                    }
                    self.text_detector_progress = Some((0, total));
                    self.set_text_detector_status(
                        self.text_detector_running_status(),
                        Severity::Info,
                    );
                }
                TextDetectorControllerEvent::PageDetected { page_idx, result, texts } => {
                    // NeuroLate "recognize text right away": the block texts of this page become
                    // bubbles exactly as block OCR makes them.
                    if let Some(texts) = texts {
                        self.create_bubbles_from_page_texts(canvas, project, page_idx, &result, &texts);
                    }
                    self.text_detector_results.insert(page_idx, result);
                    self.text_detector_mask_textures.remove(&page_idx);
                    self.sync_text_mask_page_from_result(page_idx);
                }
                TextDetectorControllerEvent::PageFailed { page_idx, error } => {
                    self.push_toast(
                        ctx,
                        tf!("translation.tab.detector_page_skipped_status", page_idx = page_idx, error = error),
                        Severity::Warning,
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
                        tf!("translation.tab.detector_done_blocks_status", total_blocks = total_blocks)
                    } else {
                        tf!("translation.tab.detector_done_blocks_errors_status", total_blocks = total_blocks, failed_pages = failed_pages)
                    };
                    let severity = if failed_pages == 0 {
                        Severity::Success
                    } else {
                        Severity::Warning
                    };
                    self.set_text_detector_status(status, severity);
                }
                TextDetectorControllerEvent::DetectFailed { error } => {
                    self.text_detector_progress = None;
                    // A Surya batch fails up front when its checkpoint is missing: re-probe
                    // so the panel draws the button that error names.
                    if self.text_detector_options.algorithm == TextDetectorAlgorithm::Surya {
                        self.ocr_model_downloads.request_bundle_status(&SURYA_DETECTOR_SPECS);
                    }
                    self.set_text_detector_status(
                        tf!("translation.tab.detector_error", error = error),
                        Severity::Error,
                    );
                }
                // `ModelDownloadStarted` already set the status; the panel draws the event.
                TextDetectorControllerEvent::ModelDownloadProgress(event) => {
                    self.text_detector_model_download.record(event);
                    ctx.request_repaint();
                }
                TextDetectorControllerEvent::DetectCancelled => {
                    self.text_detector_progress = None;
                    self.set_text_detector_status(t!("translation.tab.detector_cancelled_status"), Severity::Info);
                }
                TextDetectorControllerEvent::RegionDetected(region) => {
                    self.receive_region_detection(ctx, region);
                }
                TextDetectorControllerEvent::RegionFailed { page_idx, engine, error } => {
                    ms_log::runtime_log::log_error(format!("[text-detector] region detection on page {page_idx} failed: {error}"));
                    self.region_detection_failed(ctx, engine, error);
                }
            }
        }
    }

    fn ensure_text_detection_storage_loaded(&mut self, project: &ProjectData) {
        if !self.session_caps.chapter_storage {
            return;
        }
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
        self.set_text_detector_status(t!("translation.tab.loading_saved_mask_status"), Severity::Info);

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
        // The Save button is not drawn without storage; this refuses an action that got here
        // anyway rather than writing results into a scratch chapter deleted on exit.
        if !self.session_caps.chapter_storage || self.text_detection_storage_busy {
            return;
        }
        // Save what the user sees: the shared model holds the shown masks (brush edits
        // included), the tab's store the blocks. Only the masks to write are copied under the
        // lock; the files are written on a worker.
        let pages = match self.text_mask_model.as_ref().map(|model| model.lock()) {
            Some(Ok(model)) => detector_region::detection_pages_for_save(&self.text_detector_results, Some(&model)),
            Some(Err(error)) => {
                ms_log::runtime_log::log_error(format!("[text-detector] text mask model lock poisoned, saving detector results without shown masks: {error}"));
                detector_region::detection_pages_for_save(&self.text_detector_results, None)
            }
            None => detector_region::detection_pages_for_save(&self.text_detector_results, None),
        };
        if pages.is_empty() {
            self.set_text_detector_status(
                t!("translation.tab.no_results_to_save_status"),
                Severity::Error,
            );
            return;
        }
        let project_dir = project.paths.project_dir.clone();
        let storage_dir = project.paths.text_detection_dir.clone();
        let (tx, rx) = mpsc::channel::<TextDetectionStorageEvent>();
        self.text_detection_storage_busy = true;
        self.text_detection_storage_rx = Some(rx);
        self.set_text_detector_status(t!("translation.tab.saving_mask_status"), Severity::Info);

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
                // `loaded_for` stays unset, so the next region dispatch or detector panel draw
                // runs the load again; held regions wait for it (`region_merge_must_wait`).
                if !self.pending_region_detections.is_empty() {
                    ms_log::runtime_log::log_warn(format!(
                        "[text-detector] stored detection job died; {} region result(s) wait for the stored detection to load",
                        self.pending_region_detections.len()
                    ));
                }
                self.set_text_detector_status(
                    t!("translation.tab.text_detection_aborted_status"),
                    Severity::Error,
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
                self.apply_loaded_detection_pages(pages);
                self.text_detection_storage_loaded_for = Some(project_dir);
                let status = if failed == 0 {
                    tf!("translation.tab.loaded_saved_pages_status", loaded = loaded)
                } else {
                    tf!("translation.tab.load_progress_status", loaded = loaded, failed = failed)
                };
                let severity = if failed == 0 {
                    Severity::Success
                } else {
                    Severity::Warning
                };
                self.set_text_detector_status(status, severity);
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
                    tf!("translation.tab.saved_pages_status", saved = saved)
                } else {
                    tf!("translation.tab.save_progress_status", saved = saved, failed = failed)
                };
                let severity = if failed == 0 {
                    Severity::Success
                } else {
                    Severity::Warning
                };
                self.set_text_detector_status(status, severity);
            }
            TextDetectionStorageEvent::Failed { project_dir, error } => {
                if project_dir != current_project_dir {
                    return;
                }
                self.text_detection_storage_loaded_for = Some(project_dir);
                self.set_text_detector_status(
                    tf!("translation.tab.text_detection_error", error = error),
                    Severity::Error,
                );
            }
        }
    }

    /// Empties both detector stores, their mask textures and the region results still waiting
    /// for a storage job (they belong to the results being cleared and must not come back).
    fn clear_detector_result_stores(&mut self) {
        self.text_detector_results.clear();
        self.text_detector_mask_textures.clear();
        self.clear_text_mask_model();
        self.pending_region_detections.clear();
        self.pending_ocr_regions.clear();
    }

    /// Applies the chapter's stored detection: fills only pages that hold no result yet (a page
    /// detected in this session wins) and mirrors each into the shared model.
    fn apply_loaded_detection_pages(&mut self, pages: Vec<(usize, TextDetectorPageResult)>) {
        for (page_idx, result) in pages {
            if let std::collections::hash_map::Entry::Vacant(entry) = self.text_detector_results.entry(page_idx) {
                entry.insert(result);
                self.text_detector_mask_textures.remove(&page_idx);
                self.sync_text_mask_page_from_result(page_idx);
            }
        }
    }

    /// Replaces the detector panel status line; `severity` selects its themed colour
    /// (`Info` for in-progress work, `Warning` for partial results/retries).
    fn set_text_detector_status(&mut self, text: impl Into<String>, severity: Severity) {
        self.text_detector_status = text.into();
        self.text_detector_status_severity = severity;
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
        // The stored mask is the RAW backend glyph mask (`result.mask_alpha`), not a
        // raster of the expanded/merged blocks, so mirror the RAW detector boxes
        // (`result.blocks`) alongside it. Autoclean's box candidate must match what the
        // mask represents; resolved boxes belong only to the synthesized-rect path in
        // `materialize_text_mask_page_from_blocks_if_missing`.
        let blocks = detector_rects_to_blocks_px(&result.blocks);
        if let Some(model) = self.text_mask_model.as_ref()
            && let Ok(mut model) = model.lock()
        {
            model.set_page_with_blocks(
                page_idx,
                result.source_size,
                result.mask_size,
                result.mask_alpha.clone(),
                blocks,
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
        let Some((mask_alpha, resolved)) = rasterize_resolved_blocks(result, &self.text_detector_options) else {
            return;
        };
        // The synthesized mask IS a raster of the resolved (expanded/merged) blocks, so
        // store those same resolved blocks as the page's box set — they match the mask.
        let mask_size = result.source_size;
        let blocks = detector_rects_to_blocks_px(&resolved);
        let source_size = result.source_size;
        if let Some(model) = self.text_mask_model.as_ref()
            && let Ok(mut model) = model.lock()
        {
            model.set_page_with_blocks(page_idx, source_size, mask_size, mask_alpha, blocks);
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

    /// Builds the run mode for the selected algorithm (`TextDetectorPanelOptions::run_mode`,
    /// the same mode the panel's plan notice uses), or the localized refusal text when
    /// [`TextDetectorAlgorithm::availability`] says it cannot run now.
    fn text_detector_run_mode(&self, project: &ProjectData) -> Result<TextDetectorRunMode, String> {
        let algorithm = self.text_detector_options.algorithm;
        self.text_detector_availability().map_err(|reason| reason.status_text(algorithm))?;
        Ok(self.text_detector_options.run_mode(self.neurolate_detect(project)))
    }

    fn text_detector_running_status(&self) -> &'static str {
        match self.text_detector_options.algorithm {
            TextDetectorAlgorithm::Classic => t!("translation.tab.detecting_status"),
            TextDetectorAlgorithm::PaddleOcr => t!("translation.tab.detecting_paddle_status"),
            TextDetectorAlgorithm::Ai => t!("translation.tab.detecting_ai_status"),
            TextDetectorAlgorithm::Surya => t!("translation.tab.detecting_surya_status"),
            TextDetectorAlgorithm::NeuroLate => t!("translation.tab.detecting_neurolate_status"),
        }
    }

    fn start_text_detection_for_current_page(
        &mut self,
        project: &ProjectData,
        canvas: &CanvasView,
    ) {
        if self.detector_busy_for_batches() {
            return;
        }
        let page_idx = canvas.current_page_idx();
        let Some(page) = project.pages.iter().find(|page| page.idx == page_idx) else {
            self.set_text_detector_status(t!("translation.tab.current_page_not_found_status"), Severity::Error);
            return;
        };
        let mode = match self.text_detector_run_mode(project) {
            Ok(mode) => mode,
            Err(error) => {
                self.set_text_detector_status(error, Severity::Error);
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
                    Severity::Info,
                );
            }
            Err(error) => {
                self.set_text_detector_status(
                    tf!("translation.tab.detector_start_error", error = error),
                    Severity::Error,
                );
            }
        }
    }

    fn start_text_detection_for_all_pages(&mut self, project: &ProjectData) {
        if self.detector_busy_for_batches() {
            return;
        }
        let pages = project
            .pages
            .iter()
            .map(|page| (page.idx, page.path.clone()))
            .collect::<Vec<_>>();
        let mode = match self.text_detector_run_mode(project) {
            Ok(mode) => mode,
            Err(error) => {
                self.set_text_detector_status(error, Severity::Error);
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
                    Severity::Info,
                );
            }
            Err(error) => {
                self.set_text_detector_status(
                    tf!("translation.tab.detector_start_error", error = error),
                    Severity::Error,
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
        if let Err(off) = self.current_ocr_route_availability() {
            self.push_toast(ctx, off.user_message(), Severity::Warning, 2.6);
            return;
        }
        if let Some(error) = self.current_ocr_torch_requirement_error() {
            self.push_toast(ctx, error, Severity::Error, 2.6);
            return;
        }
        if self.ocr_controller.state() != OcrLoadState::Ready {
            self.set_text_detector_status(
                t!("translation.tab.engine_not_loaded_status"),
                Severity::Error,
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
            for rect in blocks {
                let Some(uv_rect) = block_bubbles::block_uv_rect(&rect, result.source_size) else {
                    continue;
                };
                tasks.push_back(TextDetectorOcrTask { page_idx, uv_rect, retry_attempt: 0 });
            }
        }

        if tasks.is_empty() {
            self.set_text_detector_status(t!("translation.tab.no_blocks_status"), Severity::Error);
            self.text_detector_progress = None;
            return;
        }

        self.pending_textdetector_ocr_tasks = tasks;
        self.textdetector_ocr_active_request_id = None;
        self.textdetector_ocr_active_task = None;
        self.textdetector_ocr_active_engine = None;
        self.textdetector_ocr_retry_state = None;
        self.textdetector_ocr_total = self.pending_textdetector_ocr_tasks.len();
        self.textdetector_ocr_done = 0;
        self.textdetector_ocr_recognized = 0;
        self.text_detector_progress = Some((0, self.textdetector_ocr_total));
        self.set_text_detector_status(t!("translation.tab.block_recognition_status"), Severity::Info);
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
            self.set_text_detector_status(t!("translation.tab.block_recognition_status"), Severity::Info);
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
                options: build_ocr_runtime_options(&self.ocr_panel_options, &self.mt_panel_options.source_lang, &self.neurolate_settings, project.comic_type),
                page_path: page.path.clone(),
                uv_rect: task.uv_rect,
                image_override_png: None,
                join_newlines: self.ocr_panel_options.join_newlines,
                reflect_strings: self.ocr_panel_options.reflect_strings,
                char_replacements: self.ocr_panel_options.runtime_char_replacements(),
                fix_caps_lock: self.ocr_panel_options.fix_caps_lock,
                // Block OCR reads text only (NeuroLate: `TEXT` mode), never a region.
                region: None,
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
                        tf!(
                            "translation.tab.recognition_unavailable_retry_status",
                            sec = format!("{:.0}", TEXT_DETECTOR_OCR_RETRY_DELAY_SECS)
                        ),
                        Severity::Warning,
                    );
                    break;
                }
                self.abort_textdetector_ocr(
                    ctx,
                    t!("translation.tab.recognition_start_stopped_status").to_string(),
                    Severity::Error,
                );
                break;
            }
            self.textdetector_ocr_active_request_id = Some(request_id);
            self.textdetector_ocr_active_task = Some(task);
            self.textdetector_ocr_active_engine = Some(self.ocr_panel_options.engine);
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
        self.textdetector_ocr_active_engine = None;
        self.textdetector_ocr_retry_state = None;
        self.pending_textdetector_ocr_tasks.clear();
        self.text_detector_progress = None;

        if force_error {
            self.set_text_detector_status(
                t!("translation.tab.engine_not_loaded_status"),
                Severity::Error,
            );
            return;
        }

        if recognized > 0 {
            self.set_text_detector_status(
                tf!("translation.tab.recognition_done_blocks_status", recognized = recognized, total = total),
                Severity::Success,
            );
            self.push_toast(
                ctx,
                tf!("translation.tab.detector_recognition_progress_status", recognized = recognized, total = total),
                Severity::Success,
                2.4,
            );
        } else {
            self.set_text_detector_status(
                t!("translation.tab.recognition_done_no_text_status"),
                Severity::Warning,
            );
        }
    }

    /// Hard-resets the detector block-recognition batch and shows `message` with `severity`
    /// (`Error` after a fatal condition, `Info` for a user stop).
    fn abort_textdetector_ocr(&mut self, ctx: &egui::Context, message: String, severity: Severity) {
        self.textdetector_ocr_total = 0;
        self.textdetector_ocr_done = 0;
        self.textdetector_ocr_recognized = 0;
        self.textdetector_ocr_active_request_id = None;
        self.textdetector_ocr_active_task = None;
        self.textdetector_ocr_active_engine = None;
        self.textdetector_ocr_retry_state = None;
        self.pending_textdetector_ocr_tasks.clear();
        self.text_detector_progress = None;
        self.set_text_detector_status(message, severity);
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
        if !self.detector_overlay_visible() {
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
        page_idx: usize,
        page_rect: Rect,
    ) {
        if !self.detector_overlay_visible() {
            return;
        }
        let clip_rect = ui.clip_rect().intersect(page_rect);
        if !clip_rect.is_positive() {
            return;
        }
        let painter = ui.painter().with_clip_rect(clip_rect);

        if self.text_detector_edit_lines_mode {
            self.draw_text_detector_line_edit_overlay_on_page(ui, &painter, page_idx, page_rect);
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

    /// Adds a line centred at `uv` (page-normalized) on page `page_idx`, creating the page's
    /// detector entry first when it has none, and selects it.
    fn create_text_detector_line_at_uv(
        &mut self,
        project: &ProjectData,
        page_idx: usize,
        uv: Pos2,
    ) -> Result<(), String> {
        self.ensure_text_detector_page_entry(project, page_idx)?;
        self.add_text_detector_line_at_uv(page_idx, uv)
    }

    /// Adds a line centred at `uv` (page-normalized) to the EXISTING detector entry of page
    /// `page_idx` and selects it. Errors when the page has no entry or the rect is degenerate.
    fn add_text_detector_line_at_uv(&mut self, page_idx: usize, uv: Pos2) -> Result<(), String> {
        let next_idx = {
            let result = self
                .text_detector_results
                .get_mut(&page_idx)
                .ok_or_else(|| tf!("translation.tab.prepare_detection_error", page_idx = page_idx))?;
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
            .ok_or_else(|| t!("translation.tab.create_line_invalid_coords_error").to_string())?;
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
                .ok_or_else(|| tf!("translation.tab.page_not_found_error", page_idx = page_idx))?;
            let (w, h) = image::image_dimensions(&page.path).map_err(|err| {
                tf!("translation.tab.page_image_size_error", page_idx = page_idx, page = page.path.display(), err = err)
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
            .ok_or_else(|| tf!("translation.tab.prepare_detection_error", page_idx = page_idx))
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
                            tf!("translation.tab.apply_translation_error", bubble_id = bubble_id),
                            Severity::Error,
                            2.4,
                        );
                    }
                }
                MtControllerEvent::ItemAreasTranslated { bubble_id, areas } => {
                    if !canvas.apply_machine_translation_areas(bubble_id, areas) {
                        self.push_toast(
                            ctx,
                            tf!("translation.tab.apply_area_translation_error", bubble_id = bubble_id),
                            Severity::Error,
                            2.4,
                        );
                    }
                }
                MtControllerEvent::RunFinished { translated, errors } => {
                    self.mt_progress = None;
                    let severity = if errors == 0 {
                        Severity::Success
                    } else {
                        Severity::Warning
                    };
                    self.push_toast(
                        ctx,
                        tf!("translation.tab.mt_progress_status", translated = translated, translated_2 = translated + errors, errors = errors),
                        severity,
                        2.8,
                    );
                }
                MtControllerEvent::RunCancelled { translated, errors } => {
                    self.mt_progress = None;
                    self.push_toast(
                        ctx,
                        tf!("translation.tab.mt_cancelled_status", translated = translated, errors = errors),
                        Severity::Warning,
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
                            tf!("translation.tab.mt_error_status", error = error),
                            Severity::Error,
                            3.2,
                        );
                    }
                }
                // Arrives before the run's first request, so it shows until the first progress
                // toast replaces it; the worker already logged it with the path.
                MtControllerEvent::ContextNotice { notice } => {
                    self.push_toast(ctx, notice, Severity::Warning, 6.0);
                }
                MtControllerEvent::Progress {
                    translated,
                    errors,
                    total,
                    context_used_chars,
                    context_budget_chars,
                    pruned_replicas,
                } => {
                    // Replaces the whole progress, which also clears a finished limiter wait.
                    self.mt_progress = Some(MtPanelProgress {
                        translated,
                        errors,
                        total,
                        context_used_chars,
                        context_budget_chars,
                        pruned_replicas,
                        rate_limit_wait: None,
                    });
                    self.push_toast(
                        ctx,
                        tf!("translation.tab.ai_mt_progress_status", translated = translated, total = total, errors = errors, used = format_context_chars(context_used_chars), budget = format_context_chars(context_budget_chars), pruned_replicas = pruned_replicas),
                        Severity::Info,
                        2.4,
                    );
                }
                MtControllerEvent::RateLimitWait { wait } => {
                    self.mt_progress.get_or_insert_default().rate_limit_wait = wait;
                }
                MtControllerEvent::RunRateLimited { service, translated, errors, retry_in_secs } => {
                    self.mt_progress = None;
                    self.push_toast(
                        ctx,
                        tf!("translation.tab.mt_rate_limited_stop_status", service = service.title(), minutes = whole_minutes_ceil(retry_in_secs), translated = translated, errors = errors),
                        Severity::Warning,
                        6.0,
                    );
                }
            }
        }
    }

    /// Applies finished AI API connection requests (key save / delete, metadata refresh) of the
    /// OCR and MT panels: a replaced model marks that panel's settings dirty, every notice
    /// becomes a toast. Non-blocking.
    fn poll_ai_api_events(&mut self, ctx: &egui::Context) {
        let ocr = self.ocr_ai_api_tasks.poll_and_apply(&mut self.ocr_panel_options.ai_api);
        self.ocr_settings_dirty |= ocr.model_changed;
        let mt = self.mt_ai_api_tasks.poll_and_apply(&mut self.mt_panel_options.ai_api);
        self.mt_settings_dirty |= mt.model_changed;
        for notice in ocr.notices.into_iter().chain(mt.notices) {
            self.push_toast(ctx, notice.text, notice.severity, notice.duration_s);
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
                t!("translation.tab.mt_cancelled_notice").to_string(),
                Severity::Warning,
                2.2,
            );
        } else if had_pending {
            self.push_toast(
                ctx,
                t!("translation.tab.mt_deferred_cancelled_status").to_string(),
                Severity::Warning,
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
                t!("translation.tab.no_bubbles_for_mt_status").to_string(),
                Severity::Warning,
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
            // A hint is an author note to the translator, never a replica: it must not be sent
            // for translation, nor offered as context.
            if is_hint_bubble_record(bubble) {
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

    /// True when the AI per-ImageBubble mode is selected and usable (AI tab + a model not known
    /// to be text-only).
    fn ai_mt_imagebubble_mode_active(&self) -> bool {
        self.mt_panel_options.active_tab == MtPanelTab::AiApi
            && self.mt_panel_options.ai_image_mode == AiMtImageMode::ImagesOnly
            && image_input_support(&self.mt_panel_options.ai_api.model) != ImageInputSupport::NotSupported
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
            // A hint is an author note to the translator, never a replica: it is neither
            // translated nor used as ordered context.
            if is_hint_bubble_record(bubble) {
                continue;
            }
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
            // A hint is an author note to the translator, never a replica: an explicit per-id
            // request must not translate it either.
            if is_hint_bubble_record(bubble) {
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
        if self.mt_controller.is_busy() {
            self.push_toast(
                ctx,
                t!("translation.mt.already_running_status").to_string(),
                Severity::Warning,
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
            self.push_toast(ctx, tf!("translation.tab.mt_start_error", err = err), Severity::Error, 3.0);
        } else {
            // Drop any previous credit/limit notice as soon as a new run is accepted.
            self.mt_stop_notice = None;
        }
    }

    /// Shows `text` as the canvas toast for `duration_s` seconds (at least 0.2 s), replacing any
    /// current toast. `severity` is chosen by the MEANING of the message (`Info` = in progress,
    /// `Success`, `Warning` = nothing done / partial, `Error` = failed); it selects the colour.
    fn push_toast(&mut self, ctx: &egui::Context, text: String, severity: Severity, duration_s: f64) {
        let now = ctx.input(|i| i.time);
        self.ocr_toast = Some(OcrToast {
            text,
            severity,
            hide_at_s: now + duration_s.max(0.2),
        });
    }

    /// Builds the AI MT options from the current panel state. `project` is cloned into the options
    /// because the worker thread needs an owned snapshot for image loading. Only meaningful when the
    /// AI API tab is active.
    fn current_ai_mt_options(&self, project: &ProjectData) -> AiMtOptions {
        AiMtOptions {
            service: self.mt_panel_options.ai_api.service,
            base_url: self.mt_panel_options.ai_api.base_url.clone(),
            model: self.mt_panel_options.ai_api.model.clone(),
            system_instruction: self.mt_panel_options.ai_api.system_instruction.clone(),
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
            strict_json: self.mt_panel_options.ai_strict_json,
            session_caps: self.session_caps,
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
        let items = self.collect_ai_mt_scope_items(canvas, project, current_page_only);
        if !items.iter().any(|item| item.needs_translation) {
            self.push_toast(
                ctx,
                t!("translation.tab.no_bubbles_preview_status").to_string(),
                Severity::Warning,
                2.3,
            );
            return;
        }
        let options = self.current_ai_mt_options(project);
        let source_lang = normalized_lang_input(&self.mt_panel_options.source_lang, "auto");
        let target_lang = normalized_lang_input(&self.mt_panel_options.target_lang, "ru");
        let scope_label = if current_page_only {
            t!("translation.tab.scope_current_page")
        } else {
            t!("translation.tab.scope_whole_project")
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
            t!("translation.tab.preparing_request_status").to_string(),
            Severity::Info,
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
                    tf!("translation.tab.build_request_error", error = error),
                    Severity::Error,
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
        egui::Window::new(t!("translation.tab.ai_request_window_title")).id(egui::Id::new("translation.tab.ai_request_window_title"))
            .open(&mut open)
            .resizable(true)
            .default_size([720.0, 640.0])
            .show(ctx, |ui| {
                ui.label(tf!("translation.tab.request_preview_summary", scope_label = scope_label, batch_total = preview.batch_total, translate = preview.translate_count, context = preview.context_count, items_total = preview.total_item_count, images = preview.image_count, kib = preview.image_bytes / 1024));
                if preview.batch_total > 1 {
                    ui.colored_label(
                        ms_theme::status::WARNING,
                        t!("translation.tab.request_preview_context_hint"),
                    );
                }
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.heading(t!("translation.tab.system_prompt_label"));
                        selectable_monospace(ui, &preview.system_prompt);
                        ui.add_space(10.0);
                        ui.separator();
                        ui.heading(t!("translation.tab.user_message_label"));
                        let mut image_idx = 0usize;
                        for part in &preview.parts {
                            match part {
                                MtRequestPreviewPart::Text(text) => {
                                    selectable_monospace(ui, text);
                                    ui.add_space(6.0);
                                }
                                MtRequestPreviewPart::Image(image) => {
                                    ui.label(tf!("translation.tab.bubble_image_summary", image = image.bubble_id, image_2 = image.width, image_3 = image.height, image_4 = image.png_byte_len / 1024));
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
            && image_input_support(&self.mt_panel_options.ai_api.model) != ImageInputSupport::NotSupported
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
                    .stroke(Stroke::new(1.0, toast.severity.color()))
                    .show(ui, |ui| {
                        ui.colored_label(toast.severity.color(), toast.text);
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
        let title = tf!("translation.tab.loading_images_status", loaded = status.loaded_pages, total = status.total_pages);
        let details = if status.load_errors_count > 0 {
            tf!("translation.tab.load_errors_status", errors = status.load_errors_count)
        } else {
            t!("translation.tab.preparing_pages_status").to_string()
        };

        egui::Area::new("translation_startup_page_load_toast".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center_top() + egui::vec2(-160.0, 16.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    // In-progress notice: same `Info` severity as the "loading" canvas toasts.
                    .stroke(Stroke::new(1.0, Severity::Info.color()))
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
            &self.mt_panel_options.source_lang,
            &self.neurolate_settings,
        ) else {
            self.push_toast(
                ctx,
                t!("translation.tab.selection_no_pages_status").to_string(),
                Severity::Warning,
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
                self.push_toast(ctx, error, Severity::Error, 3.0);
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
            self.push_toast(ctx, error, Severity::Error, 2.6);
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
            self.push_toast(ctx, t!("translation.tab.engine_loading_status").to_string(), Severity::Info, 2.2);
        }
    }

    fn handle_advanced_recognition_window(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        let Some(action) = self.advanced_recognition.draw(ctx, self.input_frame.as_ref(), &self.input_labels) else {
            return;
        };
        match action {
            AdvancedRecognitionAction::Recognize { image_override_png } => {
                if let Some(refusal) = self.ocr_request_refusal() {
                    self.push_toast(ctx, refusal, Severity::Error, 2.6);
                    return;
                }
                if let Some(error) = self.current_ocr_torch_requirement_error() {
                    self.push_toast(ctx, error, Severity::Error, 2.6);
                    return;
                }
                if self.manual_ocr_active_request_id.is_some() {
                    return;
                }
                let Some(mut built_request) = self.advanced_recognition_request.clone() else {
                    self.push_toast(
                        ctx,
                        t!("translation.tab.no_selection_status").to_string(),
                        Severity::Warning,
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
                        .unwrap_or(t!("translation.tab.start_recognition_error"))
                        .to_string();
                    let _ = self
                        .advanced_recognition
                        .apply_recognition_error(request_id, error);
                }
            }
            AdvancedRecognitionAction::QuickRecognizeSelection { image_override_png } => {
                if let Some(refusal) = self.ocr_request_refusal() {
                    self.push_toast(ctx, refusal, Severity::Error, 2.6);
                    return;
                }
                if let Some(error) = self.current_ocr_torch_requirement_error() {
                    self.push_toast(ctx, error, Severity::Error, 2.6);
                    return;
                }
                if self.manual_ocr_active_request_id.is_some() {
                    return;
                }
                let Some(mut built_request) = self.advanced_recognition_request.clone() else {
                    self.push_toast(
                        ctx,
                        t!("translation.tab.no_selection_status").to_string(),
                        Severity::Warning,
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
                        .unwrap_or(t!("translation.tab.start_recognition_error"))
                        .to_string();
                    let _ = self
                        .advanced_recognition
                        .apply_recognition_error(request_id, error.clone());
                    self.push_toast(ctx, tf!("translation.tab.quick_ocr_error", error = error), Severity::Error, 3.0);
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
                // The window's ORIGINAL page selection (never its edited / rotated preview),
                // with the engine parameters as they are now.
                if let Some(page) = project.pages.iter().find(|page| page.idx == page_idx) {
                    let options = build_ocr_runtime_options(&self.ocr_panel_options, &self.mt_panel_options.source_lang, &self.neurolate_settings, project.comic_type);
                    let engine = self.ocr_panel_options.engine;
                    let selection = detector_region::OcrSelection { page_idx, page_path: page.path.clone(), uv_rect };
                    // Never carried by an OCR answer: NeuroLate sends a separate `MASK` request.
                    self.start_selection_region_detection(ctx, project, engine, &options, selection, None);
                }
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
        if let Some(refusal) = self.ocr_request_refusal() {
            self.push_toast(ctx, refusal, Severity::Error, 2.6);
            return;
        }
        if let Some(error) = self.current_ocr_torch_requirement_error() {
            self.push_toast(ctx, error, Severity::Error, 2.6);
            return;
        }
        let request_id = self.next_ocr_request_id;
        self.next_ocr_request_id = self.next_ocr_request_id.saturating_add(1);

        let Some(mut built_request) = build_ocr_request(
            canvas,
            project,
            scene_rect,
            request_id,
            &self.ocr_panel_options,
            &self.mt_panel_options.source_lang,
            &self.neurolate_settings,
        ) else {
            self.push_toast(
                ctx,
                t!("translation.tab.selection_no_pages_status").to_string(),
                Severity::Warning,
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
                t!("translation.tab.ocr_already_running_status").to_string(),
                Severity::Warning,
                2.0,
            );
            return;
        }

        let region_engine = built_request.request.engine;
        let region_options = built_request.request.options.clone();
        let region_selection = detector_region::OcrSelection {
            page_idx: built_request.page_idx,
            page_path: built_request.request.page_path.clone(),
            uv_rect: built_request.request.uv_rect,
        };
        // NeuroLate: the same request also returns the region (none while the detector is busy).
        built_request.request.region = self.ocr_region_want_for(region_engine, &region_options, built_request.page_idx);
        let carried_by = built_request.request.region.is_some().then_some(request_id);
        self.dispatch_manual_ocr_request(
            ctx,
            built_request,
            pending_insert,
            ManualOcrResultTarget::AdvancedWindowText,
        );
        // The detector runs on the same selection only when the OCR itself was dispatched.
        if self.manual_ocr_active_request_id == Some(request_id) {
            self.start_selection_region_detection(ctx, project, region_engine, &region_options, region_selection, carried_by);
        }
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
                t!("translation.tab.image_source_ribbon_crop")
            } else {
                t!("translation.common.image_source_external")
            })
            .width(170.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut source_type,
                    "page_crop".to_string(),
                    t!("translation.tab.image_source_ribbon_crop"),
                );
                ui.selectable_value(
                    &mut source_type,
                    "external".to_string(),
                    t!("translation.common.image_source_external"),
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
            if ui.small_button(t!("translation.common.paste_image_button")).clicked() {
                match save_clipboard_image_bubble(project, bubble_id) {
                    Ok(path) => self.queue_footer_patch(
                        bubble_id,
                        "image_path",
                        Value::String(project_relative_path(project, &path)),
                        now_s,
                    ),
                    Err(err) => self.push_toast(ui.ctx(), err, Severity::Error, 3.0),
                }
            }
            // Disabled while any image-bubble dialog is open: one dialog at a time, and the
            // open one may belong to another bubble. Resolved by `poll_image_bubble_pick`.
            let picking = self.image_bubble_pick.is_some();
            if ui
                .add_enabled(!picking, egui::Button::new(t!("translation.common.choose_file_button")).small())
                .clicked()
            {
                self.image_bubble_pick = Some(ImageBubbleFilePick::request(ui.ctx(), bubble_id));
            }
        }
    }

    /// Draws the hint-bubble half of the canvas bubble footer: the single
    /// `canvas.bubble.hint_show_outside_label` checkbox, right after the shared replica-order
    /// spin box.
    ///
    /// The value is read from `extra["hint_show_outside_translation"]`, but a still-pending footer
    /// patch wins: writes are debounced (`FOOTER_PATCH_DEBOUNCE_SECS`), so reading the bubble alone
    /// would visibly bounce the checkbox back for a few frames after a click.
    fn build_hint_bubble_footer_controls(&mut self, ui: &mut egui::Ui, bubble: &Bubble, now_s: f64) {
        let bubble_id = bubble.id;
        let stored = bubble_extra_bool(&bubble.extra, HINT_SHOW_OUTSIDE_EXTRA_KEY, false);
        let mut show_outside = self
            .pending_footer_patches
            .get(&bubble_id)
            .and_then(|patch| patch.get(HINT_SHOW_OUTSIDE_EXTRA_KEY))
            .and_then(Value::as_bool)
            .unwrap_or(stored);
        if ui
            .checkbox(&mut show_outside, t!("canvas.bubble.hint_show_outside_label"))
            .on_hover_text(t!("canvas.bubble.hint_show_outside_tooltip"))
            .changed()
        {
            self.queue_footer_patch(
                bubble_id,
                HINT_SHOW_OUTSIDE_EXTRA_KEY,
                Value::Bool(show_outside),
                now_s,
            );
        }
    }
}

impl CanvasHooks for TranslationTabState {
    /// The create-bubble command's binding (`crate::input::CREATE_BUBBLE`), from this frame's
    /// labels; `None` while it is unbound.
    fn page_menu_create_bubble_shortcut(&self) -> Option<String> {
        self.input_labels().create_bubble_menu.clone()
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
            // A hint is not a replica: its single line lives in `text` and is never a translation,
            // so a status stripe for it would always read "translated". Same exemption as
            // `bubble_status_style`.
            .filter(|bubble| {
                bubble.bubble_class.as_deref().map(BubbleClass::from_str) != Some(BubbleClass::Hint)
            })
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
        _ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        _zoom: f32,
    ) {
        self.draw_text_detector_additional_overlay_on_page(ui, page_idx, image_rect);
    }

    fn draw_canvas_overlay_top_left(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: egui::Rect,
        canvas: &mut CanvasView,
        project: &ProjectData,
        status: CanvasUiStatus,
        panel_dock: &mut PanelDockState,
    ) {
        let _ = (
            &project.image_dir,
            status.loaded_pages,
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

        // The detector overlay shown outside its panel (OCR engine's "create mask" parameter)
        // needs the stored detection too; the load runs on its worker.
        if self.detector_overlay_visible() {
            self.ensure_text_detection_storage_loaded(project);
        }
        self.poll_text_detector_events(ctx, canvas, project);
        self.poll_text_detection_storage_events(project);
        self.poll_ocr_events(ctx, canvas, project);
        self.poll_ocr_model_downloads(ctx);
        self.poll_mt_events(ctx, canvas);
        self.poll_ai_api_events(ctx);
        self.poll_browser_extension(ctx);
        self.poll_neurolate_key(ctx);
        // The canvas-wide rect selections (mouse and finger alike) come from the router's
        // claim; the canvas raises its own capture area while one is armed.
        self.drive_canvas_selection(ctx, canvas, project);
        self.handle_advanced_recognition_window(ctx, canvas, project);
        self.maybe_dispatch_next_textdetector_ocr_request(ctx, project);
        self.handle_pending_mt_actions(ctx, canvas, project);
        self.poll_image_bubble_pick(ctx, project, now_s);
        self.flush_footer_patches(canvas, project, now_s);
        self.bubbles_panel.flush_text_updates(canvas, now_s);
        // The MT request preview outlives its tool body: polled and drawn on every frame, so it
        // keeps working while the MT tab is behind a sibling or switched off.
        self.poll_ai_mt_request_preview(ctx);
        self.draw_ai_mt_request_preview_window(ctx);
        // A pending rebuild (tool just switched on, settings just loaded) is done here rather
        // than in the body: the body runs only while its tab is in front, and the dock below
        // must see the fresh text on this very frame.
        if self.tools.is_on(TranslationTool::Composition) && self.composition_rebuild_requested {
            self.rebuild_composition_text(project);
        }

        let show_startup_load_toast =
            status.total_pages > 0 && status.loaded_pages < status.total_pages;
        let toast_top_offset = if show_startup_load_toast { 96.0 } else { 16.0 };
        self.draw_startup_page_load_toast(ctx, canvas_rect, status);
        self.draw_toast(ctx, canvas_rect, toast_top_offset);
        if self.text_detector_edit_lines_mode {
            let panel_pos = self.text_detector_edit_panel_pos(canvas_rect);
            egui::Area::new("translation_text_detector_line_edit_panel".into())
                .fixed_pos(panel_pos)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(Color32::from_rgb(26, 88, 40))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(88, 180, 110)))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(t!("translation.tab.lines_edit_mode_label"))
                                    .color(Color32::from_rgb(210, 255, 220))
                                    .strong(),
                            );
                            if ui.button(t!("translation.tab.exit_button")).clicked() {
                                self.set_text_detector_edit_lines_mode(false);
                            }
                        });
                });
        }
        if self.text_detector_edit_mask_mode {
            let panel_pos = self.text_detector_edit_panel_pos(canvas_rect);
            egui::Area::new("translation_text_detector_mask_edit_panel".into())
                .fixed_pos(panel_pos)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(Color32::from_rgb(104, 28, 28))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(212, 72, 72)))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(t!("translation.tab.mask_edit_mode_label"))
                                    .color(Color32::from_rgb(255, 220, 220))
                                    .strong(),
                            );
                            let mut radius = self.text_detector_mask_brush.radius_px();
                            if ui
                                .add(WheelSlider::new(&mut radius, 1..=200).text(t!("translation.tab.brush_px_label")))
                                .changed()
                            {
                                self.text_detector_mask_brush.set_radius_px(radius);
                            }
                            ui.small(tf!("translation.tab.paint_binding_hint", binding = self.input_labels.mask_paint));
                            ui.small(tf!("translation.tab.erase_binding_hint", binding = self.input_labels.mask_erase));
                            if ui.button(t!("translation.tab.exit_button")).clicked() {
                                self.set_text_detector_edit_mask_mode(false);
                            }
                        });
                });
        }
        if self.ocr_quick_selection_mode_active()
            || self.ocr_advanced_selection_mode_active()
            || self.canvas_selection.is_some()
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
        {
            ctx.request_repaint();
        }
        // The dock runs LAST in this hook, but NOT because that would win it the pointer: within
        // one `Order`, egui's layer list is persistent and re-sorted stably every pass
        // (`egui-0.35.0/src/memory/mod.rs:1215-1221`, `:1350`), so where in the frame an `Area` is
        // created is irrelevant. What raises a layer is `Area::begin`'s `move_to_top` for an area
        // that was not visible last frame (`egui-0.35.0/src/containers/area.rs:548-552`): the OCR
        // and image-crop capture surfaces above are full-canvas `Order::Foreground` areas, so each
        // time the user ENTERS one of those selection modes the surface rises above the dock's
        // panels and swallows clicks on them until the mode ends. Known and unchanged by the dock
        // migration — the old controls panel was on the lower `Order::Middle` and lost the same
        // clicks (`egui-docs/06-overlays.md` §1.1).
        //
        // It must run on EVERY frame this tab is active — the dock's detached sub-windows are
        // immediate viewports that only exist while `PanelDock::end` shows them
        // (`studio.rs::PanelDockPass`), so this hook has no early return before this point.
        self.draw_panel_dock(ctx, canvas_rect, canvas, project, status.total_pages, panel_dock);
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
                .on_hover_text(t!("translation.tab.replica_order_tooltip"));
            if order_resp.changed() {
                state.bubble_order = bubble_order.clamp(0, 100_000);
                self.queue_footer_patch(
                    bubble_id,
                    "bubble_order",
                    Value::Number(state.bubble_order.into()),
                    now_s,
                );
            }

            // Class dispatch AFTER the order spin box: every class keeps the replica order, but
            // only a text bubble gets the character field below it.
            match bubble.bubble_class.as_deref().map(BubbleClass::from_str) {
                Some(BubbleClass::Image) => {
                    self.build_image_bubble_footer_controls(ui, project, bubble, now_s);
                    return;
                }
                Some(BubbleClass::Hint) => {
                    self.build_hint_bubble_footer_controls(ui, bubble, now_s);
                    return;
                }
                Some(BubbleClass::Text) | None => {}
            }

            // Borrow `character_names` as a slice instead of cloning the whole Vec
            // every frame for every edited bubble. `character_names` and
            // `footer_character_autocomplete` are disjoint fields, so the immutable
            // suggestions borrow coexists with the mutable autocomplete entry borrow;
            // it ends at the `draw` call, before the later `queue_footer_patch` writes.
            // That disjointness is also why the sentinel is trimmed by a FREE function over
            // the field rather than a `&self` method — a method would borrow the whole
            // `self` and collide with the `footer_character_autocomplete` entry below.
            let suggestions: &[String] = footer_autocomplete_candidates(&self.character_names);
            let autocomplete = self
                .footer_character_autocomplete
                .entry(bubble_id)
                .or_insert_with(|| {
                    AutocompleteLine::new(("translation_footer_character", bubble_id))
                });
            autocomplete.set_max_suggestions(FOOTER_CHARACTER_AUTOCOMPLETE_MAX);
            autocomplete.set_hint_text(t!("translation.tab.who_speaks_placeholder"));

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
        // A hint is not a replica: it has no original, no translation and no character, so the
        // incomplete-replica rules would paint a warning border on every hint. Exempt the class.
        if bubble.bubble_class.as_deref().map(BubbleClass::from_str) == Some(BubbleClass::Hint) {
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
        if ui.button(t!("translation.tab.create_line_button")).clicked() {
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
        // A missing or unknown variant key loads as the default variant (saved back as its key).
        self.ocr_panel_options.paddle_vl_model = paddle_vl_model_from_settings(ocr);
        // Absent or unknown values load as the defaults: never the previous title's choices.
        self.neurolate_settings = crate::neurolate::settings_from_json(ocr.and_then(|ocr| ocr.get("params")).and_then(Value::as_object));
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
            // Absent in projects saved before the option existed; the `true`
            // default from `OcrPanelOptions::default` then stays in force.
            if let Some(fix_caps) = ocr_obj.get("fix_caps_lock").and_then(Value::as_bool) {
                self.ocr_panel_options.fix_caps_lock = fix_caps;
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
                    if let Some(detect_mask) = detect_mask_param(paddle) {
                        self.ocr_panel_options.paddle_detect_mask = detect_mask;
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
                    if let Some(detect_mask) = detect_mask_param(surya) {
                        self.ocr_panel_options.surya_detect_mask = detect_mask;
                    }
                }
                let ai_api_obj = params
                    .get("ai_api")
                    .or_else(|| params.get("aiapi"))
                    .and_then(Value::as_object);
                if let Some(ai_api) = ai_api_obj {
                    if let Some(service) = ai_api.get("service").and_then(Value::as_str) {
                        let service = AiApiService::from_key(service);
                        self.ocr_panel_options.ai_api.service = service;
                        // A compatible service has no default model: never keep the hosted default
                        // of `AiApiConnectionState::new` under it when no model was saved.
                        self.ocr_panel_options.ai_api.model = service.default_model().to_string();
                    }
                    // Absent in settings written before compatible services existed: the field keeps
                    // its current value (empty in a fresh state, else the previously loaded
                    // settings' URL). A key is bound to its exact URL, so a kept URL never sends
                    // a key saved for another server.
                    if let Some(base_url) = ai_api.get("base_url").and_then(Value::as_str) {
                        self.ocr_panel_options.ai_api.base_url = base_url.trim().to_string();
                    }
                    if let Some(model) = ai_api.get("model").and_then(Value::as_str) {
                        let trimmed = model.trim();
                        if !trimmed.is_empty() {
                            self.ocr_panel_options.ai_api.model = trimmed.to_string();
                        }
                    }
                    if let Some(system_instruction) =
                        ai_api.get("system_instruction").and_then(Value::as_str)
                    {
                        self.ocr_panel_options.ai_api.system_instruction =
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
                    let service = AiApiService::from_key(service);
                    self.mt_panel_options.ai_api.service = service;
                    // As for OCR: a saved service without a saved model gets its own default.
                    self.mt_panel_options.ai_api.model = service.default_model().to_string();
                }
                // Absent in settings written before compatible services existed: kept as is, as
                // for OCR above (empty only in a fresh state).
                if let Some(base_url) = ai_obj.get("base_url").and_then(Value::as_str) {
                    self.mt_panel_options.ai_api.base_url = base_url.trim().to_string();
                }
                if let Some(model) = ai_obj.get("model").and_then(Value::as_str)
                    && !model.trim().is_empty()
                {
                    self.mt_panel_options.ai_api.model = model.trim().to_string();
                }
                if let Some(system_instruction) =
                    ai_obj.get("system_instruction").and_then(Value::as_str)
                {
                    self.mt_panel_options.ai_api.system_instruction =
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
                // Opt-in: a missing key (settings written before the option existed) reads as off.
                self.mt_panel_options.ai_strict_json = ai_obj.get("strict_json").and_then(Value::as_bool).unwrap_or(false);
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
            if let Some(value) = comp_obj.get("include_hint_bubbles").and_then(Value::as_bool) {
                self.composition_panel_options.include_hint_bubbles = value;
            }
            if let Some(value) = comp_obj.get("hint_wrap").and_then(Value::as_str) {
                self.composition_panel_options.hint_wrap = normalize_wrap_with(value);
            }
            if let Some(value) = comp_obj.get("hint_wrap_enabled").and_then(Value::as_bool) {
                self.composition_panel_options.hint_wrap_enabled = value;
            }
            if let Some(value) = comp_obj.get("hint_extra_sep").and_then(Value::as_str) {
                self.composition_panel_options.hint_extra_sep = value.to_string();
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
            neurolate: self.neurolate_settings,
        };
        if self.settings_save_tx.send(request).is_ok() {
            self.ocr_settings_dirty = false;
            self.mt_settings_dirty = false;
            self.composition_settings_dirty = false;
            self.text_detector_settings_dirty = false;
        }
    }
}

/// Assembles the footer character autocomplete base out of its four sources.
///
/// The returned order IS the suggestion priority the widget shows: the "no character" sentinel
/// first, then the project roster from `characters.json` (`project_names`, in file order), then the
/// free-form names typed on this chapter's bubbles (`chapter_names`, in bubble order), then the
/// built-in service names. Entries are trimmed, empties dropped, and duplicates removed
/// case-insensitively keeping the FIRST spelling — so a chapter name that already exists in the
/// roster does not appear twice and keeps the roster's spelling.
#[must_use]
fn build_translation_character_names(
    project_names: &[String],
    chapter_names: &[String],
) -> Vec<String> {
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

    push_unique(footer_no_character());
    for name in project_names {
        push_unique(name);
    }
    for name in chapter_names {
        push_unique(name);
    }
    for name in footer_additional_character_names() {
        push_unique(name);
    }
    out
}

/// The suggestion slice the FOOTER's free-input field gets: `names` without a leading
/// `footer_no_character()` sentinel.
///
/// The sentinel belongs to the bubbles panel alone, where the character field is a combo box and
/// "(не указан)" is a selectable row (`panels/bubbles.rs` uses `character_names[0]` as its
/// fallback value). In the footer the same list feeds a free-text field, and `AutocompleteLine`
/// may splice the TAIL of a candidate from a word boundary: with the sentinel present, typing
/// "ук" matches inside "(не указан)" and silently writes `указан)` into the field with
/// `changed = true`. The debounced patch then stores that fragment as a real character name — it
/// matches no sentinel, so nothing downstream (chapter names, recent-character cards, MT grouping,
/// composition) would ever filter it out again. Keeping the sentinel out of the widget's input is
/// the fix; the widget itself is right to offer tails.
///
/// The check is explicit, never a blind `[1..]`: a list that does not start with the sentinel (an
/// empty one, or a future assembly order) is returned untouched. Returns a borrowed slice — the
/// footer must not clone this per frame.
#[must_use]
fn footer_autocomplete_candidates(names: &[String]) -> &[String] {
    match names.split_first() {
        Some((first, rest)) if first == footer_no_character() => rest,
        _ => names,
    }
}

/// Collects every character name actually entered on `bubbles`, in bubble-id order.
///
/// `bubbles` is the whole chapter; the function does not care which page a bubble is on. The name
/// is read from the same `extra` key, with the same reader, that
/// `bubble_footer_state_from_record` uses for the footer field (`bubble_extra_string(..,
/// "character_name")`), so what is collected is byte-for-byte what the user sees — but without
/// building the whole `BubbleFooterState`, whose `clarification` `String` this function would
/// allocate and immediately drop for every bubble in the chapter. The name is collected regardless
/// of `is_known_character` — free-form input is the point of this list.
///
/// Dropped: bubbles of the `Image` and `Hint` classes (neither has a character field, so a stale
/// `character_name` left in their `extra` by a class switch is not a name anyone typed there),
/// blank names, and the two display sentinels `footer_no_character()` / `footer_no_characters()`.
/// The sentinel filter is REQUIRED, not defensive: `panels/bubbles.rs` writes
/// `footer_no_character()` into `Bubble.extra` through the footer patch queue when the "И.П."
/// checkbox is switched on for a bubble whose name is not in the list, so the sentinel really does
/// reach persisted data and would otherwise come back as a "character" named "(не указан)".
/// Duplicates are removed case-insensitively, keeping the first spelling; names appear in
/// ascending `Bubble.id` order of the bubble that introduced them, so the result is deterministic
/// whatever order `bubbles` arrives in.
#[must_use]
fn collect_chapter_character_names(bubbles: &[Bubble]) -> Vec<String> {
    let mut ordered = bubbles.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|bubble| bubble.id);

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for bubble in ordered {
        match bubble.bubble_class.as_deref().map(BubbleClass::from_str) {
            Some(BubbleClass::Image | BubbleClass::Hint) => continue,
            Some(BubbleClass::Text) | None => {}
        }
        let name = bubble_extra_string(&bubble.extra, "character_name");
        let trimmed = name.trim();
        if trimmed.is_empty()
            || trimmed == footer_no_character()
            || trimmed == footer_no_characters()
        {
            continue;
        }
        if seen.insert(trimmed.to_lowercase()) {
            out.push(trimmed.to_string());
        }
    }
    out
}

fn recent_character_entry_from_state(state: &BubbleFooterState) -> Option<RecentCharacterEntry> {
    let trimmed = state.character_name.trim();
    if trimmed.is_empty() || trimmed == footer_no_character() || trimmed == footer_no_characters() {
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

/// Cheap change probe of the title `characters.json` through the document store; `None`
/// when it is absent or cannot be stated (the latter is logged).
fn characters_file_signature(project: &ProjectData) -> Option<ms_docstore::Signature> {
    let path = project.paths.characters_dir.join("characters.json");
    match ms_docstore::signature(&ms_docstore::DocRef::new(&path, ms_docstore::DocKind::Characters)) {
        Ok(signature) => signature,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[translation::characters_watch] failed to probe characters file; path={} error={err}",
                path.display()
            ));
            None
        }
    }
}

/// Rounds one float detector rect outward into an integer `[x1, y1, x2, y2]` covering
/// rect in source-page pixel space, or `None` when it cannot be represented.
///
/// `x1`/`y1` are floored, `x2`/`y2` ceiled, so the integer rect fully covers the float
/// rect. Any non-finite coordinate, or one whose rounded form falls outside the `i32`
/// range, makes the whole rect unrepresentable: it is skipped (with a warning log)
/// rather than wrapped into a bogus box. In-range negative coordinates are preserved so
/// callers can clamp them to their own raster as needed.
fn detector_rect_to_px_rect(rect: &TextDetectorRect) -> Option<[i32; 4]> {
    let round = |value: f32, ceil: bool| -> Option<i32> {
        if !value.is_finite() {
            return None;
        }
        let rounded = if ceil { value.ceil() } else { value.floor() };
        // Bounds-check before converting so a stray coordinate is skipped, never
        // truncated or wrapped into a valid-looking box.
        if rounded < i32::MIN as f32 || rounded > i32::MAX as f32 {
            return None;
        }
        Some(rounded as i32)
    };
    match (
        round(rect.x1, false),
        round(rect.y1, false),
        round(rect.x2, true),
        round(rect.y2, true),
    ) {
        (Some(x1), Some(y1), Some(x2), Some(y2)) => Some([x1, y1, x2, y2]),
        _ => {
            ms_log::runtime_log::log_warn(format!(
                "autoclean: skipping detector rect with non-finite or out-of-i32-range coordinates: {rect:?}"
            ));
            None
        }
    }
}

/// Converts float detector rects into integer `[x1, y1, x2, y2]` covering rects in
/// source-page pixel space for `TextMaskModel`.
///
/// Each edge is rounded outward via [`detector_rect_to_px_rect`], which skips any rect
/// with a non-finite or out-of-range coordinate. Returns `None` for an empty slice, and
/// also when every rect was skipped, so the model's "`None` == no detector boxes known"
/// contract holds (an empty box vector is never stored).
fn detector_rects_to_blocks_px(rects: &[TextDetectorRect]) -> Option<Vec<[i32; 4]>> {
    if rects.is_empty() {
        return None;
    }
    let blocks: Vec<[i32; 4]> = rects.iter().filter_map(detector_rect_to_px_rect).collect();
    if blocks.is_empty() {
        None
    } else {
        Some(blocks)
    }
}

/// Rasterizes a mask-less result's blocks as the user sees them: the blocks resolved with the
/// detector panel's options (expanded / merged) filled into a source-size 0/255 mask. Returns
/// the mask and those resolved blocks, or `None` when the page has no size or no block covers a
/// pixel. The one owner of the legacy "blocks without a stored mask" display mask.
fn rasterize_resolved_blocks(result: &TextDetectorPageResult, options: &TextDetectorPanelOptions) -> Option<(Vec<u8>, Vec<TextDetectorRect>)> {
    let mask_w = usize::try_from(result.source_size[0]).ok().unwrap_or(0);
    let mask_h = usize::try_from(result.source_size[1]).ok().unwrap_or(0);
    if mask_w == 0 || mask_h == 0 {
        return None;
    }
    let resolved = detector_blocks_with_options(result, options);
    let mut mask_alpha = vec![0u8; mask_w.saturating_mul(mask_h)];
    for rect in &resolved {
        // Same checked float->int rounding as the block set, then clamp the covering
        // rect into the mask raster (negative edges -> 0, far edges -> mask extent).
        let Some([rx1, ry1, rx2, ry2]) = detector_rect_to_px_rect(rect) else {
            continue;
        };
        let x0 = usize::try_from(rx1.max(0)).unwrap_or(0).min(mask_w);
        let y0 = usize::try_from(ry1.max(0)).unwrap_or(0).min(mask_h);
        let x1 = usize::try_from(rx2.max(0)).unwrap_or(0).min(mask_w);
        let y1 = usize::try_from(ry2.max(0)).unwrap_or(0).min(mask_h);
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
        return None;
    }
    Some((mask_alpha, resolved))
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

    ms_text_detect::blocks::sort_reading_order(&mut merged);
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
        .ok_or_else(|| tf!("translation.tab.invalid_source_size_error", path = blocks_path.display()))?;
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
    ms_text_detect::blocks::sort_reading_order(&mut blocks);

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
        .ok_or_else(|| tf!("translation.tab.invalid_mask_error", page_idx = page_idx))?;
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

/// True when the stored bubble record is a hint bubble (`bubble_class == "hint"`).
fn is_hint_bubble_record(bubble: &Bubble) -> bool {
    bubble.bubble_class.as_deref().map(BubbleClass::from_str) == Some(BubbleClass::Hint)
}

fn image_bubbles_dir(project: &ProjectData) -> PathBuf {
    project.paths.unsaved_image_bubbles_dir.clone()
}

fn save_clipboard_image_bubble(project: &ProjectData, bubble_id: i64) -> Result<PathBuf, String> {
    let clipboard_image = paste_image::read_image_from_clipboard()?;
    let width = u32::try_from(clipboard_image.width)
        .map_err(|_| t!("translation.image_io.clipboard_too_wide_error").to_string())?;
    let height = u32::try_from(clipboard_image.height)
        .map_err(|_| t!("translation.image_io.clipboard_too_tall_error").to_string())?;
    let Some(image) = image::RgbaImage::from_raw(width, height, clipboard_image.rgba) else {
        return Err(t!("translation.image_io.clipboard_size_mismatch_error").to_string());
    };
    let dir = image_bubbles_dir(project);
    fs::create_dir_all(&dir)
        .map_err(|err| tf!("translation.image_io.create_dir_error", dir = dir.display(), err = err))?;
    let path = dir.join(format!("image_bubble_{bubble_id}.png"));
    image
        .save(&path)
        .map_err(|err| tf!("translation.image_io.save_error", path = path.display(), err = err))?;
    Ok(path)
}

fn copy_external_image_bubble(
    project: &ProjectData,
    bubble_id: i64,
    source: &Path,
) -> Result<PathBuf, String> {
    if !source.is_file() {
        return Err(tf!("translation.image_io.file_not_found_error", source = source.display()));
    }
    let dir = image_bubbles_dir(project);
    fs::create_dir_all(&dir)
        .map_err(|err| tf!("translation.image_io.create_dir_error", dir = dir.display(), err = err))?;
    let ext = source
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("png");
    let path = dir.join(format!("image_bubble_{bubble_id}.{ext}"));
    fs::copy(source, &path).map_err(|err| {
        tf!("translation.image_io.copy_error", source = source.display(), path = path.display(), err = err)
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
                &latest.neurolate,
            ) {
                // Structured log: a malformed `settings.json` is now left untouched, so this
                // is where the user-facing trace of a skipped save lives.
                ms_log::runtime_log::log_error(format!(
                    "[translation::settings_saver] failed to persist translation settings; path={} error={err} possible_cause=malformed or unwritable settings.json (the file was left untouched)",
                    latest.settings_file.display()
                ));
            }
        }
    });
    (tx, handle)
}

/// Persists the translation tab's four sections (`OCR`, `machine_translation`, `composition`,
/// `text_detector`) into the title `settings.json` through the serialized
/// `ms_docstore::update`, so concurrent writers of other keys are never reverted.
///
/// # Errors
/// Returns a technical message when the existing file cannot be read, does not parse, or
/// its root is not a JSON object (in every case it is left untouched), or when the atomic
/// write fails.
fn save_translation_settings_to_project_file(
    settings_file: &Path,
    ocr_options: &OcrPanelOptions,
    mt_options: &MtPanelOptions,
    composition_options: &CompositionPanelOptions,
    text_detector_options: &TextDetectorPanelOptions,
    neurolate: &NeurolateSettings,
) -> Result<(), String> {
    let doc = ms_docstore::DocRef::new(settings_file, ms_docstore::DocKind::ProjectSettings);
    // Default options = the historical layout (2-space pretty, no trailing newline, parent
    // directories created). A malformed or unreadable file is an error and stays untouched,
    // and so is a parseable non-object root (as in the canvas and spellcheck writers of the
    // same document): replacing it would destroy whatever it holds.
    ms_docstore::update(&doc, ms_docstore::WriteOptions::default(), |root| {
        let Some(root_obj) = root.as_object_mut() else {
            return Err(format!("settings root of '{}' is not a JSON object", settings_file.display()));
        };
        apply_translation_settings_sections(root_obj, ocr_options, mt_options, composition_options, text_detector_options, neurolate);
        Ok(())
    })
    .map_err(|err| format!("failed to save translation settings '{}': {err}", settings_file.display()))
}

/// Keys of `text_detector.params` that older builds wrote and nothing reads any more; the
/// settings writer removes them (reading ignores them).
const STALE_TEXT_DETECTOR_PARAM_KEYS: [&str; 4] =
    ["det_rearrange_max_batches", "font size multiplier", "font size max", "font size min"];

/// Writes the four sections the translation tab owns (`OCR`, `machine_translation`,
/// `composition`, `text_detector`) into the `settings.json` root; other keys are untouched.
/// The NeuroLate settings go under `OCR.params.neurolate`.
fn apply_translation_settings_sections(
    root_obj: &mut Map<String, Value>,
    ocr_options: &OcrPanelOptions,
    mt_options: &MtPanelOptions,
    composition_options: &CompositionPanelOptions,
    text_detector_options: &TextDetectorPanelOptions,
    neurolate: &NeurolateSettings,
) {

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
        "fix_caps_lock".to_string(),
        Value::Bool(ocr_options.fix_caps_lock),
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
    paddle_obj.insert("detect_mask".to_string(), Value::Bool(ocr_options.paddle_detect_mask));
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
    paddle_vl_obj.insert(
        "model".to_string(),
        Value::String(ocr_options.paddle_vl_model.key().to_string()),
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
    surya_obj.insert("detect_mask".to_string(), Value::Bool(ocr_options.surya_detect_mask));
    params_obj.insert("surya".to_string(), Value::Object(surya_obj));
    let mut ai_api_obj = params_obj
        .get("ai_api")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    ai_api_obj.insert(
        "service".to_string(),
        Value::String(ocr_options.ai_api.service.key().to_string()),
    );
    ai_api_obj.insert(
        "base_url".to_string(),
        Value::String(ocr_options.ai_api.base_url.trim().to_string()),
    );
    ai_api_obj.insert(
        "model".to_string(),
        Value::String(ocr_options.ai_api.model.clone()),
    );
    ai_api_obj.insert(
        "system_instruction".to_string(),
        Value::String(ocr_options.ai_api.system_instruction.clone()),
    );
    params_obj.insert("ai_api".to_string(), Value::Object(ai_api_obj));
    neurolate.write_json(&mut params_obj);
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
        Value::String(mt_options.ai_api.service.key().to_string()),
    );
    mt_ai_obj.insert(
        "base_url".to_string(),
        Value::String(mt_options.ai_api.base_url.trim().to_string()),
    );
    mt_ai_obj.insert(
        "model".to_string(),
        Value::String(mt_options.ai_api.model.clone()),
    );
    mt_ai_obj.insert(
        "system_instruction".to_string(),
        Value::String(mt_options.ai_api.system_instruction.clone()),
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
        "strict_json".to_string(),
        Value::Bool(mt_options.ai_strict_json),
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
        "include_hint_bubbles".to_string(),
        Value::Bool(composition_options.include_hint_bubbles),
    );
    composition_obj.insert(
        "hint_wrap".to_string(),
        Value::String(normalize_wrap_with(&composition_options.hint_wrap)),
    );
    composition_obj.insert(
        "hint_wrap_enabled".to_string(),
        Value::Bool(composition_options.hint_wrap_enabled),
    );
    composition_obj.insert(
        "hint_extra_sep".to_string(),
        Value::String(composition_options.hint_extra_sep.clone()),
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
    // Retired CTD params (the backend never used them; removed from the panel): strip them so
    // project settings written before their removal do not keep stale entries.
    for stale in STALE_TEXT_DETECTOR_PARAM_KEYS {
        text_detector_params.remove(stale);
    }
    text_detector_params.insert(
        "detect_size".to_string(),
        Value::Number((text_detector_options.ai_detect_size.clamp(896, 2048) as i64).into()),
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
        "neurolate" => TextDetectorAlgorithm::NeuroLate,
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
        "baberu" | "baberu_ocr" | "baberuocr" => OcrEngine::Baberu,
        "aiapi" | "ai_api" | "ai-api" | "genai" => OcrEngine::AiApi,
        "neurolate" => OcrEngine::NeuroLate,
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
        OcrEngine::Baberu => "baberu",
        OcrEngine::NeuroLate => "neurolate",
    }
}

/// The PaddleOCR-VL variant persisted in an `OCR` settings section
/// (`params.paddle_vl.model`, legacy section alias `params.paddleocrvl`). Missing, non-string
/// or unknown values give `PaddleVlVariant::default()`.
fn paddle_vl_model_from_settings(ocr: Option<&Map<String, Value>>) -> PaddleVlVariant {
    ocr.and_then(|ocr| ocr.get("params"))
        .and_then(Value::as_object)
        .and_then(|params| params.get("paddle_vl").or_else(|| params.get("paddleocrvl")))
        .and_then(Value::as_object)
        .and_then(|paddle_vl| paddle_vl.get("model"))
        .and_then(Value::as_str)
        .and_then(PaddleVlVariant::from_key)
        .unwrap_or_default()
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

/// An engine's "create mask and text blocks" parameter from its `OCR.params.<engine>` section;
/// `None` (keep the default) when absent or not a boolean.
fn detect_mask_param(engine_params: &Map<String, Value>) -> Option<bool> {
    engine_params.get("detect_mask").and_then(Value::as_bool)
}

/// Snapshots the OCR panel state into worker options. `ocr_source_lang` is the OCR input
/// language (the MT panel's source-language code) the AI API default instruction is worded for;
/// with `neurolate` (the tab's NeuroLate settings) and `project_type` it also resolves the
/// NeuroLate request parameters (`crate::neurolate::request_params`).
fn build_ocr_runtime_options(ocr_options: &OcrPanelOptions, ocr_source_lang: &str, neurolate: &NeurolateSettings, project_type: Option<ComicType>) -> OcrRuntimeOptions {
    let surya_is_active = ocr_options.engine == OcrEngine::Surya;
    OcrRuntimeOptions {
        manga_model: ocr_options.manga_model.clone(),
        paddle_lang: ocr_options.paddle_lang.clone(),
        paddle_vl_script: ocr_options.paddle_vl_script.clone(),
        paddle_vl_model: ocr_options.paddle_vl_model,
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
        paddle_detect_mask: ocr_options.paddle_detect_mask,
        surya_detect_mask: ocr_options.surya_detect_mask,
        ai_api_service: ocr_options.ai_api.service,
        ai_api_base_url: ocr_options.ai_api.base_url.clone(),
        ai_api_model: ocr_options.ai_api.model.clone(),
        ai_api_system_instruction: ocr_options.ai_api.system_instruction.clone(),
        ai_api_source_lang: ocr_source_lang.to_string(),
        neurolate: crate::neurolate::request_params(neurolate, ocr_source_lang, project_type),
        neurolate_detect_mask: neurolate.detect_mask,
    }
}

/// Builds the recognize request for a canvas selection on the page it overlaps most (`None`
/// when it overlaps no page). `ocr_source_lang` and `neurolate` as in
/// `build_ocr_runtime_options`. The request carries no region (the quick-selection trigger
/// sets it).
fn build_ocr_request(
    canvas: &CanvasView,
    project: &ProjectData,
    selection_rect: Rect,
    request_id: u64,
    ocr_options: &OcrPanelOptions,
    ocr_source_lang: &str,
    neurolate: &NeurolateSettings,
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
            options: build_ocr_runtime_options(ocr_options, ocr_source_lang, neurolate, project.comic_type),
            page_path: page.path.clone(),
            uv_rect: [u1, v1, u2, v2],
            image_override_png: None,
            join_newlines: ocr_options.join_newlines,
            reflect_strings: ocr_options.reflect_strings,
            char_replacements: ocr_options.runtime_char_replacements(),
            fix_caps_lock: ocr_options.fix_caps_lock,
            region: None,
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
    result: &crate::ocr::OcrRecognizeResult,
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
    use super::{
        Bubble, Map, TranslationTabState, Value,
        build_translation_character_names, collect_chapter_character_names,
        footer_additional_character_names, footer_autocomplete_candidates, footer_no_character,
        footer_no_characters,
        footer_tracking_should_recompute,
    };

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

    /// Builds a bubble carrying `character_name` in `extra`, the way the footer stores it.
    /// `class` is the persisted `bubble_class` token (`None` = a legacy text bubble).
    fn character_bubble(id: i64, class: Option<&str>, character_name: &str) -> Bubble {
        let mut extra = Map::new();
        extra.insert(
            "character_name".to_string(),
            Value::String(character_name.to_string()),
        );
        Bubble {
            id,
            img_idx: 0,
            img_u: 0.5,
            img_v: 0.5,
            side: None,
            bubble_class: class.map(str::to_string),
            bubble_type: None,
            text: String::new(),
            original_text: String::new(),
            extra,
        }
    }

    /// Order is the suggestion priority the user sees, so it must follow the bubbles' own order
    /// (ascending id) whatever order the snapshot arrives in, and a name re-used with different
    /// capitalization must not produce a second entry.
    #[test]
    fn chapter_character_names_follow_bubble_id_order_and_dedupe_by_case() {
        let bubbles = vec![
            character_bubble(30, Some("text"), "Борис"),
            character_bubble(10, None, "Аня"),
            character_bubble(20, Some("text"), "аня"),
            character_bubble(40, Some("text"), "  Борис  "),
        ];
        assert_eq!(
            collect_chapter_character_names(&bubbles),
            vec!["Аня".to_string(), "Борис".to_string()],
            "first spelling wins and each name appears once"
        );
    }

    /// `footer_no_character()` genuinely reaches `Bubble.extra` (the panel's "И.П." checkbox
    /// writes it as a fallback value), so filtering it here is required rather than defensive; an
    /// empty field is not a name; and neither an image nor a hint bubble has a character field at
    /// all — a `character_name` left in their `extra` by a class switch must not leak into the
    /// base.
    #[test]
    fn chapter_character_names_drop_sentinels_blanks_and_classes_without_a_character() {
        let bubbles = vec![
            character_bubble(1, Some("text"), ""),
            character_bubble(2, Some("text"), "   "),
            character_bubble(3, Some("text"), footer_no_character()),
            character_bubble(4, Some("text"), footer_no_characters()),
            character_bubble(5, Some("image"), "Обложка"),
            character_bubble(6, Some("hint"), "Примечание"),
            character_bubble(7, Some("text"), "Секретарша"),
        ];
        assert_eq!(
            collect_chapter_character_names(&bubbles),
            vec!["Секретарша".to_string()],
        );
    }

    /// The assembled base is what the footer widget ranks its suggestions by, so the four groups
    /// must stay in this exact order: sentinel, project roster, chapter-typed names, built-ins.
    #[test]
    fn the_autocomplete_base_orders_sentinel_roster_chapter_then_builtins() {
        let project = vec!["Аня".to_string()];
        let chapter = vec!["Секретарша".to_string()];
        let out = build_translation_character_names(&project, &chapter);
        assert_eq!(out[0], footer_no_character());
        assert_eq!(out[1], "Аня");
        assert_eq!(out[2], "Секретарша");
        assert!(
            out[3..]
                .iter()
                .map(String::as_str)
                .eq(footer_additional_character_names()),
            "the built-in service names close the list, in their declared order"
        );
    }

    /// A name typed in the chapter that the roster already holds must not double the entry: the
    /// roster spelling is the one shown, and the chapter copy is dropped case-insensitively.
    #[test]
    fn a_chapter_name_already_in_the_roster_is_not_repeated() {
        let project = vec!["Аня".to_string()];
        let chapter = vec!["аня".to_string(), "Секретарша".to_string()];
        let out = build_translation_character_names(&project, &chapter);
        // Cyrillic case folding is not ASCII case folding, so compare through `to_lowercase`,
        // the same key the deduplication itself uses.
        let anya_entries = out
            .iter()
            .filter(|name| name.to_lowercase() == "аня")
            .count();
        assert_eq!(anya_entries, 1);
        assert_eq!(out[1], "Аня", "the roster spelling survives");
        assert_eq!(out[2], "Секретарша");
    }

    /// The bubbles panel indexes `character_names[0]` as its "no character" fallback value and
    /// shows the built-ins as combo-box rows, so an empty project must still produce exactly
    /// sentinel + the 13 built-in service names.
    #[test]
    fn an_empty_project_still_yields_the_sentinel_and_every_builtin() {
        let out = build_translation_character_names(&[], &[]);
        assert_eq!(out.len(), 1 + footer_additional_character_names().len());
        assert_eq!(out[0], footer_no_character());
        assert!(
            out[1..]
                .iter()
                .map(String::as_str)
                .eq(footer_additional_character_names())
        );
    }

    /// The sentinel is a combo-box row, never a free-input candidate: `AutocompleteLine` splices
    /// the tail of a candidate from a word boundary, so leaving "(не указан)" in would let a query
    /// like "ук" write the fragment `указан)` into the field and, one debounce later, into the
    /// bubble as a real character name. The trim must be conditional, not a blind `[1..]`.
    #[test]
    fn the_footer_gets_the_candidates_without_the_leading_sentinel() {
        let full = build_translation_character_names(&["Аня".to_string()], &[]);
        let offered = footer_autocomplete_candidates(&full);
        assert_eq!(offered.len(), full.len() - 1);
        assert_eq!(offered[0], "Аня");
        assert!(
            !offered.iter().any(|name| name == footer_no_character()),
            "no candidate may carry the sentinel's parentheses into a free-input field"
        );

        // A list that does not start with the sentinel is passed through untouched.
        let no_sentinel = vec!["Аня".to_string(), "Борис".to_string()];
        assert_eq!(footer_autocomplete_candidates(&no_sentinel), &no_sentinel[..]);
        assert!(footer_autocomplete_candidates(&[]).is_empty());
    }

    /// The revision gate exists so an unrelated bubble edit does not reassemble the list. Nothing
    /// else observes that, so assert it directly: a sync whose collected set is unchanged must
    /// leave `character_names` byte-identical, and a sync that adds a name must rebuild it.
    #[test]
    fn an_unchanged_chapter_name_set_does_not_reassemble_the_list() {
        let mut tab = TranslationTabState::default();
        tab.character_names_project = vec!["Аня".to_string()];
        tab.character_names_chapter = vec!["Секретарша".to_string()];
        // A value the assembler could never produce: if it survives, no rebuild happened.
        tab.character_names = vec!["<not reassembled>".to_string()];

        let same = vec![character_bubble(1, Some("text"), "Секретарша")];
        tab.sync_chapter_character_names(&same);
        assert_eq!(
            tab.character_names,
            vec!["<not reassembled>".to_string()],
            "an equal collected set must not touch the assembled list"
        );

        let grown = vec![
            character_bubble(1, Some("text"), "Секретарша"),
            character_bubble(2, Some("text"), "Босс"),
        ];
        tab.sync_chapter_character_names(&grown);
        assert_eq!(
            tab.character_names_chapter,
            vec!["Секретарша".to_string(), "Босс".to_string()]
        );
        assert_eq!(tab.character_names[0], footer_no_character());
        assert_eq!(tab.character_names[1], "Аня");
        assert_eq!(tab.character_names[2], "Секретарша");
        assert_eq!(tab.character_names[3], "Босс");
    }

    /// A roster reload replaces only the roster half. The chapter names are derived from bubbles,
    /// not from `characters.json`, so they must survive it — otherwise every mtime tick of that
    /// file would drop the free-form names until the next bubble revision change. This exercises
    /// the assignment + `rebuild_character_names` pair that `reload_character_names` performs
    /// around its disk read (the read itself needs a real `ProjectData` and is not covered here).
    #[test]
    fn a_roster_reload_keeps_the_chapter_collected_names() {
        let mut tab = TranslationTabState::default();
        tab.character_names_chapter = vec!["Секретарша".to_string()];
        tab.character_names_project = vec!["Аня".to_string()];
        tab.rebuild_character_names();

        tab.character_names_project = vec!["Борис".to_string()];
        tab.rebuild_character_names();

        assert_eq!(tab.character_names_chapter, vec!["Секретарша".to_string()]);
        assert_eq!(tab.character_names[0], footer_no_character());
        assert_eq!(tab.character_names[1], "Борис", "the roster was replaced");
        assert_eq!(
            tab.character_names[2], "Секретарша",
            "the chapter-collected names survived the reload"
        );
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod settings_persistence_tests {
    use super::{
        CompositionPanelOptions, MtPanelOptions, OcrPanelOptions, TextDetectorPanelOptions, Value,
        save_translation_settings_to_project_file,
    };

    fn save_defaults(path: &std::path::Path) -> Result<(), String> {
        save_translation_settings_to_project_file(
            path,
            &OcrPanelOptions::default(),
            &MtPanelOptions::default(),
            &CompositionPanelOptions::default(),
            &TextDetectorPanelOptions::default(),
            &crate::neurolate::NeurolateSettings::default(),
        )
    }

    /// The translation writer owns only its four sections; `canvas` and top-level keys of
    /// other writers survive.
    #[test]
    fn save_keeps_keys_of_other_writers() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"canvas":{"bubble_type":"aside"},"comic_type":"ribbon"}"#).map_err(|err| err.to_string())?;
        save_defaults(&path)?;
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|err| err.to_string())?).map_err(|err| err.to_string())?;
        assert_eq!(root["canvas"]["bubble_type"], "aside");
        assert_eq!(root["comic_type"], "ribbon");
        for section in ["OCR", "machine_translation", "composition", "text_detector"] {
            assert!(root[section].is_object(), "{section} written");
        }
        Ok(())
    }

    /// Declared behavior change: a corrupt `settings.json` is no longer replaced by `{}` plus
    /// the four sections; the save fails and the file stays byte-identical.
    #[test]
    fn save_never_overwrites_malformed_file() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("settings.json");
        let seed = "{\"canvas\": {";
        std::fs::write(&path, seed).map_err(|err| err.to_string())?;
        assert!(save_defaults(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).map_err(|err| err.to_string())?, seed);
        Ok(())
    }

    /// The retired CTD params of older builds are stripped on save; the kept params and
    /// unknown keys of the same object survive.
    #[test]
    fn save_strips_retired_text_detector_params() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("settings.json");
        let seed = r#"{"text_detector":{"params":{"detect_size":1024,"det_rearrange_max_batches":4,"font size multiplier":1.0,"font size max":-1.0,"font size min":-1.0,"custom":7}}}"#;
        std::fs::write(&path, seed).map_err(|err| err.to_string())?;
        save_defaults(&path)?;
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|err| err.to_string())?).map_err(|err| err.to_string())?;
        let params = root["text_detector"]["params"].as_object().ok_or("params is an object")?;
        for stale in ["det_rearrange_max_batches", "font size multiplier", "font size max", "font size min"] {
            assert!(!params.contains_key(stale), "{stale} stripped: {params:?}");
        }
        assert_eq!(params["detect_size"], 1280, "the current options are written");
        assert_eq!(params["custom"], 7, "unknown keys survive");
        Ok(())
    }

    /// A parseable document whose root is not an object is an error too, and stays
    /// byte-identical (consistent with the other `settings.json` writers).
    #[test]
    fn save_never_overwrites_a_non_object_root() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("settings.json");
        let seed = "[1, 2]";
        std::fs::write(&path, seed).map_err(|err| err.to_string())?;
        assert!(save_defaults(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).map_err(|err| err.to_string())?, seed);
        Ok(())
    }
}

#[cfg(test)]
mod ocr_model_settings_tests {
    use super::{
        CompositionPanelOptions, Map, MtPanelOptions, OcrEngine, OcrPanelOptions, PaddleVlVariant, TextDetectorPanelOptions,
        Value, apply_translation_settings_sections, detect_mask_param, ocr_engine_to_project_key, paddle_vl_model_from_settings,
        parse_ocr_engine_key,
    };
    use serde_json::json;

    /// Writes `ocr` through the real settings writer and returns the `OCR` section.
    fn written_ocr_section(ocr: &OcrPanelOptions, seed: Value) -> Map<String, Value> {
        let mut root = seed.as_object().cloned().unwrap_or_default();
        apply_translation_settings_sections(
            &mut root,
            ocr,
            &MtPanelOptions::default(),
            &CompositionPanelOptions::default(),
            &TextDetectorPanelOptions::default(),
            &crate::neurolate::NeurolateSettings::default(),
        );
        root.get("OCR").and_then(Value::as_object).cloned().unwrap_or_default()
    }

    /// Every variant survives a write + read, keeps the sibling `script` key, and is stored
    /// as its stable key.
    #[test]
    fn paddle_vl_model_round_trips_for_every_variant() {
        for variant in PaddleVlVariant::ALL {
            let options = OcrPanelOptions { paddle_vl_model: variant, ..Default::default() };
            let section = written_ocr_section(&options, json!({}));
            assert_eq!(section["params"]["paddle_vl"]["model"], json!(variant.key()));
            assert_eq!(section["params"]["paddle_vl"]["script"], json!("auto"));
            assert_eq!(paddle_vl_model_from_settings(Some(&section)), variant);
        }
    }

    /// A project saved before the option existed, an unknown key or a non-string value all
    /// load as the default variant (normalized to its key on the next save).
    #[test]
    fn missing_or_unknown_paddle_vl_model_loads_as_default() {
        assert_eq!(paddle_vl_model_from_settings(None), PaddleVlVariant::Official16);
        for section in [
            json!({}),
            json!({ "params": {} }),
            json!({ "params": { "paddle_vl": { "script": "korean" } } }),
            json!({ "params": { "paddle_vl": { "model": "official_9_9" } } }),
            json!({ "params": { "paddle_vl": { "model": 3 } } }),
        ] {
            let map = section.as_object().cloned().unwrap_or_default();
            assert_eq!(paddle_vl_model_from_settings(Some(&map)), PaddleVlVariant::Official16, "{section}");
        }
        // The legacy section alias and key normalization of `from_key` are honoured.
        let legacy = json!({ "params": { "paddleocrvl": { "model": " MANGA_JA " } } });
        assert_eq!(paddle_vl_model_from_settings(legacy.as_object()), PaddleVlVariant::MangaJa);
    }

    #[test]
    fn baberu_engine_key_round_trips_and_accepts_aliases() {
        for alias in ["baberu", "baberu_ocr", "BaberuOCR", " baberuocr "] {
            assert_eq!(parse_ocr_engine_key(alias), OcrEngine::Baberu, "{alias}");
        }
        assert_eq!(parse_ocr_engine_key(ocr_engine_to_project_key(OcrEngine::Baberu)), OcrEngine::Baberu);
        let options = OcrPanelOptions { engine: OcrEngine::Baberu, ..Default::default() };
        assert_eq!(written_ocr_section(&options, json!({}))["engine"], json!("baberu"));
    }

    /// The "create mask and text blocks" parameters are per engine: written under their own
    /// engine's params section, never as a top-level `OCR` key; other keys of those sections
    /// survive.
    #[test]
    fn detect_mask_parameters_are_written_per_engine() {
        let options = OcrPanelOptions { paddle_detect_mask: true, surya_detect_mask: false, ..Default::default() };
        let written = written_ocr_section(&options, json!({ "OCR": { "params": { "surya": { "custom": 1 } } } }));
        assert_eq!(written["params"]["paddle"]["detect_mask"], json!(true));
        assert_eq!(written["params"]["surya"]["detect_mask"], json!(false));
        assert_eq!(written["params"]["surya"]["custom"], json!(1));
        assert!(written.get("detect_mask").is_none());
    }

    /// What is written is what the loader reads back per engine; an absent or non-boolean
    /// value keeps the default.
    #[test]
    fn detect_mask_parameters_are_read_per_engine() {
        let options = OcrPanelOptions { paddle_detect_mask: false, surya_detect_mask: true, ..Default::default() };
        let written = written_ocr_section(&options, json!({}));
        let params = written.get("params").and_then(Value::as_object).cloned().unwrap_or_default();
        let section = |engine: &str| params.get(engine).and_then(Value::as_object).cloned().unwrap_or_default();
        assert_eq!(detect_mask_param(&section("paddle")), Some(false));
        assert_eq!(detect_mask_param(&section("surya")), Some(true));
        assert_eq!(detect_mask_param(&Map::new()), None);
        let not_bool = json!({ "detect_mask": "yes" });
        assert_eq!(detect_mask_param(not_bool.as_object().unwrap_or(&Map::new())), None);
    }
}

#[cfg(test)]
mod ocr_model_download_wiring_tests {
    use super::{ModelDownloadAction, OcrEngine, OcrLoadState, OcrModelDownloadController, PaddleVlVariant, TranslationTabState};
    use crate::ocr_model_download::fakes::{intact_verify, missing_probe, waits_for_cancel};
    use crate::ocr_model_download::{DownloadNotice, Jobs, OcrModelDownloadState};
    use ms_sysprobe::ai_models::external_catalog::BABERU_OCR;
    use std::time::{Duration, Instant};

    /// A tab whose download controller runs fakes (no filesystem, no network).
    fn tab_with_fake_downloads() -> TranslationTabState {
        let mut tab = TranslationTabState::default();
        tab.ocr_model_downloads = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: waits_for_cancel, verify: intact_verify });
        tab
    }

    fn settle(tab: &mut TranslationTabState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            tab.ocr_model_downloads.poll();
            if !tab.ocr_model_downloads.is_active() {
                break;
            }
            assert!(Instant::now() < deadline, "download controller did not settle");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A PaddleOCR-VL variant switch unloads the engine (the loaded weights are the old
    /// variant's) and probes the new variant; an unchanged selection probes nothing.
    #[test]
    fn a_variant_switch_unloads_paddle_vl_and_probes_the_new_variant() {
        let mut tab = tab_with_fake_downloads();
        tab.ocr_panel_options.engine = OcrEngine::PaddleVl;
        tab.ocr_panel_options.paddle_vl_model = PaddleVlVariant::MangaJa;
        tab.set_ocr_state_for_engine(OcrEngine::PaddleVl, OcrLoadState::Ready);
        let manga_ja = tab.selected_ocr_model_specs();
        tab.apply_ocr_model_panel_changes(&manga_ja, PaddleVlVariant::MangaJa, ModelDownloadAction::None);
        assert_eq!(tab.ocr_state_for_engine(OcrEngine::PaddleVl), OcrLoadState::Ready);
        assert_eq!(tab.ocr_model_downloads.state_for(PaddleVlVariant::MangaJa.spec().id), OcrModelDownloadState::Unknown);

        let official = [PaddleVlVariant::Official16.spec()];
        tab.apply_ocr_model_panel_changes(&official, PaddleVlVariant::Official16, ModelDownloadAction::None);
        assert_eq!(tab.ocr_state_for_engine(OcrEngine::PaddleVl), OcrLoadState::NotLoaded);
        assert_eq!(tab.ocr_model_downloads.state_for(PaddleVlVariant::MangaJa.spec().id), OcrModelDownloadState::Checking);
        settle(&mut tab);
        assert_eq!(tab.ocr_model_downloads.state_for(PaddleVlVariant::MangaJa.spec().id), OcrModelDownloadState::Missing);

        // An engine switch to Baberu probes Baberu and leaves PaddleOCR-VL's load state.
        tab.ocr_panel_options.engine = OcrEngine::Baberu;
        tab.apply_ocr_model_panel_changes(&manga_ja, PaddleVlVariant::MangaJa, ModelDownloadAction::None);
        assert_eq!(tab.ocr_model_downloads.state_for(BABERU_OCR.id), OcrModelDownloadState::Checking);
        settle(&mut tab);
    }

    /// An EasyOCR language change that switches the script probes the new script's model.
    #[test]
    fn an_easyocr_script_switch_probes_the_new_model() {
        use ms_sysprobe::ai_models::easyocr::{EasyOcrScript, easyocr_spec};
        let mut tab = tab_with_fake_downloads();
        tab.ocr_panel_options.engine = OcrEngine::EasyOcr;
        tab.ocr_panel_options.easy_langs = "ko".to_string();
        let korean = tab.selected_ocr_model_specs();
        assert_eq!(korean.iter().map(|spec| spec.id).collect::<Vec<_>>(), [easyocr_spec(EasyOcrScript::Korean).id]);
        tab.ocr_panel_options.easy_langs = "ja, en".to_string();
        tab.apply_ocr_model_panel_changes(&korean, PaddleVlVariant::Official16, ModelDownloadAction::None);
        assert_eq!(tab.ocr_model_downloads.state_for(easyocr_spec(EasyOcrScript::Japanese).id), OcrModelDownloadState::Checking);
        assert_eq!(tab.ocr_model_downloads.state_for(easyocr_spec(EasyOcrScript::Korean).id), OcrModelDownloadState::Unknown);
        settle(&mut tab);
    }

    /// Download and Cancel go to the selected engine's bundle (Surya: both checkpoints); an
    /// engine without an external model ignores them.
    #[test]
    fn download_and_cancel_route_to_the_selected_spec() {
        let mut tab = tab_with_fake_downloads();
        tab.ocr_panel_options.engine = OcrEngine::MangaOcr;
        tab.apply_ocr_model_panel_changes(&[], PaddleVlVariant::Official16, ModelDownloadAction::Download);
        assert!(!tab.ocr_model_downloads.is_active());

        tab.ocr_panel_options.engine = OcrEngine::Baberu;
        let baberu = tab.selected_ocr_model_specs();
        tab.apply_ocr_model_panel_changes(&baberu, PaddleVlVariant::Official16, ModelDownloadAction::Download);
        assert!(matches!(tab.ocr_model_downloads.state_for(BABERU_OCR.id), OcrModelDownloadState::Downloading(_)));
        tab.apply_ocr_model_panel_changes(&baberu, PaddleVlVariant::Official16, ModelDownloadAction::Cancel);
        settle(&mut tab);
        let snapshot = tab.ocr_model_downloads.panel_snapshot(&baberu);
        assert_eq!(snapshot.notice, Some(DownloadNotice::Cancelled));
        assert_eq!(snapshot.state, OcrModelDownloadState::Missing);

        tab.ocr_panel_options.engine = OcrEngine::Surya;
        let surya = tab.selected_ocr_model_specs();
        assert_eq!(surya.len(), 2);
        tab.apply_ocr_model_panel_changes(&surya, PaddleVlVariant::Official16, ModelDownloadAction::Download);
        for spec in &surya {
            assert!(matches!(tab.ocr_model_downloads.state_for(spec.id), OcrModelDownloadState::Downloading(_)), "{}", spec.id);
        }
        // The detector panel's Surya bundle shares the detection spec: it sees the running
        // download as another bundle's and cannot start its own.
        let detector = tab.ocr_model_downloads.panel_snapshot(&super::SURYA_DETECTOR_SPECS);
        assert!(detector.other_download.is_some());
        tab.apply_detector_model_panel_action(ModelDownloadAction::Cancel);
        settle(&mut tab);
        assert_eq!(tab.ocr_model_downloads.panel_snapshot(&surya).notice, Some(DownloadNotice::Cancelled));
    }
}
