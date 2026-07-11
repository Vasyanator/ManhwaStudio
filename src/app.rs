/*
FILE OVERVIEW: src/app.rs
Main eframe application orchestration (`MangaApp`): project/model initialization,
background page/overlay loading, tab routing, and global hotkey dispatch.

Main structs:
- `MangaApp`: root app state (project, shared models, canvas/tabs, loaders, hotkeys).
- `PageImageInfo`: source-page geometry and load state independent from GPU residency.
- `PageTexture` / `TextureTile`: tiled source-page GPU residency backed by decoded tile bytes.
- `DecodedTile` / `DecodedPage`: background decode payload before GPU upload.
- `UploadTask`: incremental texture upload state with per-frame budget.

Async/background pipeline:
- `spawn_loader_thread`: decodes pages in worker pool and sends `LoaderEvent`; workers throttle
  their look-ahead to `DECODE_AHEAD_WINDOW` pages past the promotion frontier so a slow early page
  cannot let decoded-but-unpromotable pages pile up unbounded in `decoded_pending_by_idx`.
- `spawn_overlay_loader_thread`: loads clean overlays in background and prebuilds the
  `RgbaImage` + `ColorImage` payload before it reaches the GUI thread.
- `decode_worker_count`: picks decode worker parallelism (logical cores by default, env override).
- `poll_loader_events` + `promote_decoded_pages_in_order`: preserve strict page order.
- `upload_textures_incremental`: frame-budgeted GPU upload to avoid GUI freezes.
- `poll_overlay_loader_events`: applies decoded overlays into shared model.
- `decode_image_rgba`: common image decode path with optional experimental GPU decode via ffmpeg.
- `page_cache`: when enabled and allowed by the memory profile at startup, main page decode also
  seeds the shared page RGBA cache to avoid a second full decode pass later.

App/frame flow:
- `new`: builds models, tabs, and input manager registrations.
- `new`: wires shared models (`bubbles`, `clean_overlays`, `text_mask`) between tabs.
- `new`: also starts shared AI-backend health probe and wires it into Settings/Translation tabs.
- `ui` (eframe::App, egui 0.35): frame tick (poll workers, draw active tab, dispatch hotkeys).
- `dispatch_hotkeys` + `execute_hotkey_command`: execute `InputManagerV2` commands.

Hotkeys:
- Translation canvas zoom/edit commands + panel toggles (`P/O/K/M/D` by default).
- Cleaning canvas zoom commands.

Profiling (optional, behind the `profiling` cargo feature):
- `ui` is instrumented with coarse `puffin::profile_scope!` markers around the heavy
  per-frame phases (worker polling, GPU upload, active-tab draw). The puffin_egui profiler
  window is shown while the feature is compiled in. All puffin references are gated behind
  `#[cfg(feature = "profiling")]` so the default build pulls in no puffin and has zero overhead.
  How to run: `cargo run --features profiling`.
*/

use crate::ai_backend_supervisor::AiBackendHandle;
use crate::canvas::{
    AsideBubbleCompactMode, AsideBubbleSideMode, BubbleMode, BubbleTextField, BubbleType,
    CanvasDrawParams, CanvasUiStatus, CanvasView, CanvasViewportSnapshot, OnTopFocusMode,
    SourceTextureUploadBudget, TranslationStatusDisplay, spawn_overlay_autosave_thread,
};
use crate::input_manager_v2::{HotkeyScopeV2, HotkeySpecV2, InputManagerV2};
use crate::memory_manager::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, MemoryManager, MemoryPressure, classify_memory_pressure,
    current_memory_availability, select_eviction_candidates,
};
use crate::models::bubbles_model::{BubblesModel, SharedCanvasSettings};
use crate::models::clean_overlays_model::{CleanOverlaysModel, save_overlay_snapshots_to};
use crate::models::text_mask_model::TextMaskModel;
use crate::project::{ComicType, ProjectData, save_comic_type_to_project_file};
use crate::runtime_log;
use crate::tabs::AppTab;
use crate::tabs::characters::{CharactersTabAction, CharactersTabState};
use crate::tabs::cleaning::CleaningTabState;
use crate::tabs::notes::NotesTabState;
use crate::tabs::ps_editor::PsEditorTabState;
use crate::tabs::settings::SettingsTabState;
use crate::tabs::terms::TermsTabState;
use crate::tabs::translation::backend_health::{
    AiBackendDeviceOption, AiBackendHealthSnapshot, AiBackendProbeCommand,
};
use crate::tabs::translation::{
    HOTKEY_TRANSLATION_COPY_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_COPY_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE,
    HOTKEY_TRANSLATION_PASTE_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_PASTE_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL, HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL, HOTKEY_TRANSLATION_TOGGLE_MT_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL, TranslationHotkeyHints, TranslationTabState,
};
use crate::tabs::typing::TypingTabState;
use crate::tabs::wiki::WikiTabState;
use eframe::egui;
use egui::{Align2, ColorImage, TextureOptions};
use ms_thread::{self as thread, JoinHandle};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use web_time::{Duration, Instant};

const DECODE_TILE_SIDE: u32 = 2048;
const LOADER_QUEUE_CAPACITY: usize = 2;
/// Maximum number of source pages workers may decode ahead of the next page still waiting
/// for in-order promotion. Bounds `decoded_pending_by_idx` so a slow early page cannot let
/// later pages pile up unbounded in RAM. Must be >= 1 so the page that unblocks promotion is
/// always inside the window (otherwise the loader would deadlock).
const DECODE_AHEAD_WINDOW: usize = 8;
const EVENT_POLL_BUDGET: usize = 8;
const UPLOAD_TILE_BUDGET_PER_FRAME: usize = 4;
const UPLOAD_BYTES_BUDGET_PER_FRAME: usize = 24 * 1024 * 1024;
const OVERLAY_EVENT_POLL_BUDGET: usize = 8;
const GPU_DECODE_DEFAULT_MIN_PIXELS: u64 = 6_000_000;
const ENV_IMAGE_DECODE_THREADS: &str = "MF_IMAGE_DECODE_THREADS";
const ENV_GPU_DECODE: &str = "MF_GPU_DECODE";
const ENV_GPU_DECODE_MIN_PIXELS: &str = "MF_GPU_DECODE_MIN_PIXELS";
const HOTKEY_TRANSLATION_ZOOM_IN: &str = "translation.canvas.zoom_in";
const HOTKEY_TRANSLATION_ZOOM_OUT: &str = "translation.canvas.zoom_out";
const HOTKEY_TRANSLATION_ZOOM_RESET: &str = "translation.canvas.zoom_reset";
const HOTKEY_TRANSLATION_DELETE_BUBBLE: &str = "translation.bubble.delete_selected";
const HOTKEY_TRANSLATION_CREATE_BUBBLE: &str = "translation.bubble.create_at_cursor";
const HOTKEY_CLEANING_ZOOM_IN: &str = "cleaning.canvas.zoom_in";
const HOTKEY_CLEANING_ZOOM_OUT: &str = "cleaning.canvas.zoom_out";
const HOTKEY_CLEANING_ZOOM_RESET: &str = "cleaning.canvas.zoom_reset";
const MEMORY_PRESSURE_POLL_INTERVAL: Duration = Duration::from_millis(1000);
const SOFT_PRESSURE_TARGET_FREE_BYTES: u64 = 256 * 1024 * 1024;
const HARD_PRESSURE_TARGET_FREE_BYTES: u64 = 768 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
struct CachedHotkeyHints {
    bindings_revision: u64,
    create_bubble_hint: Option<String>,
    translation_hints: TranslationHotkeyHints,
}

pub struct MangaApp {
    project: ProjectData,
    memory_manager: Arc<MemoryManager>,
    memory_pressure: MemoryPressure,
    last_memory_pressure_poll: Option<Instant>,
    bubbles_model: Arc<Mutex<BubblesModel>>,
    #[allow(dead_code)]
    clean_overlays_model: Arc<Mutex<CleanOverlaysModel>>,
    #[allow(dead_code)]
    text_mask_model: Arc<Mutex<TextMaskModel>>,
    applied_bubbles_revision: u64,
    canvas: CanvasView,
    page_infos: HashMap<usize, PageImageInfo>,
    textures: HashMap<usize, PageTexture>,
    failed_pages: HashSet<usize>,
    load_errors: Vec<String>,
    fonts_initialized: bool,
    fonts_load_started: bool,
    fonts_load_rx: Option<Receiver<Option<FontLoadResult>>>,
    loader_rx: Receiver<LoaderEvent>,
    decoded_queue: VecDeque<DecodedPage>,
    decoded_pending_by_idx: HashMap<usize, DecodedPage>,
    next_decode_idx_to_enqueue: usize,
    /// Lowest page index still awaiting in-order promotion, published to the decode worker
    /// pool so it can throttle how far ahead it decodes (see `DECODE_AHEAD_WINDOW`). Mirrors
    /// `next_decode_idx_to_enqueue`; updated whenever promotion advances it.
    decode_promotion_progress: Arc<AtomicUsize>,
    upload_task: Option<UploadTask>,
    loader_finished: bool,
    overlay_loader_rx: Option<Receiver<OverlayLoaderEvent>>,
    overlay_loader_started: bool,
    overlay_loader_finished: bool,
    page_cache_loader_rx: Option<Receiver<PageCacheLoaderEvent>>,
    page_cache_loader_started: bool,
    page_cache_loader_finished: bool,
    cached_hotkey_hints: CachedHotkeyHints,
    active_tab: AppTab,
    shared_canvas_viewport: CanvasViewportSnapshot,
    active_viewport_owner_tab: Option<AppTab>,
    /// Current page shared across view tabs (canvas tabs + the PS editor). Lets the PS editor pick
    /// up the page the canvas was showing and hand its page back on the way out.
    shared_page_idx: usize,
    /// Page-local source-pixel center of the shared view, for reproducing the camera position
    /// across tabs that do not share the canvas scroll model. `None` until a view tab publishes it.
    shared_page_center: Option<egui::Vec2>,
    /// The previously active "view" tab (canvas tabs or PS editor), used to detect transitions
    /// into/out of the PS editor for view sync. Non-view tabs (Settings, Wiki, …) preserve it.
    prev_view_tab: Option<AppTab>,
    ai_backend_health: Arc<Mutex<AiBackendHealthSnapshot>>,
    /// Per-frame cached copy of `ai_backend_health`, refreshed once per frame in `update`.
    /// The AI prompt/version-warning helpers read this snapshot instead of locking and
    /// cloning the shared health state up to three times per frame.
    ai_backend_health_cached: AiBackendHealthSnapshot,
    ai_backend_probe_tx: Option<Sender<AiBackendProbeCommand>>,
    translation_tab: TranslationTabState,
    cleaning_tab: CleaningTabState,
    typing_tab: TypingTabState,
    ps_editor_tab: PsEditorTabState,
    /// Shared unified layer document: the source of truth for per-page layer MODEL state, held by
    /// both view tabs (`Arc<Mutex<…>>`). Each tab re-projects its current page whenever the doc's
    /// `version` changes (the in-memory cross-tab sync). This field keeps the `Arc` alive.
    #[allow(dead_code)]
    layer_doc: std::sync::Arc<std::sync::Mutex<crate::models::layer_model::layer_doc::LayerDoc>>,
    characters_tab: CharactersTabState,
    terms_tab: TermsTabState,
    notes_tab: NotesTabState,
    settings_tab: SettingsTabState,
    wiki_tab: WikiTabState,
    input_manager_v2: InputManagerV2,
    comic_type_prompt_open: bool,
    comic_type_prompt_error: Option<String>,
    ai_device_prompt_open: bool,
    ai_device_prompt_initialized: bool,
    ai_device_prompt_pytorch_device: String,
    ai_device_prompt_directml_device_id: String,
    ai_device_prompt_applying: bool,
    ai_device_prompt_error: Option<String>,
    ai_backend_version_warning_open: bool,
    ai_backend_version_warning_dismissed: bool,
    /// Background thread that autosaves dirty overlays every 30 s.
    #[allow(dead_code)]
    overlay_autosave_thread: Option<JoinHandle<()>>,
    /// Active "save to project" merge job.
    save_to_project_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// Status text shown next to the "save to project" button.
    save_to_project_status: Option<(String, f64)>,
    /// Which action should happen after a successful "save then close" flow.
    pending_save_completion_action: Option<PendingCloseAction>,
    /// A project save requested while the typing tab's whole-project LAYER preload was still
    /// incomplete, deferred until the preload PASS drains. While true, `start_save_to_project` has
    /// NOT run yet: `request_save_to_project` started the async preload (`begin_preload_all_pages`)
    /// and `drive_pending_save_preload` advances it each frame (regardless of the active tab, since
    /// the typing tab only drives its own preload when it is drawn) then dispatches the real save
    /// once the pass drains (`!preload_all_pages_active`). Gating on pass COMPLETION rather than full
    /// residency is required so a page that genuinely fails to decode (and never becomes resident)
    /// cannot hang the save forever (Finding 1); every page that DID load has its lazy legacy→v3 text
    /// migration baked into the committed `layers.json` (see `flush_text_layers`), and a decode-failed
    /// page keeps its committed text verbatim. Masks and клин are intentionally NOT gated (no save-time
    /// correctness effect — masks are copied verbatim from the committed store, клин edits are
    /// dirty-flushed synchronously). Also feeds `save_busy`, which suppresses concurrent export
    /// dispatch (Finding 2).
    pending_save_after_preload: bool,
    /// Which exit dialog variant is currently shown.
    exit_dialog: Option<ExitDialogKind>,
    /// In-flight unsaved cleanup that must complete before the app is allowed to close.
    pending_exit_cleanup: Option<PendingExitCleanup>,
    has_unsaved_changes_cached: bool,
    next_unsaved_dir_check_s: f64,
    /// Windows can misplace a maximized root window when maximize is requested at native creation.
    #[cfg(target_os = "windows")]
    maximize_root_window_on_first_frame: bool,
    /// When true the app closes and signals main to reopen the launcher.
    return_to_launcher: bool,
    /// Shared flag written before the window closes so that `run_main_window` can detect it.
    return_to_launcher_flag: Arc<AtomicBool>,
    /// Whether the puffin_egui frame-profiler window is shown. Compiled in only under the
    /// `profiling` feature; defaults to on so the profiler is visible immediately, and can be
    /// toggled at runtime with F11.
    #[cfg(feature = "profiling")]
    profiler_window_open: bool,
}

/// Which variant of the exit/leave dialog is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitDialogKind {
    /// Triggered by the OS window-close button (×).
    WindowClose,
    /// Triggered by the in-app "Выйти в лаунчер" button.
    ReturnToLauncher,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingCloseAction {
    Exit,
    ReturnToLauncher,
}

struct PendingExitCleanup {
    action: PendingCloseAction,
    rx: Receiver<Result<(), String>>,
}

struct FontLoadResult {
    regular_bytes: Vec<u8>,
    bold_bytes: Option<Vec<u8>>,
}

pub struct PageTexture {
    pub tiles: Vec<TextureTile>,
    pub linear_last_used_frame: u64,
    pub nearest_last_used_frame: u64,
}

pub struct TextureTile {
    pub linear_texture: Option<egui::TextureHandle>,
    pub nearest_texture: Option<egui::TextureHandle>,
    pub origin_px: egui::Vec2,
    pub size_px: egui::Vec2,
    pub rgba: Arc<[u8]>,
}

impl PageTexture {
    #[must_use]
    pub fn estimated_linear_gpu_bytes(&self) -> u64 {
        self.tiles
            .iter()
            .filter(|tile| tile.linear_texture.is_some())
            .map(|tile| u64::try_from(tile.rgba.len()).unwrap_or(u64::MAX))
            .sum()
    }

    #[must_use]
    pub fn estimated_nearest_gpu_bytes(&self) -> u64 {
        self.tiles
            .iter()
            .filter(|tile| tile.nearest_texture.is_some())
            .map(|tile| u64::try_from(tile.rgba.len()).unwrap_or(u64::MAX))
            .sum()
    }

    pub fn drop_nearest_textures(&mut self) {
        for tile in &mut self.tiles {
            tile.nearest_texture = None;
        }
        self.nearest_last_used_frame = 0;
    }

    pub fn drop_linear_textures(&mut self) {
        for tile in &mut self.tiles {
            tile.linear_texture = None;
        }
        self.linear_last_used_frame = 0;
    }
}

#[derive(Debug, Clone)]
pub struct PageImageInfo {
    pub width_px: u32,
    pub height_px: u32,
    pub load_state: SourcePageLoadState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePageLoadState {
    Loading,
    Available,
    Failed,
}

struct DecodedTile {
    origin_px: [u32; 2],
    size_px: [u32; 2],
    rgba: Vec<u8>,
}

struct DecodedPage {
    idx: usize,
    width: u32,
    height: u32,
    tiles: Vec<DecodedTile>,
    cache_rgba: Option<Arc<image::RgbaImage>>,
}

struct PreparedOverlay {
    rgba: Arc<image::RgbaImage>,
    color: ColorImage,
}

enum LoaderEvent {
    Decoded(DecodedPage),
    Failed {
        idx: usize,
        path: String,
        error: String,
    },
    Finished,
}

enum OverlayLoaderEvent {
    Decoded {
        idx: usize,
        overlay: PreparedOverlay,
    },
    Failed {
        idx: usize,
        path: String,
        error: String,
    },
    Finished,
}

enum PageCacheLoaderEvent {
    Decoded {
        idx: usize,
        image: image::RgbaImage,
    },
    Failed {
        idx: usize,
        path: String,
        error: String,
    },
    Finished,
}

struct UploadTask {
    idx: usize,
    tiles: Vec<DecodedTile>,
    next_tile: usize,
    uploaded_tiles: Vec<TextureTile>,
}

impl MangaApp {
    pub fn new(
        project: ProjectData,
        ai_backend: AiBackendHandle,
        return_to_launcher_flag: Arc<AtomicBool>,
    ) -> Self {
        let ai_enabled = ai_backend.ai_enabled;
        // Enable puffin scope recording once at editor startup. Without this the
        // `profile_scope!` markers are no-ops and the profiler window stays empty.
        #[cfg(feature = "profiling")]
        puffin::set_scopes_on(true);
        let user_settings = crate::config::load_user_settings_for_startup().unwrap_or_else(|err| {
            runtime_log::log_warn(format!(
                "[memory] failed to load user settings for memory profile; using default: {err}"
            ));
            serde_json::Value::Object(serde_json::Map::new())
        });
        let memory_manager = Arc::new(MemoryManager::new(
            crate::config::memory_profile_from_user_settings(&user_settings),
        ));
        let cache_pages = project.canvas_settings.cache_pages;
        let cache_pages_on_initial_load =
            should_seed_page_cache_on_initial_load(cache_pages, memory_manager.budget());
        let pages: Vec<(usize, PathBuf)> = project
            .pages
            .iter()
            .map(|p| (p.idx, p.path.clone()))
            .collect();
        let page_infos = project
            .pages
            .iter()
            .map(|page| {
                (
                    page.idx,
                    PageImageInfo {
                        width_px: 0,
                        height_px: 0,
                        load_state: SourcePageLoadState::Loading,
                    },
                )
            })
            .collect();
        let (tx, rx) = sync_channel(LOADER_QUEUE_CAPACITY);
        let decode_promotion_progress = Arc::new(AtomicUsize::new(0));
        spawn_loader_thread(
            pages,
            tx,
            cache_pages_on_initial_load,
            Arc::clone(&decode_promotion_progress),
        );
        // Warm the process-global text-render FontSystem pool off the GUI thread. The first lease
        // pays a full system-font scan (~2.2s in a fresh process); doing it here on a detached
        // background thread keeps it off the critical path so the user's first text render finds a
        // warm system. Guarded by `Once` because `new` can run multiple times per process
        // (launcher<->studio transitions) and we want at most one prewarm thread.
        spawn_text_render_font_pool_prewarm();
        let mut canvas = CanvasView::default();
        let loaded_bubble_mode = BubbleMode::from_str(&project.canvas_settings.bubble_type);
        canvas.state.bubble_mode = BubbleMode::Hybrid;
        canvas.state.hybrid_editable_bubble_type = match loaded_bubble_mode {
            BubbleMode::Aside => BubbleType::Aside,
            BubbleMode::OnTop => BubbleType::OnTop,
            BubbleMode::Hybrid => {
                BubbleType::from_str(&project.canvas_settings.editable_bubble_type)
                    .resolved(BubbleType::OnTop)
            }
        };
        canvas.state.hybrid_readonly_bubble_type = match loaded_bubble_mode {
            BubbleMode::Aside => BubbleType::Aside,
            BubbleMode::OnTop => BubbleType::OnTop,
            BubbleMode::Hybrid => {
                BubbleType::from_str(&project.canvas_settings.readonly_bubble_type)
                    .resolved(BubbleType::Aside)
            }
        };
        canvas.state.show_bubbles = project.canvas_settings.show_bubbles;
        canvas.state.show_bubble_status = project.canvas_settings.show_bubble_status;
        canvas.state.bubble_status_rules = project.canvas_settings.bubble_status_rules.clone();
        canvas.state.bubble_opacity = project.canvas_settings.bubble_opacity;
        canvas.state.bubble_min_width = project.canvas_settings.aside_min_width_px as f32;
        canvas.state.bubble_max_width = project.canvas_settings.aside_max_width_px as f32;
        canvas.state.aside_compact_mode =
            AsideBubbleCompactMode::from_str(&project.canvas_settings.aside_compact_mode);
        canvas.state.aside_side_mode =
            AsideBubbleSideMode::from_str(&project.canvas_settings.aside_side_mode);
        canvas.state.aside_second_column = project.canvas_settings.aside_second_column;
        canvas.state.on_top_focus_mode =
            OnTopFocusMode::from_str(&project.canvas_settings.on_top_focus_mode);
        canvas.state.scale_bubbles = project.canvas_settings.scale_bubbles;
        canvas.state.page_spacing = project.canvas_settings.page_spacing_px as f32;
        canvas.state.separate_pages = project.canvas_settings.separate_pages;
        canvas.state.edge_margin = project.canvas_settings.vertical_edge_margin_px as f32;
        canvas.state.side_margin = project.canvas_settings.side_margin_px as f32;
        canvas.state.aside_scale_pct = project.canvas_settings.aside_scale_pct;
        canvas.state.auto_insert_last_character =
            project.canvas_settings.auto_insert_last_character;
        canvas.state.spellcheck_original = project.canvas_settings.spellcheck_original;
        canvas.state.spellcheck_translation = project.canvas_settings.spellcheck_translation;
        canvas.state.tabs_autosync_enabled = project.canvas_settings.tabs_autosync_enabled;
        canvas.state.cache_pages = cache_pages;
        canvas.state.translation_status_display =
            TranslationStatusDisplay::from_str(&project.canvas_settings.translation_status_display);
        let shared_canvas_settings = SharedCanvasSettings {
            bubble_type: BubbleMode::Hybrid.as_str().to_string(),
            editable_bubble_type: canvas
                .state
                .hybrid_editable_bubble_type
                .as_str()
                .to_string(),
            readonly_bubble_type: canvas
                .state
                .hybrid_readonly_bubble_type
                .as_str()
                .to_string(),
            show_bubbles: canvas.state.show_bubbles,
            show_bubble_status: canvas.state.show_bubble_status,
            bubble_status_rules: canvas.state.bubble_status_rules.clone(),
            bubble_opacity: canvas.state.bubble_opacity,
            page_spacing: canvas.state.page_spacing,
            separate_pages: canvas.state.separate_pages,
            edge_margin: canvas.state.edge_margin,
            side_margin: canvas.state.side_margin,
            bubble_min_width: canvas.state.bubble_min_width,
            bubble_max_width: canvas.state.bubble_max_width,
            aside_compact_mode: canvas.state.aside_compact_mode.as_str().to_string(),
            aside_side_mode: canvas.state.aside_side_mode.as_str().to_string(),
            aside_second_column: canvas.state.aside_second_column,
            on_top_focus_mode: canvas.state.on_top_focus_mode.as_str().to_string(),
            scale_bubbles: canvas.state.scale_bubbles,
            aside_scale_pct: canvas.state.aside_scale_pct,
            auto_insert_last_character: canvas.state.auto_insert_last_character,
            spellcheck_original: canvas.state.spellcheck_original,
            spellcheck_translation: canvas.state.spellcheck_translation,
            tabs_autosync_enabled: canvas.state.tabs_autosync_enabled,
            cache_pages: canvas.state.cache_pages,
            translation_status_display: canvas
                .state
                .translation_status_display
                .as_str()
                .to_string(),
        };
        let bubbles_model = Arc::new(Mutex::new(BubblesModel::new(
            project.bubbles.as_ref().clone(),
            project.paths.bubbles_file.clone(),
            project.paths.unsaved_bubbles_file.clone(),
            shared_canvas_settings,
        )));
        canvas.set_scroll_area_id_salt("translation_canvas_scroll");
        canvas.set_bubbles_model(Arc::clone(&bubbles_model));
        let shared_canvas_viewport = canvas.viewport_snapshot();
        let applied_bubbles_revision = bubbles_model.lock().map(|m| m.revision()).unwrap_or(0);
        // The backend process + health/device probe are owned app-globally by
        // `AiBackendSupervisor` (see `run_main`); the studio just shares its handle so
        // the backend keeps running across launcher<->studio transitions.
        let ai_backend_health = Arc::clone(&ai_backend.health);
        let ai_backend_probe_tx = ai_backend.probe_tx.clone();
        let text_mask_model = Arc::new(Mutex::new(TextMaskModel::new()));
        let mut translation_tab = TranslationTabState::new(
            ai_enabled,
            Arc::clone(&ai_backend_health),
            ai_backend_probe_tx.clone(),
        );
        translation_tab.set_text_mask_model(Arc::clone(&text_mask_model));
        let mut cleaning_tab = CleaningTabState::default();
        cleaning_tab.set_canvas_scroll_area_id_salt("cleaning_canvas_scroll");
        cleaning_tab.set_bubbles_model(Arc::clone(&bubbles_model));
        cleaning_tab.set_text_mask_model(Arc::clone(&text_mask_model));
        cleaning_tab.set_ai_backend_health(Arc::clone(&ai_backend_health));
        let page_paths: Vec<PathBuf> = project.pages.iter().map(|p| p.path.clone()).collect();
        let clean_overlays_model =
            Arc::new(Mutex::new(CleanOverlaysModel::new_from_pages(&page_paths)));
        if let Ok(mut overlays) = clean_overlays_model.lock() {
            overlays.set_cache_pages_enabled(canvas.state.cache_pages);
            overlays.set_memory_profile(memory_manager.profile());
        }
        cleaning_tab.set_overlays_model(Arc::clone(&clean_overlays_model));
        let mut typing_tab = TypingTabState::default();
        typing_tab.set_canvas_scroll_area_id_salt("typing_canvas_scroll");
        typing_tab.set_bubbles_model(Arc::clone(&bubbles_model));
        typing_tab.set_overlays_model(Arc::clone(&clean_overlays_model));
        let mut ps_editor_tab = PsEditorTabState::default();
        ps_editor_tab.set_overlays_model(Arc::clone(&clean_overlays_model));
        // Shared unified layer document, owned at the app level and held by both view tabs. It is the
        // source of truth for per-page layer MODEL state; each tab re-projects its current page when
        // the doc's `version` changes, which is the in-memory cross-tab sync.
        let layer_doc = std::sync::Arc::new(std::sync::Mutex::new(
            crate::models::layer_model::layer_doc::LayerDoc::new(),
        ));
        // Enable the off-thread coalescing layer saver ONCE on the shared doc, right after creation
        // and before either tab gets its clone. Heavy doc-routed flushes (PS per-edit/raster, typing
        // text) then enqueue PNG-encode + manifest RMW off the GUI thread; the save-to-project merge
        // worker and app-close drain barrier the queue so no write is lost. With no saver the doc's
        // enqueue_page_* methods fall back to a synchronous flush, so wiring is safe either way.
        if let Ok(mut doc) = layer_doc.lock() {
            doc.enable_background_saver();
        } else {
            runtime_log::log_error(
                "[app] could not enable background layer saver: shared doc lock poisoned at startup",
            );
        }
        typing_tab.set_layer_doc(std::sync::Arc::clone(&layer_doc));
        ps_editor_tab.set_layer_doc(std::sync::Arc::clone(&layer_doc));
        let mut settings_tab =
            SettingsTabState::new(ai_backend.clone(), Arc::clone(&memory_manager));
        settings_tab.set_canvas_settings_binding(
            project.paths.settings_file.clone(),
            bubbles_model
                .lock()
                .map(|model| model.canvas_snapshot())
                .unwrap_or_default(),
            Arc::clone(&bubbles_model),
            Arc::clone(&clean_overlays_model),
        );
        if let Some(layout) = settings_tab.take_typing_panel_layout_request() {
            typing_tab.set_panel_layout(layout);
        }
        let user_config_path = crate::config::user_config_path();
        let input_manager_v2 = build_input_manager_v2(user_config_path.as_path());
        let comic_type_prompt_open = project.comic_type.is_none();
        let has_unsaved_changes_cached = project.paths.unsaved_dir.exists();

        // Start the 30-second overlay autosave background thread.
        let overlay_autosave_thread = Some(spawn_overlay_autosave_thread(
            Arc::clone(&clean_overlays_model),
            project.paths.unsaved_clean_layers_dir.clone(),
        ));

        Self {
            project,
            memory_manager,
            memory_pressure: MemoryPressure::Normal,
            last_memory_pressure_poll: None,
            bubbles_model,
            clean_overlays_model,
            text_mask_model,
            applied_bubbles_revision,
            canvas,
            page_infos,
            textures: HashMap::new(),
            failed_pages: HashSet::new(),
            load_errors: Vec::new(),
            fonts_initialized: false,
            fonts_load_started: false,
            fonts_load_rx: None,
            loader_rx: rx,
            decoded_queue: VecDeque::new(),
            decoded_pending_by_idx: HashMap::new(),
            next_decode_idx_to_enqueue: 0,
            decode_promotion_progress,
            upload_task: None,
            loader_finished: false,
            overlay_loader_rx: None,
            overlay_loader_started: false,
            overlay_loader_finished: false,
            page_cache_loader_rx: None,
            page_cache_loader_started: false,
            page_cache_loader_finished: false,
            cached_hotkey_hints: CachedHotkeyHints::default(),
            active_tab: AppTab::Translation,
            shared_canvas_viewport,
            active_viewport_owner_tab: Some(AppTab::Translation),
            shared_page_idx: 0,
            shared_page_center: None,
            prev_view_tab: Some(AppTab::Translation),
            ai_backend_health,
            ai_backend_health_cached: AiBackendHealthSnapshot::default(),
            ai_backend_probe_tx: ai_backend_probe_tx.clone(),
            translation_tab,
            cleaning_tab,
            typing_tab,
            ps_editor_tab,
            layer_doc,
            characters_tab: CharactersTabState::default(),
            terms_tab: TermsTabState::default(),
            notes_tab: NotesTabState::default(),
            settings_tab,
            wiki_tab: WikiTabState::new(),
            input_manager_v2,
            comic_type_prompt_open,
            comic_type_prompt_error: None,
            ai_device_prompt_open: false,
            ai_device_prompt_initialized: false,
            ai_device_prompt_pytorch_device: String::new(),
            ai_device_prompt_directml_device_id: String::new(),
            ai_device_prompt_applying: false,
            ai_device_prompt_error: None,
            ai_backend_version_warning_open: false,
            ai_backend_version_warning_dismissed: false,
            overlay_autosave_thread,
            save_to_project_rx: None,
            save_to_project_status: None,
            pending_save_completion_action: None,
            pending_save_after_preload: false,
            exit_dialog: None,
            pending_exit_cleanup: None,
            has_unsaved_changes_cached,
            next_unsaved_dir_check_s: 0.0,
            #[cfg(target_os = "windows")]
            maximize_root_window_on_first_frame: true,
            return_to_launcher: false,
            return_to_launcher_flag,
            #[cfg(feature = "profiling")]
            profiler_window_open: true,
        }
    }

    /// Returns true if there are in-session changes not yet merged into the project folder.
    fn has_unsaved_changes(&self) -> bool {
        self.has_unsaved_changes_cached
    }

    /// Refreshes the cached unsaved-changes flag.
    ///
    /// `has_unsaved_changes_cached` is a sticky latch that is only cleared on an explicit
    /// save (`mark_saved_to_project`). While it is already true there is nothing to
    /// recompute, so the per-frame model locks and the staging-dir probe are skipped.
    /// While it is false the work is throttled to roughly once per second via the existing
    /// `next_unsaved_dir_check_s` timer; this keeps the GUI thread off the shared model
    /// locks every frame while still surfacing new unsaved state within ~1s.
    fn refresh_unsaved_changes_cache(&mut self, now: f64) {
        if self.has_unsaved_changes_cached || now < self.next_unsaved_dir_check_s {
            return;
        }
        self.next_unsaved_dir_check_s = now + 1.0;
        if let Ok(model) = self.bubbles_model.lock()
            && model.has_unsaved_changes()
        {
            self.has_unsaved_changes_cached = true;
            return;
        }
        if let Ok(model) = self.clean_overlays_model.lock()
            && model.has_project_unsaved_changes()
        {
            self.has_unsaved_changes_cached = true;
            return;
        }
        self.has_unsaved_changes_cached = self.project.paths.unsaved_dir.exists();
    }

    /// Entry point for every "save to project" trigger (the toolbar button and both exit-dialog
    /// "save chapter" buttons). Gates the save behind the whole-project layer preload so every page
    /// that can load has its lazy legacy→v3 text migration baked into the committed `layers.json`.
    ///
    /// Fast path: when every page is already resident (`all_pages_loaded`), it runs
    /// `start_save_to_project` immediately (no behavior change from before). Otherwise it DEFERS:
    /// it starts the async, non-blocking preload (`begin_preload_all_pages`) and sets
    /// `pending_save_after_preload`; `drive_pending_save_preload`, called every frame, advances the
    /// preload and dispatches the real save once the PASS drains (not full residency — a page that
    /// fails to decode never becomes resident, so a residency gate would hang; its committed text is
    /// preserved verbatim). A save already running or already pending on the preload is a no-op
    /// (idempotent trigger).
    ///
    /// Only LAYER residency is gated: masks are copied verbatim from the committed `text_images/` by
    /// the merge (no in-memory mask data is consumed), and клин edits are captured synchronously via
    /// `take_dirty_save_snapshots`, so gating on the mask loader or the клин loader would have no
    /// save-time correctness effect (CLAUDE.md §14).
    fn request_save_to_project(&mut self) {
        match save_trigger_decision(
            self.save_to_project_rx.is_some(),
            self.pending_save_after_preload,
            self.typing_tab.all_pages_loaded(&self.project),
        ) {
            SaveGateDecision::Ignore => {}
            SaveGateDecision::SaveNow => self.start_save_to_project(),
            SaveGateDecision::Defer => {
                // Start the whole-project layer preload (idempotent; wires the typing loader dirs if
                // the tab was never visited) and remember the intent. `drive_pending_save_preload`
                // completes it from the frame loop regardless of which tab is active.
                self.typing_tab.begin_preload_all_pages(&self.project);
                self.pending_save_after_preload = true;
                let (done, total) = self.typing_tab.preload_all_pages_progress();
                self.save_to_project_status = Some((
                    tf!("app.save.preparing_pages", done = done, total = total),
                    0.0,
                ));
            }
        }
    }

    /// Per-frame driver for a save deferred behind the whole-project layer preload. Advances the
    /// preload (the typing tab drives its own preload only while it is the active tab, so the save
    /// path must drive it here to stay correct on any tab), keeps repainting so the decode results
    /// drain, and dispatches the real `start_save_to_project` once the preload PASS has drained
    /// (`deferred_save_ready`, NOT full residency — see that fn for why). No-op unless
    /// `pending_save_after_preload`.
    fn drive_pending_save_preload(&mut self, ctx: &egui::Context) {
        if !self.pending_save_after_preload {
            return;
        }
        // Advance the preload from the frame loop (independent of the active tab) and keep the frame
        // loop alive so decode results keep draining to completion.
        self.typing_tab.drive_page_preload();
        ctx.request_repaint();
        if deferred_save_ready(self.typing_tab.preload_all_pages_active()) {
            // The preload PASS has drained (every page that could decode is resident): run the real
            // save now. `flush_text_layers` flushes every resident page, so the merge is authoritative
            // for them and the committed `layers.json` becomes v3-complete. A page that genuinely
            // failed to decode never became resident; the merge keeps its committed text verbatim
            // (it is simply absent from `owned_text_pages`), so the save stays correct without hanging.
            self.pending_save_after_preload = false;
            self.start_save_to_project();
        } else {
            // Still preparing: refresh the "Подготовка страниц N/M" status.
            let (done, total) = self.typing_tab.preload_all_pages_progress();
            self.save_to_project_status =
                Some((tf!("app.save.preparing_pages", done = done, total = total), 0.0));
        }
    }

    /// Snapshot dirty overlay pages, save them in a background thread, then merge the
    /// unsaved staging dir into the project folder and remove the staging dir on completion.
    fn start_save_to_project(&mut self) {
        if self.save_to_project_rx.is_some() {
            return;
        }
        // Flush the PS editor's active-page raster layers into the staging dir so the merge below
        // picks them up. Other visited pages were already written on their page switch.
        self.ps_editor_tab.flush_layers(&self.project);
        // Flush the typing tab's text overlays (inline v3 payload) into the staging `layers.json` so a
        // legacy chapter that was only viewed (no edits → no placement save) still migrates its text
        // before the unsaved→committed merge. The typing tab owns text persistence now. The returned set
        // is the OWNED text pages (doc-resident this session) — the merge replaces those pages' text and
        // preserves committed text for pages NOT in it (no silent drop on a PS raster-only edit).
        let owned_text_pages = self.typing_tab.flush_text_layers();
        // Capture the background layer-saver handle on the GUI thread (cheap Sender clone; no doc lock
        // held during the merge). The PS `flush_layers` + typing `flush_text_layers` above now ENQUEUE
        // their page writes; the merge worker must BARRIER the saver before reading the staging
        // `layers.json` so every enqueued page is on disk first. The FIFO channel guarantees the
        // just-enqueued jobs precede the barrier. `None` ⇒ no saver enabled, so the flushes already
        // wrote synchronously and no barrier is needed.
        let saver_handle = self
            .layer_doc
            .lock()
            .ok()
            .and_then(|guard| guard.saver_handle());
        let unsaved_dir = self.project.paths.unsaved_dir.clone();
        let project_dir = self.project.paths.project_dir.clone();
        let unsaved_clean_layers_dir = self.project.paths.unsaved_clean_layers_dir.clone();
        let clean_overlays_model = Arc::clone(&self.clean_overlays_model);
        let dirty_overlay_snapshots = match self.clean_overlays_model.lock() {
            Ok(mut overlays) => overlays.take_dirty_save_snapshots(),
            Err(_) => Vec::new(),
        };
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        thread::spawn(move || {
            // Drain the layer saver FIRST: block until every enqueued layer page write (rasters + text
            // + effects from the flushes above) is on disk, so the merge reads a complete staging
            // `layers.json`. This barrier runs in the worker, never on the GUI thread.
            let failed_pages = saver_handle.map_or_else(HashSet::new, |handle| handle.barrier_blocking());
            // A failed staging write is not authoritative: preserve its committed text instead of
            // overwriting it with incomplete or stale staging data.
            let effective_owned_text_pages: HashSet<usize> = owned_text_pages
                .difference(&failed_pages)
                .copied()
                .collect();
            if let Err(err) =
                save_overlay_snapshots_to(&unsaved_clean_layers_dir, &dirty_overlay_snapshots)
            {
                if let Ok(mut overlays) = clean_overlays_model.lock() {
                    overlays.restore_dirty_save_indexes(
                        dirty_overlay_snapshots.iter().map(|(idx, _, _)| *idx),
                    );
                }
                let _ = tx.send(Err(format!(
                    "failed to flush dirty overlays into '{}': {err}",
                    unsaved_clean_layers_dir.display()
                )));
                return;
            }
            let result = merge_unsaved_into_project(&unsaved_dir, &project_dir, &effective_owned_text_pages);
            let _ = tx.send(result);
        });
        self.save_to_project_rx = Some(rx);
        self.save_to_project_status = Some((t!("app.save.saving").to_string(), 0.0));
    }

    /// Poll the in-flight "save to project" job.  Returns true when the job completed this frame.
    fn poll_save_to_project(&mut self, now: f64) -> bool {
        let Some(rx) = self.save_to_project_rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.save_to_project_rx = None;
                if let Ok(mut model) = self.bubbles_model.lock() {
                    model.mark_saved_to_project();
                }
                if let Ok(mut model) = self.clean_overlays_model.lock() {
                    model.mark_saved_to_project();
                }
                self.has_unsaved_changes_cached = false;
                self.save_to_project_status = Some((t!("app.save.saved").to_string(), now));
                runtime_log::log_info("[save_to_project] merge complete");
                true
            }
            Ok(Err(err)) => {
                self.save_to_project_rx = None;
                let msg = tf!("app.save.error", err = err);
                runtime_log::log_error(format!("[save_to_project] {msg}"));
                self.save_to_project_status = Some((msg, now));
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.save_to_project_rx = None;
                let msg = t!("app.save.thread_crashed").to_string();
                self.save_to_project_status = Some((msg.clone(), now));
                runtime_log::log_error(format!("[save_to_project] {msg}"));
                true
            }
        }
    }

    /// Delete the unsaved staging folder in a background thread and report the result.
    fn spawn_unsaved_delete_job(
        unsaved_dir: PathBuf,
    ) -> std::sync::mpsc::Receiver<Result<(), String>> {
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        thread::spawn(move || {
            let result = if unsaved_dir.exists() {
                fs::remove_dir_all(&unsaved_dir).map_err(|err| {
                    tf!("app.save.remove_temp_error", unsaved_dir = unsaved_dir.display(), err = err)
                })
            } else {
                Ok(())
            };
            let _ = tx.send(result);
        });
        rx
    }

    fn start_exit_cleanup(&mut self, action: PendingCloseAction) {
        if self.pending_exit_cleanup.is_some() {
            return;
        }
        // Stop the layer saver BEFORE deleting the unsaved dir. The discard path removes the staging
        // folder the saver writes into; a still-running saver could recreate files mid/post deletion,
        // leaving the staging dir partially present. `shutdown_saver` drains the queue then JOINS the
        // worker, guaranteeing it is no longer writing before the delete job spawns — closing that
        // race. We do not barrier on the GUI thread here: this is the DISCARD path (the user chose not
        // to save), so the queued writes target the dir being deleted; only the join matters. The
        // join (drain of a small queue) is bounded and runs only at teardown.
        if let Ok(mut guard) = self.layer_doc.lock() {
            guard.shutdown_saver();
        }
        let rx = Self::spawn_unsaved_delete_job(self.project.paths.unsaved_dir.clone());
        self.pending_exit_cleanup = Some(PendingExitCleanup { action, rx });
        self.save_to_project_status = Some((t!("app.cleanup.cleaning").to_string(), 0.0));
    }

    fn finalize_close(&mut self, ctx: &egui::Context, action: PendingCloseAction) {
        match action {
            PendingCloseAction::Exit => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            PendingCloseAction::ReturnToLauncher => {
                self.return_to_launcher = true;
                self.return_to_launcher_flag
                    .store(true, AtomicOrdering::SeqCst);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn poll_pending_exit_cleanup(&mut self, ctx: &egui::Context, now: f64) {
        let Some(cleanup) = self.pending_exit_cleanup.as_ref() else {
            return;
        };
        match cleanup.rx.try_recv() {
            Ok(Ok(())) => {
                let action = cleanup.action;
                self.pending_exit_cleanup = None;
                self.exit_dialog = None;
                if let Ok(mut model) = self.bubbles_model.lock() {
                    model.mark_saved_to_project();
                }
                if let Ok(mut model) = self.clean_overlays_model.lock() {
                    model.mark_saved_to_project();
                }
                self.has_unsaved_changes_cached = false;
                self.save_to_project_status = Some((t!("app.cleanup.cleaned").to_string(), now));
                runtime_log::log_info("[exit] unsaved cleanup complete");
                self.finalize_close(ctx, action);
            }
            Ok(Err(err)) => {
                self.pending_exit_cleanup = None;
                self.exit_dialog = None;
                let msg = tf!("app.cleanup.error", err = err);
                self.save_to_project_status = Some((msg.clone(), now));
                runtime_log::log_error(format!("[exit] {msg}"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending_exit_cleanup = None;
                let msg = t!("app.cleanup.thread_crashed").to_string();
                self.save_to_project_status = Some((msg.clone(), now));
                runtime_log::log_error(format!("[exit] {msg}"));
            }
        }
    }

    /// Draw and handle the active exit/leave dialog.
    /// Returns true when the dialog requested an app exit that was already dispatched.
    fn draw_exit_dialog(&mut self, ctx: &egui::Context) -> bool {
        let Some(kind) = self.exit_dialog else {
            return false;
        };

        let mut close_dialog = false;
        let mut handled = false;

        let title = match kind {
            ExitDialogKind::WindowClose => t!("app.exit.exit_button"),
            ExitDialogKind::ReturnToLauncher => t!("app.exit.to_launcher_button"),
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                match kind {
                    ExitDialogKind::WindowClose => {
                        ui.label(t!("app.exit.unsaved_warning"));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button(t!("app.exit.cancel_button")).clicked() {
                                close_dialog = true;
                            }
                            if ui.button(t!("app.exit.save_chapter_button")).clicked() {
                                self.request_save_to_project();
                                // Wait for the save to complete before we exit.
                                // We set a flag so the next poll triggers the close.
                                close_dialog = true;
                                // We'll close after save completes — handled by
                                // a pending rx; the close below is re-attempted next frame.
                                // For simplicity: schedule delayed close after merge.
                                self.exit_dialog = Some(ExitDialogKind::WindowClose);
                                handled = true; // skip the unconditional close_dialog below
                            }
                            if ui.button(t!("app.exit.discard_button")).clicked() {
                                self.start_exit_cleanup(PendingCloseAction::Exit);
                                close_dialog = true;
                                handled = true;
                            }
                        });
                    }
                    ExitDialogKind::ReturnToLauncher => {
                        ui.label(t!("app.exit.unsaved_warning"));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button(t!("app.exit.cancel_button")).clicked() {
                                close_dialog = true;
                            }
                            if ui.button(t!("app.exit.save_chapter_button")).clicked() {
                                self.pending_save_completion_action =
                                    Some(PendingCloseAction::ReturnToLauncher);
                                self.request_save_to_project();
                                close_dialog = true;
                                handled = true;
                            }
                            if ui.button(t!("app.exit.discard_button")).clicked() {
                                self.start_exit_cleanup(PendingCloseAction::ReturnToLauncher);
                                close_dialog = true;
                                handled = true;
                            }
                        });
                    }
                }
            });

        if close_dialog && !handled {
            self.exit_dialog = None;
        }
        handled
    }

    fn apply_comic_type_preset(&mut self, comic_type: ComicType) {
        self.project.comic_type = Some(comic_type);
        if let Some(root) = self.project.settings_data.as_object_mut() {
            root.insert(
                "comic_type".to_string(),
                serde_json::Value::String(comic_type.as_config_str().to_string()),
            );
        }
        if let Some((aside_compact_mode, separate_pages)) = comic_type.canvas_preset() {
            self.project.canvas_settings.aside_compact_mode = aside_compact_mode.to_string();
            self.project.canvas_settings.separate_pages = separate_pages;
            self.canvas.state.aside_compact_mode =
                AsideBubbleCompactMode::from_str(aside_compact_mode);
            self.canvas.state.separate_pages = separate_pages;
        }

        if let Err(err) =
            save_comic_type_to_project_file(&self.project.paths.settings_file, comic_type)
        {
            let user_message = t!("app.comic_type.save_error");
            self.comic_type_prompt_error = Some(user_message.to_string());
            runtime_log::log_error(format!(
                "[app] failed to persist comic_type='{}'; settings_file='{}'; cause={err}",
                comic_type.as_config_str(),
                self.project.paths.settings_file.display()
            ));
            eprintln!(
                "ERROR app::apply_comic_type_preset comic_type={} settings_file={} error={}",
                comic_type.as_config_str(),
                self.project.paths.settings_file.display(),
                err
            );
            return;
        }

        let snapshot = SharedCanvasSettings {
            bubble_type: BubbleMode::Hybrid.as_str().to_string(),
            editable_bubble_type: self
                .canvas
                .state
                .hybrid_editable_bubble_type
                .as_str()
                .to_string(),
            readonly_bubble_type: self
                .canvas
                .state
                .hybrid_readonly_bubble_type
                .as_str()
                .to_string(),
            show_bubbles: self.canvas.state.show_bubbles,
            show_bubble_status: self.canvas.state.show_bubble_status,
            bubble_status_rules: self.canvas.state.bubble_status_rules.clone(),
            bubble_opacity: self.canvas.state.bubble_opacity,
            page_spacing: self.canvas.state.page_spacing,
            separate_pages: self.canvas.state.separate_pages,
            edge_margin: self.canvas.state.edge_margin,
            side_margin: self.canvas.state.side_margin,
            bubble_min_width: self.canvas.state.bubble_min_width,
            bubble_max_width: self.canvas.state.bubble_max_width,
            aside_compact_mode: self.canvas.state.aside_compact_mode.as_str().to_string(),
            aside_side_mode: self.canvas.state.aside_side_mode.as_str().to_string(),
            aside_second_column: self.canvas.state.aside_second_column,
            on_top_focus_mode: self.canvas.state.on_top_focus_mode.as_str().to_string(),
            scale_bubbles: self.canvas.state.scale_bubbles,
            aside_scale_pct: self.canvas.state.aside_scale_pct,
            auto_insert_last_character: self.canvas.state.auto_insert_last_character,
            spellcheck_original: self.canvas.state.spellcheck_original,
            spellcheck_translation: self.canvas.state.spellcheck_translation,
            tabs_autosync_enabled: self.canvas.state.tabs_autosync_enabled,
            cache_pages: self.canvas.state.cache_pages,
            translation_status_display: self
                .canvas
                .state
                .translation_status_display
                .as_str()
                .to_string(),
        };

        if let Ok(mut bubbles_model) = self.bubbles_model.lock() {
            bubbles_model.set_canvas_settings(snapshot.clone());
        } else {
            runtime_log::log_warn(
                "[app] failed to lock BubblesModel while applying comic_type preset",
            );
            eprintln!("WARN app::apply_comic_type_preset: failed to lock BubblesModel");
        }
        if let Ok(mut overlays_model) = self.clean_overlays_model.lock() {
            overlays_model.set_cache_pages_enabled(snapshot.cache_pages);
        } else {
            runtime_log::log_warn(
                "[app] failed to lock CleanOverlaysModel while applying comic_type preset",
            );
            eprintln!("WARN app::apply_comic_type_preset: failed to lock CleanOverlaysModel");
        }
        self.settings_tab
            .replace_canvas_settings_from_snapshot(snapshot.clone());
        self.settings_tab.persist_canvas_settings();

        self.comic_type_prompt_error = None;
        self.comic_type_prompt_open = false;
    }

    fn draw_comic_type_prompt(&mut self, ctx: &egui::Context) {
        if !self.comic_type_prompt_open {
            return;
        }

        egui::Window::new(t!("app.comic_type.window_title")).id(egui::Id::new("app.comic_type.window_title"))
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .show(ctx, |ui| {
                if let Some(message) = self.comic_type_prompt_error.as_ref() {
                    ui.colored_label(egui::Color32::from_rgb(220, 90, 90), message);
                    ui.add_space(8.0);
                }
                if ui.button(t!("app.comic_type.pages_option")).clicked() {
                    self.apply_comic_type_preset(ComicType::Pages);
                }
                if ui.button(t!("app.comic_type.ribbon_option")).clicked() {
                    self.apply_comic_type_preset(ComicType::Ribbon);
                }
            });
    }

    /// Refreshes the per-frame AI backend health cache from the shared probe state.
    ///
    /// Must be called once per frame before any helper that reads
    /// `ai_backend_health_cached`, so the shared mutex is locked and cloned at most once
    /// per frame instead of per consumer.
    fn refresh_ai_backend_health_cache(&mut self) {
        self.ai_backend_health_cached = match self.ai_backend_health.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
    }

    fn refresh_ai_device_selection_prompt(&mut self) {
        if self.ai_device_prompt_open {
            return;
        }

        let snapshot = &self.ai_backend_health_cached;
        if !snapshot.connected
            || (!snapshot.torch_device_needs_selection && !snapshot.onnx_device_needs_selection)
        {
            return;
        }

        self.ai_device_prompt_open = true;
        self.ai_device_prompt_initialized = false;
        self.ai_device_prompt_applying = false;
        self.ai_device_prompt_error = None;
        if let Some(tx) = self.ai_backend_probe_tx.as_ref()
            && let Err(err) = tx.send(AiBackendProbeCommand::RefreshDeviceInfo)
        {
            runtime_log::log_warn(format!(
                "[app] failed to request AI backend device refresh: {err}"
            ));
        }
    }

    fn draw_ai_device_selection_prompt(&mut self, ctx: &egui::Context) {
        if !self.ai_device_prompt_open {
            return;
        }

        // Clone the per-frame cached snapshot (no shared lock): the device-selection window
        // closure below mutably borrows `self`, so it needs an owned snapshot to read from.
        let snapshot = self.ai_backend_health_cached.clone();
        if !snapshot.connected
            || (!snapshot.torch_device_needs_selection && !snapshot.onnx_device_needs_selection)
        {
            self.ai_device_prompt_open = false;
            self.ai_device_prompt_initialized = false;
            self.ai_device_prompt_applying = false;
            return;
        }

        let directml_options = snapshot
            .onnx_devices_by_provider
            .get("DmlExecutionProvider")
            .cloned()
            .unwrap_or_else(|| snapshot.onnx_device_options.clone());

        if !self.ai_device_prompt_initialized {
            self.ai_device_prompt_pytorch_device = snapshot
                .selected_device
                .clone()
                .or_else(|| {
                    snapshot
                        .device_options
                        .first()
                        .map(|option| option.id.clone())
                })
                .unwrap_or_default();
            self.ai_device_prompt_directml_device_id = snapshot
                .selected_onnx_device_id
                .clone()
                .or_else(|| directml_options.first().map(|option| option.id.clone()))
                .unwrap_or_default();
            self.ai_device_prompt_initialized = true;
        }

        egui::Window::new(t!("app.gpu_prompt.window_title")).id(egui::Id::new("app.gpu_prompt.window_title"))
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .show(ctx, |ui| {
                ui.label(t!("app.gpu_prompt.hint"));
                ui.add_space(8.0);

                if let Some(message) = self.ai_device_prompt_error.as_ref() {
                    ui.colored_label(egui::Color32::from_rgb(220, 90, 90), message);
                    ui.add_space(8.0);
                }
                if self.ai_device_prompt_applying {
                    ui.small(t!("app.gpu_prompt.applying"));
                    ui.add_space(8.0);
                }

                if snapshot.torch_device_needs_selection && !snapshot.device_options.is_empty() {
                    let selected_text = option_label(
                        &snapshot.device_options,
                        &self.ai_device_prompt_pytorch_device,
                    );
                    // Stable id_salt so a live UI-language switch does not reset the
                    // combo state (native `ComboBox` derives its id from the label text
                    // unless given an explicit salt).
                    egui::ComboBox::new("ai_device_prompt_pytorch_combo", t!("app.gpu_prompt.pytorch_device_label"))
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            for option in &snapshot.device_options {
                                ui.selectable_value(
                                    &mut self.ai_device_prompt_pytorch_device,
                                    option.id.clone(),
                                    option.label.as_str(),
                                );
                            }
                        });
                    ui.add_space(6.0);
                }

                if snapshot.onnx_device_needs_selection {
                    let selected_text =
                        option_label(&directml_options, &self.ai_device_prompt_directml_device_id);
                    // Stable id_salt (see the PyTorch combo above).
                    egui::ComboBox::new("ai_device_prompt_directml_combo", t!("app.gpu_prompt.directml_device_label"))
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            for option in &directml_options {
                                ui.selectable_value(
                                    &mut self.ai_device_prompt_directml_device_id,
                                    option.id.clone(),
                                    option.label.as_str(),
                                );
                            }
                        });
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.ai_device_prompt_applying,
                            egui::Button::new(t!("app.gpu_prompt.confirm_button")),
                        )
                        .clicked()
                    {
                        if snapshot.onnx_device_needs_selection
                            && self.ai_device_prompt_directml_device_id.trim().is_empty()
                        {
                            self.ai_device_prompt_error =
                                Some(t!("app.gpu_prompt.select_directml_hint").to_string());
                            return;
                        }
                        if let Some(tx) = self.ai_backend_probe_tx.as_ref() {
                            let mut command_sent = false;
                            if snapshot.torch_device_needs_selection
                                && !self.ai_device_prompt_pytorch_device.trim().is_empty()
                            {
                                match tx.send(AiBackendProbeCommand::SetDevice(
                                    self.ai_device_prompt_pytorch_device.clone(),
                                )) {
                                    Ok(()) => {
                                        command_sent = true;
                                    }
                                    Err(err) => {
                                        runtime_log::log_warn(format!(
                                            "[app] failed to send PyTorch device selection: {err}"
                                        ));
                                    }
                                }
                            }
                            if snapshot.onnx_device_needs_selection {
                                match tx.send(AiBackendProbeCommand::SetOnnxDevice {
                                    provider: "DmlExecutionProvider".to_string(),
                                    device_id: self.ai_device_prompt_directml_device_id.clone(),
                                }) {
                                    Ok(()) => {
                                        command_sent = true;
                                    }
                                    Err(err) => {
                                        runtime_log::log_warn(format!(
                                            "[app] failed to send DirectML device selection: {err}"
                                        ));
                                    }
                                }
                            }
                            if command_sent {
                                self.ai_device_prompt_error = None;
                                self.ai_device_prompt_applying = true;
                            } else {
                                self.ai_device_prompt_error =
                                    Some(t!("app.gpu_prompt.send_failed").to_string());
                            }
                        } else {
                            self.ai_device_prompt_error =
                                Some(t!("app.gpu_prompt.backend_unavailable").to_string());
                        }
                    }
                });
            });
    }

    fn current_ai_backend_version_mismatch(&self) -> Option<(String, String)> {
        let snapshot = &self.ai_backend_health_cached;
        if !snapshot.connected {
            return None;
        }

        let studio_version = env!("CARGO_PKG_VERSION").trim().to_string();
        let backend_version = snapshot
            .backend_version
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(t!("app.version.unknown"))
            .to_string();
        if backend_version == studio_version {
            return None;
        }

        Some((studio_version, backend_version))
    }

    fn refresh_ai_backend_version_warning(&mut self) {
        if self.ai_backend_version_warning_open || self.ai_backend_version_warning_dismissed {
            return;
        }
        let Some((studio_version, backend_version)) = self.current_ai_backend_version_mismatch()
        else {
            return;
        };

        runtime_log::log_warn(format!(
            "[app] AI backend version mismatch: studio={studio_version} backend={backend_version}"
        ));
        self.ai_backend_version_warning_open = true;
    }

    fn draw_ai_backend_version_warning(&mut self, ctx: &egui::Context) {
        if !self.ai_backend_version_warning_open {
            return;
        }

        let Some((studio_version, backend_version)) = self.current_ai_backend_version_mismatch()
        else {
            self.ai_backend_version_warning_open = false;
            return;
        };
        let message = ai_backend_version_warning_message(&studio_version, &backend_version);

        egui::Window::new(t!("app.version_warning.window_title")).id(egui::Id::new("app.version_warning.window_title"))
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .show(ctx, |ui| {
                ui.label(message);
                ui.add_space(10.0);
                if ui.button("OK").clicked() {
                    self.ai_backend_version_warning_dismissed = true;
                    self.ai_backend_version_warning_open = false;
                }
            });
    }

    fn apply_shared_viewport_to_active_canvas(&mut self) {
        let entering_from_ps = self.prev_view_tab == Some(AppTab::PsEditor);
        // Leaving the PS editor: flush its in-memory raster/group edits to disk and bump the shared
        // layer revision, so the destination tab reloads the latest state (e.g. a raster the PS
        // editor just cut into two). PS otherwise only persists on a page-switch, not a tab-switch.
        // Gated on the dirty flag: per-edit changes already enqueued their page write, so an
        // unchanged page skips a redundant snapshot+enqueue on every tab switch.
        if entering_from_ps && self.active_tab != AppTab::PsEditor {
            self.ps_editor_tab.flush_layers_if_dirty(&self.project);
        }
        match self.active_tab {
            AppTab::Translation | AppTab::Cleaning | AppTab::Typing => {
                // Canvas <-> canvas: reapply the precise scroll snapshot when the owner changed.
                if self.active_viewport_owner_tab != Some(self.active_tab) {
                    let snapshot = self.shared_canvas_viewport;
                    match self.active_tab {
                        AppTab::Translation => self.canvas.apply_viewport_snapshot(snapshot),
                        AppTab::Cleaning => self.cleaning_tab.apply_viewport_snapshot(snapshot),
                        AppTab::Typing => self.typing_tab.apply_viewport_snapshot(snapshot),
                        _ => {}
                    }
                    self.active_viewport_owner_tab = Some(self.active_tab);
                }
                // Returning from the PS editor: the scroll snapshot is stale, so re-center the
                // canvas on the page/zoom the PS editor handed back.
                if entering_from_ps {
                    let zoom = self.shared_canvas_viewport.zoom;
                    let page = self.shared_page_idx;
                    let center = self.shared_page_center;
                    match self.active_tab {
                        AppTab::Translation => self.canvas.focus_page(page, center, zoom),
                        AppTab::Cleaning => self.cleaning_tab.focus_page(page, center, zoom),
                        AppTab::Typing => self.typing_tab.focus_page(page, center, zoom),
                        _ => {}
                    }
                }
            }
            // Entering the PS editor: sync page + clean overlay + camera from the canvas world.
            AppTab::PsEditor if self.prev_view_tab != Some(AppTab::PsEditor) => {
                let zoom = self.shared_canvas_viewport.zoom;
                let page = self.shared_page_idx;
                let center = self.shared_page_center;
                self.ps_editor_tab
                    .sync_view_from_canvas(&self.project, page, zoom, center);
            }
            _ => {}
        }
    }

    fn publish_shared_viewport_from_active_canvas(&mut self) {
        match self.active_tab {
            AppTab::Translation | AppTab::Cleaning | AppTab::Typing => {
                let (snapshot, page_center) = match self.active_tab {
                    AppTab::Translation => (
                        self.canvas.viewport_snapshot(),
                        self.canvas.current_page_local_view_center(),
                    ),
                    AppTab::Cleaning => (
                        self.cleaning_tab.viewport_snapshot(),
                        self.cleaning_tab.current_page_local_view_center(),
                    ),
                    AppTab::Typing => (
                        self.typing_tab.viewport_snapshot(),
                        self.typing_tab.current_page_local_view_center(),
                    ),
                    _ => unreachable!(),
                };
                self.shared_canvas_viewport = snapshot;
                self.active_viewport_owner_tab = Some(self.active_tab);
                if let Some((page, center)) = page_center {
                    self.shared_page_idx = page;
                    self.shared_page_center = Some(center);
                }
            }
            AppTab::PsEditor => {
                // The PS editor never owns the canvas scroll snapshot, but it does feed the shared
                // page, zoom, and camera center back so the canvas can follow it on the way out.
                if let Some(page) = self.ps_editor_tab.current_page() {
                    self.shared_page_idx = page;
                }
                let (zoom, center) = self.ps_editor_tab.camera();
                self.shared_canvas_viewport.zoom = zoom;
                self.shared_page_center = Some(center);
            }
            _ => {}
        }
        if matches!(
            self.active_tab,
            AppTab::Translation | AppTab::Cleaning | AppTab::Typing | AppTab::PsEditor
        ) {
            self.prev_view_tab = Some(self.active_tab);
        }
    }

    fn dispatch_hotkeys(&mut self, ctx: &egui::Context) {
        let triggered_v2 = self
            .input_manager_v2
            .collect_triggered(ctx, self.active_tab);
        for command_id in triggered_v2 {
            self.execute_hotkey_command(ctx, &command_id);
        }
    }

    fn execute_hotkey_command(&mut self, ctx: &egui::Context, command_id: &str) {
        match command_id {
            HOTKEY_TRANSLATION_ZOOM_IN => {
                if !self.translation_tab.blocks_canvas_zoom() {
                    self.canvas.zoom_by_shortcut(1.1);
                }
            }
            HOTKEY_TRANSLATION_ZOOM_OUT => {
                if !self.translation_tab.blocks_canvas_zoom() {
                    self.canvas.zoom_by_shortcut(1.0 / 1.1);
                }
            }
            HOTKEY_TRANSLATION_ZOOM_RESET => {
                if !self.translation_tab.blocks_canvas_zoom() {
                    self.canvas.reset_zoom_shortcut();
                }
            }
            HOTKEY_TRANSLATION_DELETE_BUBBLE => {
                if !self.translation_tab.blocks_canvas_bubble_hotkeys() {
                    self.canvas.delete_selected_bubble_shortcut();
                }
            }
            HOTKEY_TRANSLATION_CREATE_BUBBLE => {
                if !self.translation_tab.blocks_canvas_bubble_hotkeys()
                    && let Some(pointer_pos) = ctx.pointer_latest_pos()
                {
                    self.translation_tab.create_bubble_at_pointer_shortcut(
                        ctx,
                        &mut self.canvas,
                        &self.project,
                        pointer_pos,
                    );
                }
            }
            HOTKEY_TRANSLATION_COPY_BUBBLE_ORIGINAL => {
                if !self.translation_tab.blocks_canvas_bubble_hotkeys() {
                    self.canvas
                        .copy_selected_bubble_text_shortcut(ctx, BubbleTextField::Original);
                }
            }
            HOTKEY_TRANSLATION_COPY_BUBBLE_TRANSLATION => {
                if !self.translation_tab.blocks_canvas_bubble_hotkeys() {
                    self.canvas
                        .copy_selected_bubble_text_shortcut(ctx, BubbleTextField::Translation);
                }
            }
            HOTKEY_TRANSLATION_PASTE_BUBBLE_ORIGINAL => {
                if !self.translation_tab.blocks_canvas_bubble_hotkeys() {
                    self.canvas
                        .paste_selected_bubble_text_shortcut(ctx, BubbleTextField::Original);
                }
            }
            HOTKEY_TRANSLATION_PASTE_BUBBLE_TRANSLATION => {
                if !self.translation_tab.blocks_canvas_bubble_hotkeys() {
                    self.canvas
                        .paste_selected_bubble_text_shortcut(ctx, BubbleTextField::Translation);
                }
            }
            HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL => {
                self.translation_tab.toggle_bubbles_panel_hotkey();
            }
            HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL => {
                self.translation_tab.toggle_ocr_panel_hotkey();
            }
            HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL => {
                self.translation_tab.toggle_composition_panel_hotkey();
            }
            HOTKEY_TRANSLATION_TOGGLE_MT_PANEL => {
                self.translation_tab
                    .toggle_machine_translation_panel_hotkey();
            }
            HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL => {
                self.translation_tab.toggle_text_detector_panel_hotkey();
            }
            HOTKEY_CLEANING_ZOOM_IN => {
                self.cleaning_tab.zoom_by_shortcut(1.1);
            }
            HOTKEY_CLEANING_ZOOM_OUT => {
                self.cleaning_tab.zoom_by_shortcut(1.0 / 1.1);
            }
            HOTKEY_CLEANING_ZOOM_RESET => {
                self.cleaning_tab.reset_zoom_shortcut();
            }
            _ => {}
        }
    }

    fn poll_loader_events(&mut self) {
        for _ in 0..EVENT_POLL_BUDGET {
            match self.loader_rx.try_recv() {
                Ok(LoaderEvent::Decoded(page)) => {
                    if let Some(info) = self.page_infos.get_mut(&page.idx) {
                        info.width_px = page.width;
                        info.height_px = page.height;
                        info.load_state = SourcePageLoadState::Available;
                    }
                    self.store_decoded_page_cache(&page);
                    if page.idx >= self.next_decode_idx_to_enqueue {
                        self.decoded_pending_by_idx.insert(page.idx, page);
                        self.promote_decoded_pages_in_order();
                    }
                }
                Ok(LoaderEvent::Failed { idx, path, error }) => {
                    if let Some(info) = self.page_infos.get_mut(&idx) {
                        info.load_state = SourcePageLoadState::Failed;
                    }
                    self.failed_pages.insert(idx);
                    self.load_errors.push(format!("{path}: {error}"));
                    self.promote_decoded_pages_in_order();
                }
                Ok(LoaderEvent::Finished) => {
                    self.loader_finished = true;
                    self.promote_decoded_pages_in_order();
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.loader_finished = true;
                    self.promote_decoded_pages_in_order();
                    break;
                }
            }
        }
    }

    fn promote_decoded_pages_in_order(&mut self) {
        let total_pages = self.project.pages.len();
        while self.next_decode_idx_to_enqueue < total_pages {
            if self.failed_pages.contains(&self.next_decode_idx_to_enqueue) {
                self.next_decode_idx_to_enqueue += 1;
                continue;
            }
            let Some(page) = self
                .decoded_pending_by_idx
                .remove(&self.next_decode_idx_to_enqueue)
            else {
                break;
            };
            self.decoded_queue.push_back(page);
            self.next_decode_idx_to_enqueue += 1;
        }
        // Publish promotion progress so the decode worker pool can slide its look-ahead
        // window and avoid decoding pages far beyond what has been promoted (see 2.10).
        self.decode_promotion_progress
            .store(self.next_decode_idx_to_enqueue, AtomicOrdering::Release);
    }

    fn upload_textures_incremental(&mut self, ctx: &egui::Context) {
        let mut uploaded_tiles = 0usize;
        let mut uploaded_bytes = 0usize;

        loop {
            if uploaded_tiles >= UPLOAD_TILE_BUDGET_PER_FRAME
                || uploaded_bytes >= UPLOAD_BYTES_BUDGET_PER_FRAME
            {
                break;
            }

            if self.upload_task.is_none() {
                let Some(page) = self.decoded_queue.pop_front() else {
                    break;
                };
                self.upload_task = Some(UploadTask {
                    idx: page.idx,
                    tiles: page.tiles,
                    next_tile: 0,
                    uploaded_tiles: Vec::new(),
                });
            }

            let mut page_finished = false;
            if let Some(task) = self.upload_task.as_mut() {
                if task.next_tile >= task.tiles.len() {
                    page_finished = true;
                } else {
                    let tile = &task.tiles[task.next_tile];
                    let color_image = ColorImage::from_rgba_unmultiplied(
                        [tile.size_px[0] as usize, tile.size_px[1] as usize],
                        &tile.rgba,
                    );
                    let texture = ctx.load_texture(
                        format!("page-{}-tile-{}", task.idx, task.next_tile),
                        color_image,
                        TextureOptions::LINEAR,
                    );
                    task.uploaded_tiles.push(TextureTile {
                        linear_texture: Some(texture),
                        nearest_texture: None,
                        // Tile sides are bounded by DECODE_TILE_SIDE (2048) and origins by
                        // page dimensions, all far below f32's exact-integer limit (2^24),
                        // so these u32->f32 casts are exact. There is no f32::from(u32).
                        #[allow(clippy::cast_precision_loss)]
                        origin_px: egui::vec2(tile.origin_px[0] as f32, tile.origin_px[1] as f32),
                        #[allow(clippy::cast_precision_loss)]
                        size_px: egui::vec2(tile.size_px[0] as f32, tile.size_px[1] as f32),
                        rgba: Arc::from(tile.rgba.as_slice()),
                    });
                    task.next_tile += 1;
                    uploaded_tiles += 1;
                    uploaded_bytes += tile.rgba.len();
                    page_finished = task.next_tile >= task.tiles.len();
                }
            }

            if page_finished && let Some(task) = self.upload_task.take() {
                self.textures.insert(
                    task.idx,
                    PageTexture {
                        tiles: task.uploaded_tiles,
                        linear_last_used_frame: ctx.cumulative_frame_nr(),
                        nearest_last_used_frame: 0,
                    },
                );
            }
        }

        if !self.loader_finished
            || !self.decoded_queue.is_empty()
            || !self.decoded_pending_by_idx.is_empty()
            || self.upload_task.is_some()
        {
            ctx.request_repaint();
        }
    }

    fn loaded_pages_count(&self) -> usize {
        self.page_infos
            .values()
            .filter(|info| info.load_state != SourcePageLoadState::Loading)
            .count()
    }

    fn main_pages_fully_loaded(&self) -> bool {
        self.loader_finished
            && self.decoded_queue.is_empty()
            && self.decoded_pending_by_idx.is_empty()
            && self.upload_task.is_none()
            && self.next_decode_idx_to_enqueue >= self.project.pages.len()
    }

    fn active_source_page_window(&self, neighbor_radius: usize) -> HashSet<usize> {
        match self.active_tab {
            AppTab::Translation => self.canvas.active_source_page_window(neighbor_radius),
            AppTab::Cleaning => self.cleaning_tab.active_source_page_window(neighbor_radius),
            AppTab::Typing => self.typing_tab.active_source_page_window(neighbor_radius),
            // The PS-like editor owns its own page residency through `CleanOverlaysModel`'s page
            // cache, so it does not participate in the shared source-texture window.
            AppTab::PsEditor
            | AppTab::Characters
            | AppTab::Terms
            | AppTab::Notes
            | AppTab::Settings
            | AppTab::Wiki => HashSet::new(),
        }
    }

    fn active_canvas_source_pixel_inspection(&self) -> bool {
        match self.active_tab {
            AppTab::Translation => self.canvas.source_pixel_inspection_active(),
            AppTab::Cleaning => self.cleaning_tab.source_pixel_inspection_active(),
            AppTab::Typing => self.typing_tab.source_pixel_inspection_active(),
            AppTab::PsEditor
            | AppTab::Characters
            | AppTab::Terms
            | AppTab::Notes
            | AppTab::Settings
            | AppTab::Wiki => false,
        }
    }

    fn source_page_gpu_memory_snapshot(
        &self,
        active_pages: &HashSet<usize>,
    ) -> Vec<CacheResourceInfo> {
        let mut resources = Vec::new();
        for (page_idx, page_texture) in &self.textures {
            let visible = active_pages.contains(page_idx);
            let linear_bytes = page_texture.estimated_linear_gpu_bytes();
            if linear_bytes > 0 {
                resources.push(CacheResourceInfo {
                    id: format!("source-page-linear-{page_idx}"),
                    kind: CacheResourceKind::PageLinearGpu,
                    page_idx: Some(*page_idx),
                    estimated_bytes: linear_bytes,
                    last_used_frame: page_texture.linear_last_used_frame,
                    reload_cost: CacheReloadCost::Cheap,
                    dirty: false,
                    visible,
                    reconstructable: true,
                });
            }
            let nearest_bytes = page_texture.estimated_nearest_gpu_bytes();
            if nearest_bytes > 0 {
                resources.push(CacheResourceInfo {
                    id: format!("source-page-nearest-{page_idx}"),
                    kind: CacheResourceKind::PageNearestGpu,
                    page_idx: Some(*page_idx),
                    estimated_bytes: nearest_bytes,
                    last_used_frame: page_texture.nearest_last_used_frame,
                    reload_cost: CacheReloadCost::Cheap,
                    dirty: false,
                    visible,
                    reconstructable: true,
                });
            }
        }
        resources
    }

    fn evict_tab_gpu_caches(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let mut report = self.canvas.evict_clean_overlay_gpu_cache(request);
        let detector_report = self.translation_tab.evict_text_detector_gpu_cache(request);
        append_eviction_report(&mut report, detector_report);
        let cleaning_overlay_report = self.cleaning_tab.evict_clean_overlay_gpu_cache(request);
        append_eviction_report(&mut report, cleaning_overlay_report);
        let cleaning_mask_report = self.cleaning_tab.evict_cleaning_mask_gpu_cache(request);
        append_eviction_report(&mut report, cleaning_mask_report);
        let typing_overlay_report = self.typing_tab.evict_clean_overlay_gpu_cache(request);
        append_eviction_report(&mut report, typing_overlay_report);
        let typing_report = self.typing_tab.evict_gpu_caches(request);
        append_eviction_report(&mut report, typing_report);
        report
    }

    fn evict_source_page_gpu_cache(
        &mut self,
        request: &CacheEvictionRequest,
    ) -> CacheEvictionReport {
        let resources = self.source_page_gpu_memory_snapshot(
            &request.pinned_pages.iter().copied().collect::<HashSet<_>>(),
        );
        let report = select_eviction_candidates(&resources, request);
        for resource in &report.resources {
            let Some(page_idx) = resource.page_idx else {
                continue;
            };
            let Some(page_texture) = self.textures.get_mut(&page_idx) else {
                continue;
            };
            match resource.kind {
                CacheResourceKind::PageNearestGpu => page_texture.drop_nearest_textures(),
                CacheResourceKind::PageLinearGpu => page_texture.drop_linear_textures(),
                CacheResourceKind::SourcePageCpu
                | CacheResourceKind::CleanOverlayGpu
                | CacheResourceKind::CleanOverlayCpu
                | CacheResourceKind::DetectorMaskGpu
                | CacheResourceKind::CleaningMaskGpu
                | CacheResourceKind::TypingMaskGpu
                | CacheResourceKind::TextOverlayGpu
                | CacheResourceKind::PreviewGpu
                | CacheResourceKind::OcrPageCpu => {}
            }
        }
        report
    }

    fn trim_source_page_gpu_after_active_canvas(&mut self) {
        let budget = self.memory_manager.budget();
        let active_pages = self.active_source_page_window(budget.visible_neighbor_pages);
        let pixel_inspection = self.active_canvas_source_pixel_inspection();
        for (page_idx, page_texture) in &mut self.textures {
            if !pixel_inspection || !active_pages.contains(page_idx) {
                page_texture.drop_nearest_textures();
            }
            if !budget.keep_linear_gpu_outside_window && !active_pages.contains(page_idx) {
                page_texture.drop_linear_textures();
            }
        }
        if !pixel_inspection {
            return;
        }
        let request = CacheEvictionRequest {
            profile: budget.profile,
            pressure: MemoryPressure::Soft,
            target_free_bytes: u64::MAX,
            pinned_pages: active_pages.into_iter().collect::<BTreeSet<_>>(),
        };
        let _ = self.evict_source_page_gpu_cache(&request);
    }

    fn trim_tab_gpu_after_active_canvas(&mut self) {
        let budget = self.memory_manager.budget();
        let active_pages = self.active_source_page_window(budget.visible_neighbor_pages);
        let pinned_pages = active_pages.into_iter().collect::<BTreeSet<_>>();
        let pressure = if budget.profile == crate::memory_manager::MemoryProfile::Minimal {
            MemoryPressure::Soft
        } else {
            MemoryPressure::Normal
        };
        let request = CacheEvictionRequest {
            profile: budget.profile,
            pressure,
            target_free_bytes: if pressure == MemoryPressure::Normal {
                0
            } else {
                u64::MAX
            },
            pinned_pages,
        };
        // Eviction only runs under non-Normal pressure (Minimal profile / Soft pressure).
        // `evict_tab_gpu_caches` builds its own per-tab sub-snapshots internally, so no
        // separate snapshot is computed here.
        if request.pressure != MemoryPressure::Normal {
            let report = self.evict_tab_gpu_caches(&request);
            if !report.resources.is_empty() {
                runtime_log::log_info(format!(
                    "[memory] tab GPU trim freed {} resources, {} bytes",
                    report.resources.len(),
                    report.estimated_freed_bytes
                ));
            }
        }
    }

    fn poll_memory_pressure_and_evict(&mut self) {
        let now = Instant::now();
        if self
            .last_memory_pressure_poll
            .is_some_and(|last| now.duration_since(last) < MEMORY_PRESSURE_POLL_INTERVAL)
        {
            return;
        }
        self.last_memory_pressure_poll = Some(now);

        let profile = self.memory_manager.profile();
        let pressure = current_memory_availability()
            .map(|availability| classify_memory_pressure(profile, availability))
            .unwrap_or(MemoryPressure::Normal);
        let previous_pressure = self.memory_pressure;
        self.memory_pressure = pressure;
        if pressure != previous_pressure {
            runtime_log::log_info(format!(
                "[memory] pressure changed from {previous_pressure:?} to {pressure:?}"
            ));
        }
        if pressure == MemoryPressure::Normal {
            return;
        }

        let budget = self.memory_manager.budget();
        let pinned_pages = self
            .active_source_page_window(budget.visible_neighbor_pages)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let request = CacheEvictionRequest {
            profile,
            pressure,
            target_free_bytes: pressure_target_free_bytes(pressure),
            pinned_pages,
        };

        let mut report = self.evict_source_page_gpu_cache(&request);
        match self.clean_overlays_model.lock() {
            Ok(mut model) => {
                append_eviction_report(&mut report, model.evict_cache(&request));
            }
            Err(_) => runtime_log::log_warn(
                "[memory] failed to lock CleanOverlaysModel while evicting page cache",
            ),
        }
        append_eviction_report(&mut report, self.evict_tab_gpu_caches(&request));

        if report.estimated_freed_bytes > 0 {
            runtime_log::log_info(format!(
                "[memory] pressure={pressure:?} evicted_resources={} estimated_freed_bytes={}",
                report.resources.len(),
                report.estimated_freed_bytes
            ));
        }
    }

    fn ensure_overlay_loader_started(&mut self) {
        if self.overlay_loader_started || !self.main_pages_fully_loaded() {
            return;
        }
        self.overlay_loader_started = true;
        let jobs = collect_overlay_jobs(&self.project);
        let (tx, rx) = sync_channel(LOADER_QUEUE_CAPACITY);
        spawn_overlay_loader_thread(jobs, tx);
        self.overlay_loader_rx = Some(rx);
    }

    fn ensure_page_cache_loader_started(&mut self) {
        if self.page_cache_loader_started
            || !self.canvas.state.cache_pages
            || !self.memory_manager_allows_source_page_cache()
            || !self.main_pages_fully_loaded()
            || !self.overlay_loader_finished
        {
            return;
        }
        let jobs = collect_missing_page_cache_jobs(&self.project, &self.clean_overlays_model);
        if jobs.is_empty() {
            self.page_cache_loader_finished = true;
            return;
        }
        self.page_cache_loader_started = true;
        let (tx, rx) = sync_channel(LOADER_QUEUE_CAPACITY);
        spawn_page_cache_loader_thread(jobs, tx);
        self.page_cache_loader_rx = Some(rx);
    }

    fn poll_overlay_loader_events(&mut self) {
        let Some(rx) = self.overlay_loader_rx.as_ref() else {
            return;
        };
        for _ in 0..OVERLAY_EVENT_POLL_BUDGET {
            match rx.try_recv() {
                Ok(OverlayLoaderEvent::Decoded { idx, overlay }) => {
                    if let Ok(mut overlays) = self.clean_overlays_model.lock() {
                        overlays.load_prepared_overlay(idx, overlay.rgba, overlay.color);
                    }
                }
                Ok(OverlayLoaderEvent::Failed { idx, path, error }) => {
                    self.load_errors
                        .push(format!("overlay {idx} {path}: {error}"));
                }
                Ok(OverlayLoaderEvent::Finished) => {
                    self.overlay_loader_finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.overlay_loader_finished = true;
                    break;
                }
            }
        }
    }

    fn poll_page_cache_loader_events(&mut self) {
        let Some(rx) = self.page_cache_loader_rx.as_ref() else {
            return;
        };
        for _ in 0..OVERLAY_EVENT_POLL_BUDGET {
            match rx.try_recv() {
                Ok(PageCacheLoaderEvent::Decoded { idx, image }) => {
                    if let Ok(mut overlays) = self.clean_overlays_model.lock() {
                        let _ = overlays.store_cached_page_rgba(idx, image);
                    }
                }
                Ok(PageCacheLoaderEvent::Failed { idx, path, error }) => {
                    self.load_errors
                        .push(format!("page cache {idx} {path}: {error}"));
                }
                Ok(PageCacheLoaderEvent::Finished) => {
                    self.page_cache_loader_finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.page_cache_loader_finished = true;
                    break;
                }
            }
        }
    }

    fn store_decoded_page_cache(&self, page: &DecodedPage) {
        let Some(image) = page.cache_rgba.as_ref() else {
            return;
        };
        if let Ok(mut overlays) = self.clean_overlays_model.lock() {
            let _ = overlays.store_cached_page_rgba_arc(page.idx, Arc::clone(image));
        }
    }

    fn memory_manager_allows_source_page_cache(&self) -> bool {
        self.memory_manager.budget().source_page_cpu_cache_bytes > 0
    }

    fn sync_project_from_bubbles_model(&mut self) {
        let Ok(model) = self.bubbles_model.lock() else {
            return;
        };
        let revision = model.revision();
        if revision == self.applied_bubbles_revision {
            return;
        }
        self.project.bubbles = model.snapshot_shared();
        self.applied_bubbles_revision = revision;
        self.has_unsaved_changes_cached = true;
    }

    fn refresh_hotkey_hints_cache(&mut self, ctx: &egui::Context) {
        let bindings_revision = self.input_manager_v2.bindings_revision();
        if bindings_revision == self.cached_hotkey_hints.bindings_revision {
            return;
        }

        self.cached_hotkey_hints = CachedHotkeyHints {
            bindings_revision,
            create_bubble_hint: self
                .input_manager_v2
                .shortcut_text(ctx, HOTKEY_TRANSLATION_CREATE_BUBBLE),
            translation_hints: TranslationHotkeyHints {
                ocr_quick_selection_mode: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE),
                ocr_quick_selection_mode_modifier_down: false,
                ocr_advanced_selection_mode: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE),
                ocr_advanced_selection_mode_modifier_down: false,
                bubbles_panel: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL),
                ocr_panel: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL),
                composition_panel: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL),
                machine_translation_panel: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_MT_PANEL),
                text_detector_panel: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL),
            },
        };
    }

    fn ensure_fonts(&mut self, ctx: &egui::Context) {
        if self.fonts_initialized {
            return;
        }

        let system_candidates = [
            (
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                Some("/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc"),
            ),
            (
                "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
                Some("/usr/share/fonts/truetype/noto/NotoSansCJK-Bold.ttc"),
            ),
            (
                "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
                Some("/usr/share/fonts/truetype/nanum/NanumGothicBold.ttf"),
            ),
            (
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                Some("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"),
            ),
            ("/System/Library/Fonts/AppleSDGothicNeo.ttc", None),
            (
                "C:\\Windows\\Fonts\\malgun.ttf",
                Some("C:\\Windows\\Fonts\\malgunbd.ttf"),
            ),
        ];

        if !self.fonts_load_started {
            let (tx, rx) = std::sync::mpsc::channel::<Option<FontLoadResult>>();
            self.fonts_load_started = true;
            self.fonts_load_rx = Some(rx);
            thread::spawn(move || {
                for (regular_path, bold_path) in system_candidates {
                    let Ok(regular_bytes) = fs::read(regular_path) else {
                        continue;
                    };
                    let bold_bytes = bold_path.and_then(|path| fs::read(path).ok());
                    let _ = tx.send(Some(FontLoadResult {
                        regular_bytes,
                        bold_bytes,
                    }));
                    return;
                }
                let _ = tx.send(None);
            });
            return;
        }

        let Some(rx) = self.fonts_load_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Some(result)) => {
                let current_definitions = ctx.fonts(|fonts| fonts.definitions().clone());
                ctx.set_fonts(build_system_font_definitions(current_definitions, result));
                self.fonts_initialized = true;
                self.fonts_load_rx = None;
            }
            Ok(None) | Err(TryRecvError::Disconnected) => {
                self.fonts_initialized = true;
                self.fonts_load_rx = None;
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(web_time::Duration::from_millis(100));
            }
        }
    }

    fn draw_tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for tab in AppTab::ALL {
                let selected = self.active_tab == tab;
                if ui.selectable_label(selected, tab.title()).clicked() {
                    self.active_tab = tab;
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(t!("app.menu.to_launcher_button")).clicked() {
                    if self.has_unsaved_changes() {
                        self.exit_dialog = Some(ExitDialogKind::ReturnToLauncher);
                    } else {
                        self.finalize_close(ui.ctx(), PendingCloseAction::ReturnToLauncher);
                    }
                }
                let save_busy = self.save_to_project_rx.is_some() || self.pending_save_after_preload;
                if ui
                    .add_enabled(!save_busy, egui::Button::new(t!("app.menu.save_project_button")))
                    .clicked()
                {
                    self.request_save_to_project();
                }
                if let Some((status, _)) = &self.save_to_project_status {
                    ui.label(status.as_str());
                }
            });
        });
    }
}

fn build_system_font_definitions(
    mut defs: egui::FontDefinitions,
    result: FontLoadResult,
) -> egui::FontDefinitions {
    let regular_font_name = "system-ui-sans".to_string();
    let bold_font_name = "system-ui-sans-bold".to_string();
    defs.font_data.insert(
        regular_font_name.clone(),
        Arc::new(egui::FontData::from_owned(result.regular_bytes)),
    );
    if let Some(bold_bytes) = result.bold_bytes {
        defs.font_data.insert(
            bold_font_name.clone(),
            Arc::new(egui::FontData::from_owned(bold_bytes)),
        );
    }

    defs.families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, regular_font_name.clone());
    defs.families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push(regular_font_name.clone());

    let bold_family = defs
        .families
        .entry(egui::FontFamily::Name("system-ui-sans-bold".into()))
        .or_default();
    bold_family.clear();
    if defs.font_data.contains_key(&bold_font_name) {
        bold_family.push(bold_font_name);
    }
    bold_family.push(regular_font_name);
    defs
}

// The AI backend process + health/device probe outlive the studio window now (owned
// by `AiBackendSupervisor` in `run_main`), so `MangaApp` no longer stops them on drop.

impl eframe::App for MangaApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35 replaced `App::update(&Context, …)` with `App::ui(&mut Ui, …)`: the framework
        // now hands us the window-root `Ui` that fills the whole window. The top-level panels below
        // (`Panel::top`, `CentralPanel`) build on this `ui`; floating windows/prompts and every
        // context-level call keep using a borrowed `Context` handle cloned from it. Cloning (Arc
        // inside) is required so the context handle does not keep `ui` borrowed while the root
        // panels mutably borrow it.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // Advance the puffin frame and open the top-level scope for this frame. `new_frame`
        // must run exactly once per frame before any `profile_scope!` so recorded scopes are
        // attributed to the correct frame; the scope guard stays alive for the whole body.
        #[cfg(feature = "profiling")]
        puffin::GlobalProfiler::lock().new_frame();
        #[cfg(feature = "profiling")]
        puffin::profile_scope!("MangaApp::update");
        ctx.options_mut(|opt| {
            opt.zoom_with_keyboard = false;
        });
        #[cfg(target_os = "windows")]
        if self.maximize_root_window_on_first_frame {
            self.maximize_root_window_on_first_frame = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            ctx.request_repaint();
        }

        // Poll active "save to project" job and expire old status messages (show for 5 s).
        let now = ctx.input(|i| i.time);
        // Advance a save deferred behind the whole-project layer preload (started by
        // `request_save_to_project` when not every page was resident). Runs every frame regardless of
        // the active tab and dispatches the real save once every page is resident.
        self.drive_pending_save_preload(ctx);
        let save_completed_this_frame = self.poll_save_to_project(now);
        self.poll_pending_exit_cleanup(ctx, now);
        self.refresh_unsaved_changes_cache(now);
        if let Some((_, ts)) = &self.save_to_project_status {
            if *ts > 0.0 && now - ts > 5.0 {
                self.save_to_project_status = None;
            } else {
                ctx.request_repaint_after(web_time::Duration::from_secs(1));
            }
        }

        // After a "save then close" sequence: close once the save job finishes.
        if save_completed_this_frame
            && self.pending_save_completion_action.is_some()
            && self.save_to_project_rx.is_none()
        {
            if self.has_unsaved_changes() {
                let msg = tf!("app.save.temp_not_removed_error", arg = self.project.paths.unsaved_dir.display());
                self.save_to_project_status = Some((msg.clone(), now));
                runtime_log::log_error(format!("[save_to_project] {msg}"));
            } else {
                let action = self
                    .pending_save_completion_action
                    .take()
                    .unwrap_or(PendingCloseAction::Exit);
                self.exit_dialog = None;
                self.finalize_close(ctx, action);
            }
        }

        // Intercept the OS window-close button when there are unsaved changes.
        if ctx.input(|i| i.viewport().close_requested())
            && self.has_unsaved_changes()
            && self.exit_dialog.is_none()
        {
            self.pending_save_completion_action = Some(PendingCloseAction::Exit);
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.exit_dialog = Some(ExitDialogKind::WindowClose);
            // If no unsaved changes: let eframe handle the close normally.
        }

        self.ensure_fonts(ctx);
        {
            // Coarse marker for the per-frame worker polling + GPU upload phase: decoded page
            // promotion, incremental texture upload, and overlay/page-cache loader draining.
            #[cfg(feature = "profiling")]
            puffin::profile_scope!("poll_loaders_and_upload");
            self.poll_loader_events();
            self.upload_textures_incremental(ctx);
            self.ensure_overlay_loader_started();
            self.poll_overlay_loader_events();
            self.ensure_page_cache_loader_started();
            self.poll_page_cache_loader_events();
        }
        self.sync_project_from_bubbles_model();
        self.refresh_hotkey_hints_cache(ctx);
        self.canvas
            .set_create_bubble_shortcut_hint(self.cached_hotkey_hints.create_bubble_hint.clone());
        let mut translation_hotkey_hints = self.cached_hotkey_hints.translation_hints.clone();
        translation_hotkey_hints.ocr_quick_selection_mode_modifier_down = self
            .input_manager_v2
            .modifier_only_active(ctx, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE);
        translation_hotkey_hints.ocr_advanced_selection_mode_modifier_down = self
            .input_manager_v2
            .modifier_only_active(ctx, HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE);
        self.translation_tab
            .set_hotkey_hints(translation_hotkey_hints);
        if let Some(layout) = self.settings_tab.take_typing_panel_layout_request() {
            self.typing_tab.set_panel_layout(layout);
        }
        if (self.overlay_loader_started && !self.overlay_loader_finished)
            || (self.page_cache_loader_started && !self.page_cache_loader_finished)
        {
            ctx.request_repaint();
        }

        egui::Panel::top("top_bar").show(ui, |ui| {
            self.draw_tab_bar(ui);
        });

        // Show exit/leave dialog on top of all other content.
        self.draw_exit_dialog(ctx);

        self.apply_shared_viewport_to_active_canvas();

        // Coarse marker for the active-tab draw, the single most expensive phase per frame
        // (canvas/overlay rendering, side panels, tab-specific UI). `profile_scope_custom!`
        // returns the guard so it can be held across the whole match; it is dropped after the
        // match closes the scope. `_active_tab_draw_scope` is a held RAII guard, not dead code.
        #[cfg(feature = "profiling")]
        let _active_tab_draw_scope = puffin::profile_scope_custom!("active_tab_draw");
        match self.active_tab {
            AppTab::Translation => {
                self.translation_tab
                    .sync_with_project_settings(&self.project);
                self.translation_tab
                    .draw_side_panel(ctx, &mut self.canvas, &self.project);
                self.canvas.set_drag_scroll_blocked(false);
                self.canvas.set_wheel_scroll_blocked(false);
                self.canvas
                    .set_zoom_blocked(self.translation_tab.blocks_canvas_zoom());
                self.canvas.set_overlay_render_suppressed(false);
                self.canvas.set_overlay_upload_min_interval_s(0.0);
                let status = CanvasUiStatus {
                    loaded_pages: self.loaded_pages_count(),
                    total_pages: self.project.pages.len(),
                    load_errors_count: self.load_errors.len(),
                };
                let project = &self.project;
                let page_infos = &self.page_infos;
                let textures = &mut self.textures;
                let canvas = &mut self.canvas;
                let hooks = &mut self.translation_tab;

                egui::CentralPanel::default().show(ui, |ui| {
                    let mut source_upload_budget =
                        SourceTextureUploadBudget::source_page_reupload_default();
                    canvas.draw(CanvasDrawParams {
                        ctx,
                        ui,
                        project,
                        page_infos,
                        texture_cache: textures,
                        status,
                        source_upload_budget: &mut source_upload_budget,
                        hooks,
                    });
                });
            }
            AppTab::Cleaning => {
                let status = CanvasUiStatus {
                    loaded_pages: self.loaded_pages_count(),
                    total_pages: self.project.pages.len(),
                    load_errors_count: self.load_errors.len(),
                };
                let project = &self.project;
                let page_infos = &self.page_infos;
                let textures = &mut self.textures;
                let cleaning = &mut self.cleaning_tab;
                egui::CentralPanel::default().show(ui, |ui| {
                    cleaning.draw(ctx, ui, project, page_infos, textures, status);
                });
            }
            AppTab::Typing => {
                let status = CanvasUiStatus {
                    loaded_pages: self.loaded_pages_count(),
                    total_pages: self.project.pages.len(),
                    load_errors_count: self.load_errors.len(),
                };
                // Export is mutually exclusive with a project save (Finding 2): the save may be pending
                // on the whole-project preload or already running. Push it into the typing tab before
                // draw so both the export trigger and the deferred-export gate can suppress dispatch.
                let save_busy = self.save_to_project_rx.is_some() || self.pending_save_after_preload;
                let project = &self.project;
                let page_infos = &self.page_infos;
                let textures = &mut self.textures;
                let typing = &mut self.typing_tab;
                typing.set_save_busy(save_busy);
                egui::CentralPanel::default().show(ui, |ui| {
                    typing.draw(ctx, ui, project, page_infos, textures, status);
                });
            }
            AppTab::PsEditor => {
                let project = &self.project;
                let ps_editor = &mut self.ps_editor_tab;
                ps_editor.handle_hotkeys(ctx, project);
                egui::CentralPanel::default().show(ui, |ui| {
                    ps_editor.draw(ctx, ui, project);
                });
            }
            AppTab::Characters => {
                let project = &self.project;
                let tab = &mut self.characters_tab;
                let mut actions = Vec::new();
                egui::CentralPanel::default().show(ui, |ui| {
                    actions = tab.draw(ctx, ui, project);
                });
                for action in actions {
                    match action {
                        CharactersTabAction::CharactersChanged => {
                            self.notes_tab.notify_characters_changed();
                            self.translation_tab.notify_characters_changed();
                        }
                        CharactersTabAction::OpenNotesForCharacter(name) => {
                            self.notes_tab.notify_characters_changed();
                            self.notes_tab.set_character_context(name);
                            self.active_tab = AppTab::Notes;
                        }
                    }
                }
            }
            AppTab::Terms => {
                let project = &self.project;
                let tab = &mut self.terms_tab;
                let mut changed = false;
                egui::CentralPanel::default().show(ui, |ui| {
                    changed = tab.draw(ctx, ui, project);
                });
                if changed {
                    self.notes_tab.notify_terms_changed();
                }
            }
            AppTab::Notes => {
                let project = &self.project;
                let tab = &mut self.notes_tab;
                egui::CentralPanel::default().show(ui, |ui| tab.draw(ctx, ui, project));
            }
            AppTab::Settings => {
                let settings_tab = &mut self.settings_tab;
                let hotkeys_v2 = &mut self.input_manager_v2;
                egui::CentralPanel::default().show(ui, |ui| settings_tab.draw(ui, hotkeys_v2));
            }
            AppTab::Wiki => {
                egui::CentralPanel::default().show(ui, |ui| self.wiki_tab.draw(ui));
            }
        }
        // Close the active-tab draw scope before the post-draw GPU trim / memory phases.
        #[cfg(feature = "profiling")]
        drop(_active_tab_draw_scope);
        self.trim_source_page_gpu_after_active_canvas();
        self.trim_tab_gpu_after_active_canvas();
        self.poll_memory_pressure_and_evict();

        self.refresh_ai_backend_health_cache();
        self.refresh_ai_device_selection_prompt();
        self.refresh_ai_backend_version_warning();
        self.draw_comic_type_prompt(ctx);
        self.draw_ai_device_selection_prompt(ctx);
        self.draw_ai_backend_version_warning(ctx);
        if !self.comic_type_prompt_open
            && !self.ai_device_prompt_open
            && !self.ai_backend_version_warning_open
        {
            self.dispatch_hotkeys(ctx);
        } else {
            ctx.request_repaint();
        }
        self.publish_shared_viewport_from_active_canvas();

        // Frame profiler window (feature-gated). F11 toggles visibility; `profiler_window`
        // returns false when the user closes the window, which we mirror into the field.
        #[cfg(feature = "profiling")]
        {
            if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
                self.profiler_window_open = !self.profiler_window_open;
            }
            if self.profiler_window_open {
                self.profiler_window_open = puffin_egui::profiler_window(ctx);
                // The profiler view updates every frame; keep repainting while it is open.
                ctx.request_repaint();
            }
        }
    }

    /// Final shutdown drain for the background layer saver: called by eframe on real process exit
    /// (window close, return-to-launcher, or any other teardown). Blocks until every queued layer
    /// write is flushed and the worker thread is joined, so no pending raster/text/effects write is
    /// lost on exit. Must NOT hold the doc lock across the barrier: it clones the handle, drops the
    /// lock, then barriers; finally `shutdown_saver` drains the queue + joins the worker. Relying on
    /// Arc-drop ordering alone is unsafe (a lingering Arc clone in a worker could skip the join).
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 1) Barrier without holding the lock (clone the handle, drop the guard, then block).
        let handle = self
            .layer_doc
            .lock()
            .ok()
            .and_then(|guard| guard.saver_handle());
        if let Some(handle) = handle {
            // Teardown only drains writes; no project merge remains to reconcile failed pages.
            let _ = handle.barrier_blocking();
        }
        // 2) Drain remaining queue + join the worker thread (idempotent; safe if already shut down).
        if let Ok(mut guard) = self.layer_doc.lock() {
            guard.shutdown_saver();
        } else {
            runtime_log::log_error("[app] on_exit: shared doc lock poisoned, saver not joined");
        }
        runtime_log::log_info("[app] on_exit: layer saver drained and shut down");
    }
}

/// Decision for a project-save trigger given the current preload/save state — the pure core of
/// [`MangaApp::request_save_to_project`], extracted so the gate transition is unit-testable without
/// constructing a full `MangaApp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveGateDecision {
    /// A save is already running, or one is already pending on the preload — ignore the trigger.
    Ignore,
    /// Every page is resident — run the save immediately (fast path, unchanged behavior).
    SaveNow,
    /// Not every page is resident — start/continue the whole-project layer preload and keep the
    /// save pending until it drains.
    Defer,
}

/// Pure gate for a project-save TRIGGER. Save is deferred behind a full whole-project layer preload
/// so every page's lazy legacy→v3 text migration bakes into the committed `layers.json`. `Ignore`
/// keeps the trigger idempotent (a running or already-pending save is not restarted).
#[must_use]
fn save_trigger_decision(
    save_in_flight: bool,
    pending_after_preload: bool,
    all_pages_loaded: bool,
) -> SaveGateDecision {
    if save_in_flight || pending_after_preload {
        SaveGateDecision::Ignore
    } else if all_pages_loaded {
        SaveGateDecision::SaveNow
    } else {
        SaveGateDecision::Defer
    }
}

/// Pure gate for the per-frame deferred-save driver ([`MangaApp::drive_pending_save_preload`]): true
/// iff the deferred save should now be dispatched — the whole-project preload PASS has fully drained
/// (`!preload_active`).
///
/// It gates on pass COMPLETION, NOT on full residency (`all_pages_loaded`), and this is required for
/// correctness: a page whose decode genuinely fails (corrupt on-disk layer data, or a worker panic
/// that drops the sender) is dropped from the pass and NEVER becomes resident, so a residency gate
/// would leave the save deferred forever (permanent spinner, disabled "Save project", no retry, exit
/// via save unable to close). Dispatching after the pass drains is safe: the unsaved→committed merge
/// preserves a non-resident page's committed text verbatim (it is simply absent from
/// `owned_text_pages`), so nothing is lost. The pending flag is set only on the deferred path (when
/// `!all_pages_loaded`), and `begin_preload_all_pages` makes `preload_active` true that same frame
/// when pages need loading, so the gate still waits for the worker to drain before dispatching.
#[must_use]
fn deferred_save_ready(preload_active: bool) -> bool {
    !preload_active
}

fn ai_backend_version_warning_message(studio_version: &str, backend_version: &str) -> String {
    tf!("app.version_warning.mismatch_message", studio_version = studio_version, backend_version = backend_version)
}

fn append_eviction_report(target: &mut CacheEvictionReport, source: CacheEvictionReport) {
    target.estimated_freed_bytes = target
        .estimated_freed_bytes
        .saturating_add(source.estimated_freed_bytes);
    target.resources.extend(source.resources);
}

const fn pressure_target_free_bytes(pressure: MemoryPressure) -> u64 {
    match pressure {
        MemoryPressure::Normal => 0,
        MemoryPressure::Soft => SOFT_PRESSURE_TARGET_FREE_BYTES,
        MemoryPressure::Hard => HARD_PRESSURE_TARGET_FREE_BYTES,
        MemoryPressure::Critical => u64::MAX,
    }
}

fn build_input_manager_v2(user_settings_file: &Path) -> InputManagerV2 {
    let mut manager = InputManagerV2::default();
    manager.register(HotkeySpecV2 {
        id: HOTKEY_TRANSLATION_ZOOM_IN,
        title: t!("app.hotkey.zoom_in"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Equals,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Translation),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_TRANSLATION_ZOOM_OUT,
        title: t!("app.hotkey.zoom_out"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Minus,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Translation),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_TRANSLATION_ZOOM_RESET,
        title: t!("app.hotkey.zoom_reset"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Num0,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Translation),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_TRANSLATION_DELETE_BUBBLE,
        title: t!("app.hotkey.delete_bubble"),
        section: t!("app.hotkey.bubbles_category"),
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::NONE,
            egui::Key::Delete,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Translation),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_TRANSLATION_CREATE_BUBBLE,
        title: t!("app.hotkey.create_bubble"),
        section: t!("app.hotkey.bubbles_category"),
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::NONE,
            egui::Key::T,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Translation),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_CLEANING_ZOOM_IN,
        title: t!("app.hotkey.zoom_in"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Equals,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Cleaning),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_CLEANING_ZOOM_OUT,
        title: t!("app.hotkey.zoom_out"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Minus,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Cleaning),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_CLEANING_ZOOM_RESET,
        title: t!("app.hotkey.zoom_reset"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Num0,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Cleaning),
        active_when_input: false,
    });
    for spec in TranslationTabState::hotkey_specs() {
        manager.register(spec);
    }
    manager.load_overrides(user_settings_file);
    manager
}

fn option_label(options: &[AiBackendDeviceOption], selected_id: &str) -> String {
    options
        .iter()
        .find(|option| option.id == selected_id)
        .map(|option| option.label.clone())
        .unwrap_or_else(|| {
            if selected_id.trim().is_empty() {
                t!("app.hotkey.no_binding").to_string()
            } else {
                selected_id.to_string()
            }
        })
}

/// Spawns the source-page decode pool.
///
/// Decodes `pages` (`(page_idx, source_path)`) in a worker pool and streams each result
/// to the GUI thread as a `LoaderEvent` over `tx` (a bounded `SyncSender`, which provides
/// the GUI-side back-pressure on the decode-ahead window). `cache_pages_immediately`
/// requests seeding the shared page RGBA cache during decode. `promotion_progress` is the
/// GUI-updated frontier (next page index awaiting promotion); workers park while a job is
/// more than `DECODE_AHEAD_WINDOW` pages ahead of it so decoded-but-unpromotable pages
/// cannot pile up unbounded. The job queue is popped in ascending page-index order, which
/// keeps that look-ahead window deadlock-free.
/// Spawns a one-shot background thread that warms the process-global text-render FontSystem
/// pool, at most once per process.
///
/// The first `FontSystem` lease scans the system font directories (~2.2s in a fresh process,
/// ~32ms afterwards). Running that scan here, on a detached background thread, moves it off the
/// GUI thread and off the first text render's critical path. Idempotent across repeated
/// `MangaApp::new` calls (launcher<->studio transitions) via a `Once` guard, so no redundant
/// prewarm threads are spawned.
fn spawn_text_render_font_pool_prewarm() {
    static PREWARM_ONCE: std::sync::Once = std::sync::Once::new();
    PREWARM_ONCE.call_once(|| {
        runtime_log::log_info("[app] prewarming text-render font system pool in background");
        // Detached: the pool is process-global, the work is idempotent, and nothing on the GUI
        // thread waits on it. We must never join this on the GUI thread.
        thread::spawn(crate::tabs::typing::render_next::prewarm_font_system_pool);
    });
}

fn spawn_loader_thread(
    mut pages: Vec<(usize, PathBuf)>,
    tx: SyncSender<LoaderEvent>,
    cache_pages_immediately: bool,
    promotion_progress: Arc<AtomicUsize>,
) {
    thread::spawn(move || {
        if pages.is_empty() {
            let _ = tx.send(LoaderEvent::Finished);
            return;
        }

        // Enforce ascending page-index order so the look-ahead back-pressure below stays
        // deadlock-free regardless of how `project.pages` was constructed: the page that
        // unblocks promotion is always popped before any later page can park a worker.
        pages.sort_by_key(|(idx, _)| *idx);
        debug_assert!(
            pages.windows(2).all(|w| w[0].0 <= w[1].0),
            "decode jobs must be in ascending page-index order"
        );

        let worker_count = decode_worker_count(pages.len());
        let jobs = Arc::new(Mutex::new(VecDeque::from(pages)));
        let mut workers = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let tx = tx.clone();
            let jobs = Arc::clone(&jobs);
            let promotion_progress = Arc::clone(&promotion_progress);
            workers.push(thread::spawn(move || {
                loop {
                    let job = {
                        let mut queue =
                            jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        queue.pop_front()
                    };

                    let Some((idx, path)) = job else {
                        return;
                    };

                    // Look-ahead throttle: do not decode pages more than DECODE_AHEAD_WINDOW
                    // beyond the next page awaiting promotion, so decoded-but-unpromotable
                    // pages cannot pile up unbounded in the GUI-side pending map. The job
                    // queue is sorted ascending before this loop, so jobs are popped in
                    // ascending index order; the page that unblocks promotion is never stuck
                    // behind a parked worker and this cannot deadlock.
                    loop {
                        let progress = promotion_progress.load(AtomicOrdering::Acquire);
                        if decode_idx_within_window(idx, progress) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }

                    let event = match decode_page(idx, &path, cache_pages_immediately) {
                        Ok(page) => LoaderEvent::Decoded(page),
                        Err(error) => LoaderEvent::Failed {
                            idx,
                            path: path.display().to_string(),
                            error,
                        },
                    };
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }));
        }

        for worker in workers {
            let _ = worker.join();
        }

        let _ = tx.send(LoaderEvent::Finished);
    });
}

fn should_seed_page_cache_on_initial_load(
    cache_pages_enabled: bool,
    memory_budget: crate::memory_manager::MemoryBudget,
) -> bool {
    cache_pages_enabled && memory_budget.source_page_cpu_cache_bytes > 0
}

fn decode_worker_count(job_count: usize) -> usize {
    if job_count == 0 {
        return 1;
    }
    let logical = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let default_threads = logical.saturating_sub(1).max(1);
    let configured_threads = env::var(ENV_IMAGE_DECODE_THREADS)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|threads| *threads > 0);
    configured_threads
        .unwrap_or(default_threads)
        .clamp(1, job_count)
}

/// Returns true when page `idx` is close enough to the latest promoted page `progress`
/// to be decoded now, given the `DECODE_AHEAD_WINDOW` look-ahead bound.
///
/// `progress` is the lowest page index still awaiting in-order promotion. A page exactly at
/// `progress` (the one that unblocks promotion) is always inside the window, so the loader
/// pool always has at least one eligible job and cannot deadlock.
fn decode_idx_within_window(idx: usize, progress: usize) -> bool {
    idx < progress.saturating_add(DECODE_AHEAD_WINDOW)
}

fn decode_page(idx: usize, path: &Path, cache_page_rgba: bool) -> Result<DecodedPage, String> {
    let rgba = decode_image_rgba(idx, path)?;
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return Ok(DecodedPage {
            idx,
            width: w,
            height: h,
            tiles: Vec::new(),
            cache_rgba: cache_page_rgba.then(|| Arc::new(rgba)),
        });
    }

    let mut tiles = Vec::new();
    let mut y = 0u32;
    while y < h {
        let mut x = 0u32;
        while x < w {
            let tw = (w - x).min(DECODE_TILE_SIDE);
            let th = (h - y).min(DECODE_TILE_SIDE);
            tiles.push(DecodedTile {
                origin_px: [x, y],
                size_px: [tw, th],
                rgba: copy_rgba_tile_rows(&rgba, x, y, tw, th),
            });
            x += DECODE_TILE_SIDE;
        }
        y += DECODE_TILE_SIDE;
    }

    Ok(DecodedPage {
        idx,
        width: w,
        height: h,
        tiles,
        cache_rgba: cache_page_rgba.then(|| Arc::new(rgba)),
    })
}

fn collect_overlay_jobs(project: &ProjectData) -> Vec<(usize, PathBuf)> {
    project
        .pages
        .iter()
        .map(|page| {
            let stem = page
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("overlay");
            let candidate = project.paths.clean_layers_dir.join(format!("{stem}.png"));
            (page.idx, candidate)
        })
        .filter(|(_, p)| p.is_file())
        .collect()
}

fn collect_missing_page_cache_jobs(
    project: &ProjectData,
    clean_overlays_model: &Arc<Mutex<CleanOverlaysModel>>,
) -> Vec<(usize, PathBuf)> {
    let cached_indexes = clean_overlays_model
        .lock()
        .ok()
        .map(|model| {
            project
                .pages
                .iter()
                .filter_map(|page| model.has_cached_page_rgba(page.idx).then_some(page.idx))
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    project
        .pages
        .iter()
        .filter(|page| !cached_indexes.contains(&page.idx))
        .map(|page| (page.idx, page.path.clone()))
        .collect()
}

fn spawn_overlay_loader_thread(jobs: Vec<(usize, PathBuf)>, tx: SyncSender<OverlayLoaderEvent>) {
    thread::spawn(move || {
        if jobs.is_empty() {
            let _ = tx.send(OverlayLoaderEvent::Finished);
            return;
        }

        let worker_count = decode_worker_count(jobs.len());
        let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
        let mut workers = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let tx = tx.clone();
            let queue = Arc::clone(&queue);
            workers.push(thread::spawn(move || {
                loop {
                    let job = {
                        let mut q = queue.lock().unwrap_or_else(|p| p.into_inner());
                        q.pop_front()
                    };
                    let Some((idx, path)) = job else {
                        return;
                    };
                    let event = match decode_overlay(idx, &path) {
                        Ok(overlay) => OverlayLoaderEvent::Decoded { idx, overlay },
                        Err(error) => OverlayLoaderEvent::Failed {
                            idx,
                            path: path.display().to_string(),
                            error,
                        },
                    };
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }));
        }

        for w in workers {
            let _ = w.join();
        }
        let _ = tx.send(OverlayLoaderEvent::Finished);
    });
}

fn spawn_page_cache_loader_thread(
    jobs: Vec<(usize, PathBuf)>,
    tx: SyncSender<PageCacheLoaderEvent>,
) {
    thread::spawn(move || {
        if jobs.is_empty() {
            let _ = tx.send(PageCacheLoaderEvent::Finished);
            return;
        }

        let worker_count = decode_worker_count(jobs.len());
        let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
        let mut workers = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let tx = tx.clone();
            let queue = Arc::clone(&queue);
            workers.push(thread::spawn(move || {
                loop {
                    let job = {
                        let mut q = queue.lock().unwrap_or_else(|p| p.into_inner());
                        q.pop_front()
                    };
                    let Some((idx, path)) = job else {
                        return;
                    };
                    let event = match decode_page_rgba(idx, &path) {
                        Ok(image) => PageCacheLoaderEvent::Decoded { idx, image },
                        Err(error) => PageCacheLoaderEvent::Failed {
                            idx,
                            path: path.display().to_string(),
                            error,
                        },
                    };
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }));
        }

        for w in workers {
            let _ = w.join();
        }
        let _ = tx.send(PageCacheLoaderEvent::Finished);
    });
}

fn decode_overlay(idx: usize, path: &Path) -> Result<PreparedOverlay, String> {
    let image = decode_image_rgba(idx, path)?;
    let size = [image.width() as usize, image.height() as usize];
    Ok(PreparedOverlay {
        color: ColorImage::from_rgba_unmultiplied(size, image.as_raw()),
        rgba: Arc::new(image),
    })
}

fn decode_page_rgba(idx: usize, path: &Path) -> Result<image::RgbaImage, String> {
    decode_image_rgba(idx, path)
}

fn decode_image_rgba(idx: usize, path: &Path) -> Result<image::RgbaImage, String> {
    match try_decode_image_rgba_with_ffmpeg(path) {
        Ok(Some(decoded)) => return Ok(decoded),
        Ok(None) => {}
        Err(err) => {
            eprintln!(
                "warning: idx={idx} gpu decode failed for {}: {err}; fallback to CPU decoder",
                path.display()
            );
        }
    }
    // Read bytes through the storage seam (native passthrough = same file on
    // disk; web = in-memory/IndexedDB store) then decode in memory. This is what
    // lets page images load in the browser build.
    let bytes = crate::storage::storage()
        .read(path.to_string_lossy().as_ref())
        .map_err(|e| format!("idx={idx} read failed: {e}"))?;
    image::load_from_memory(&bytes)
        .map_err(|e| format!("idx={idx} decode failed: {e}"))
        .map(|img| img.to_rgba8())
}

fn try_decode_image_rgba_with_ffmpeg(path: &Path) -> Result<Option<image::RgbaImage>, String> {
    // The header is parsed once here (via `should_try_gpu_decode`) and the resulting
    // dimensions are reused for the raw-output size check below, instead of re-reading the
    // image header a second time.
    let Some((width, height)) = should_try_gpu_decode(path) else {
        return Ok(None);
    };
    if width == 0 || height == 0 {
        return Ok(Some(image::RgbaImage::new(width, height)));
    }

    let output = Command::new("ffmpeg")
        .arg("-v")
        .arg("error")
        .arg("-hwaccel")
        .arg("auto")
        .arg("-i")
        .arg(path)
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgba")
        .arg("-")
        .output()
        .map_err(|e| format!("failed to launch ffmpeg: {e}"))?;

    if !output.status.success() {
        return Err(format!("ffmpeg exited with status {}", output.status));
    }

    let expected = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    if output.stdout.len() != expected {
        return Err(format!(
            "ffmpeg raw output size mismatch: got {}, expected {}",
            output.stdout.len(),
            expected
        ));
    }

    image::RgbaImage::from_raw(width, height, output.stdout)
        .map(Some)
        .ok_or_else(|| "ffmpeg returned invalid RGBA buffer".to_string())
}

/// Decides whether to attempt experimental ffmpeg GPU decoding for `path`.
///
/// Returns `Some((width, height))` with the parsed header dimensions when GPU decode should
/// be tried, or `None` otherwise. The caller reuses the returned dimensions so the image
/// header is parsed only once across the decode-decision and decode-validation steps.
fn should_try_gpu_decode(path: &Path) -> Option<(u32, u32)> {
    if !gpu_decode_requested() || !ffmpeg_available() || !is_gpu_decode_extension_supported(path) {
        return None;
    }
    let (width, height) = image::image_dimensions(path).ok()?;
    let pixels = (width as u64).saturating_mul(height as u64);
    if pixels >= gpu_decode_min_pixels() {
        Some((width, height))
    } else {
        None
    }
}

fn is_gpu_decode_extension_supported(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "webp"
    )
}

fn gpu_decode_requested() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_var_bool(ENV_GPU_DECODE))
}

fn gpu_decode_min_pixels() -> u64 {
    static MIN_PIXELS: OnceLock<u64> = OnceLock::new();
    *MIN_PIXELS.get_or_init(|| {
        env::var(ENV_GPU_DECODE_MIN_PIXELS)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(GPU_DECODE_DEFAULT_MIN_PIXELS)
    })
}

fn ffmpeg_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

fn env_var_bool(name: &str) -> bool {
    let Ok(raw) = env::var(name) else {
        return false;
    };
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn copy_rgba_tile_rows(
    rgba: &image::RgbaImage,
    origin_x: u32,
    origin_y: u32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let src = rgba.as_raw();
    let src_stride = rgba.width() as usize * 4;
    let dst_stride = width as usize * 4;
    let mut out = vec![0u8; dst_stride.saturating_mul(height as usize)];

    for row in 0..height as usize {
        let src_start = (origin_y as usize + row)
            .saturating_mul(src_stride)
            .saturating_add(origin_x as usize * 4);
        let src_end = src_start.saturating_add(dst_stride);
        let dst_start = row.saturating_mul(dst_stride);
        let dst_end = dst_start.saturating_add(dst_stride);
        out[dst_start..dst_end].copy_from_slice(&src[src_start..src_end]);
    }

    out
}

/// Recursively copies every file from `unsaved_dir` into `project_dir` (overwriting), then removes
/// `unsaved_dir`. Called from a background thread by `start_save_to_project`.
///
/// `layers/layers.json` is SPECIAL-CASED: it is merged PER PAGE (not file-overwritten), because the
/// unsaved staging manifest only holds the pages the doc session actually visited, while the committed
/// manifest may carry MORE pages (e.g. all of them, written by the eager text migration). A blind
/// overwrite would DROP the committed-only pages — the ВВД/13 truncation. `owned_text_pages` is the set
/// of pages whose TEXT the doc loaded this session (from `flush_text_layers`); the merge replaces those
/// pages' text wholesale (authoritative, incl. deletions) and PRESERVES committed text for pages NOT in
/// it (a raster-only PS edit must not drop the committed text of a page whose text was never loaded).
/// Every other file (PNGs, the `text_info.json` legacy/`.bak`, bubbles, …) is copied verbatim as before.
fn merge_unsaved_into_project(
    unsaved_dir: &Path,
    project_dir: &Path,
    owned_text_pages: &std::collections::HashSet<usize>,
) -> Result<(), String> {
    if !unsaved_dir.is_dir() {
        // Nothing to merge — treat as success.
        return Ok(());
    }
    let committed_layers = project_dir.join(crate::config::LAYERS_DIR);
    let unsaved_layers = unsaved_dir.join(crate::config::LAYERS_DIR);
    // Copy everything EXCEPT the staging `layers/layers.json` (merged below).
    let layers_manifest = unsaved_layers.join("layers.json");
    copy_dir_overwrite_except(unsaved_dir, project_dir, &layers_manifest)?;
    // Merge the layers manifest per-page (committed-only pages preserved; unowned-page text preserved).
    crate::models::layer_model::persist::merge_unsaved_layers_into_committed(
        &committed_layers,
        &unsaved_layers,
        owned_text_pages,
    )
    .map_err(|e| tf!("app.merge.layers_merge_error", e = e))?;
    fs::remove_dir_all(unsaved_dir).map_err(|e| {
        tf!("app.merge.remove_temp_error", unsaved_dir = unsaved_dir.display(), e = e)
    })?;
    Ok(())
}

/// Recursively copies files from `src` into `dst`, creating subdirectories as needed. Existing files
/// in `dst` are overwritten. The file at the absolute path `skip` is NOT copied (handled separately).
fn copy_dir_overwrite_except(src: &Path, dst: &Path, skip: &Path) -> Result<(), String> {
    fs::create_dir_all(dst)
        .map_err(|e| tf!("app.merge.create_dir_error", dst = dst.display(), e = e))?;
    let entries = fs::read_dir(src)
        .map_err(|e| tf!("app.merge.read_dir_error", src = src.display(), e = e))?;
    for entry in entries {
        let entry = entry.map_err(|e| tf!("app.merge.read_entry_error", src = src.display(), e = e))?;
        let src_path = entry.path();
        if src_path == skip {
            continue;
        }
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_overwrite_except(&src_path, &dst_path, skip)?;
        } else {
            fs::copy(&src_path, &dst_path).map_err(|e| {
                tf!("app.merge.copy_error", src_path = src_path.display(), dst_path = dst_path.display(), e = e)
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DECODE_AHEAD_WINDOW, SaveGateDecision, ai_backend_version_warning_message,
        decode_idx_within_window, deferred_save_ready, save_trigger_decision,
        should_seed_page_cache_on_initial_load,
    };
    use crate::memory_manager::{MemoryBudget, MemoryProfile};

    #[test]
    fn save_trigger_defers_until_all_pages_loaded_then_saves() {
        // Not all pages resident and nothing in flight → defer behind the preload.
        assert_eq!(
            save_trigger_decision(false, false, false),
            SaveGateDecision::Defer
        );
        // Every page resident → run the save immediately (fast path).
        assert_eq!(
            save_trigger_decision(false, false, true),
            SaveGateDecision::SaveNow
        );
        // A save already running is never restarted, regardless of residency.
        assert_eq!(
            save_trigger_decision(true, false, true),
            SaveGateDecision::Ignore
        );
        assert_eq!(
            save_trigger_decision(true, false, false),
            SaveGateDecision::Ignore
        );
        // A save already pending on the preload is idempotent (a second click is ignored).
        assert_eq!(
            save_trigger_decision(false, true, false),
            SaveGateDecision::Ignore
        );
        assert_eq!(
            save_trigger_decision(false, true, true),
            SaveGateDecision::Ignore
        );
    }

    #[test]
    fn deferred_save_dispatches_when_preload_pass_drains() {
        // Dispatches once the preload PASS has drained (worker inactive), regardless of full
        // residency — the give-up path (a page that failed to decode never becomes resident) must
        // still reach dispatch, not hang.
        assert!(deferred_save_ready(false));
        // While the worker is still applying pages, keep waiting one more frame.
        assert!(!deferred_save_ready(true));
    }

    #[test]
    fn deferred_save_does_not_hang_on_decode_error_giving_up() {
        // FINDING 1 regression guard: a page that genuinely fails to decode is dropped from the pass
        // WITHOUT becoming resident, so `all_pages_loaded` would stay false forever. The gate keys on
        // pass completion (`!preload_active`), so once the worker finishes draining — even with a
        // failed page still non-resident — the deferred save dispatches instead of hanging.
        let all_pages_loaded_after_giveup = false; // the failed page never became resident
        let preload_active_after_drain = false; // the worker finished the pass
        // Old residency-based gate would have returned false here (hang); the new gate dispatches.
        assert!(deferred_save_ready(preload_active_after_drain));
        assert!(
            !all_pages_loaded_after_giveup,
            "precondition: not every page is resident after giving up on a decode failure"
        );
    }

    #[test]
    fn formats_ai_backend_version_warning() {
        // Pins the exact catalog key + args (in tests the empty default catalog makes
        // both sides render the key verbatim, so this compares key + interpolation).
        assert_eq!(
            ai_backend_version_warning_message("3.4.0", "3.3.0"),
            tf!(
                "app.version_warning.mismatch_message",
                studio_version = "3.4.0",
                backend_version = "3.3.0"
            )
        );
    }

    #[test]
    fn initial_page_cache_seed_requires_enabled_cache_and_memory_budget() {
        assert!(!should_seed_page_cache_on_initial_load(
            true,
            MemoryBudget::for_profile(MemoryProfile::Minimal)
        ));
        assert!(!should_seed_page_cache_on_initial_load(
            false,
            MemoryBudget::for_profile(MemoryProfile::Medium)
        ));
        assert!(should_seed_page_cache_on_initial_load(
            true,
            MemoryBudget::for_profile(MemoryProfile::Medium)
        ));
    }

    #[test]
    fn decode_window_always_admits_the_page_that_unblocks_promotion() {
        // The page exactly at the promotion frontier must always be decodable, otherwise the
        // loader could deadlock waiting for a page no worker is allowed to decode. Page counts
        // are far from usize::MAX in practice, so the frontier is always < progress + window.
        for progress in [0usize, 1, 7, 42, 100_000] {
            assert!(decode_idx_within_window(progress, progress));
        }
    }

    #[test]
    fn decode_window_bounds_look_ahead() {
        let progress = 10usize;
        // Last in-window index and first out-of-window index relative to the frontier.
        assert!(decode_idx_within_window(
            progress + DECODE_AHEAD_WINDOW - 1,
            progress
        ));
        assert!(!decode_idx_within_window(
            progress + DECODE_AHEAD_WINDOW,
            progress
        ));
        assert!(!decode_idx_within_window(
            progress + DECODE_AHEAD_WINDOW + 5,
            progress
        ));
    }

    #[test]
    fn decode_window_does_not_overflow_near_usize_max() {
        // saturating_add keeps the bound well-defined (no panic) even when progress is at the
        // maximum; the comparison still resolves without overflowing.
        assert!(decode_idx_within_window(usize::MAX - 1, usize::MAX));
        assert!(!decode_idx_within_window(usize::MAX, usize::MAX));
    }
}
