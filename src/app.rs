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
- `spawn_loader_thread`: decodes pages in worker pool and sends `LoaderEvent`.
- `spawn_overlay_loader_thread`: loads clean overlays in background and prebuilds the
  `RgbaImage` + `ColorImage` payload before it reaches the GUI thread.
- `decode_worker_count`: picks decode worker parallelism (logical cores by default, env override).
- `poll_loader_events` + `promote_decoded_pages_in_order`: preserve strict page order.
- `upload_textures_incremental`: frame-budgeted GPU upload to avoid GUI freezes.
- `poll_overlay_loader_events`: applies decoded overlays into shared model.
- `decode_image_rgba`: common image decode path with optional experimental GPU decode via ffmpeg.
- `page_cache`: when enabled from startup, main page decode also seeds the shared page RGBA cache
  to avoid a second full decode pass later.

App/frame flow:
- `new`: builds models, tabs, and input manager registrations.
- `new`: wires shared models (`bubbles`, `clean_overlays`, `text_mask`) between tabs.
- `new`: also starts shared AI-backend health probe and wires it into Settings/Translation tabs.
- `update` (eframe::App): frame tick (poll workers, draw active tab, dispatch hotkeys).
- `dispatch_hotkeys` + `execute_hotkey_command`: execute `InputManagerV2` commands.

Hotkeys:
- Translation canvas zoom/edit commands + panel toggles (`P/O/K/M/D` by default).
- Cleaning canvas zoom commands.
*/

use crate::ai_backend_capabilities;
use crate::canvas::{
    AsideBubbleCompactMode, AsideBubbleSideMode, BubbleMode, BubbleTextField, BubbleType,
    CanvasDrawParams, CanvasUiStatus, CanvasView, CanvasViewportSnapshot, OnTopFocusMode,
    SourceTextureUploadBudget, spawn_overlay_autosave_thread,
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
use crate::tabs::settings::SettingsTabState;
use crate::tabs::terms::TermsTabState;
use crate::tabs::translation::backend_health::{
    AiBackendDeviceOption, AiBackendHealthSnapshot, AiBackendProbeCommand, spawn_ai_backend_probe,
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
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DECODE_TILE_SIDE: u32 = 2048;
const LOADER_QUEUE_CAPACITY: usize = 2;
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
    ai_backend_health: Arc<Mutex<AiBackendHealthSnapshot>>,
    ai_backend_probe_tx: Option<Sender<AiBackendProbeCommand>>,
    ai_backend_probe_thread: Option<JoinHandle<()>>,
    translation_tab: TranslationTabState,
    cleaning_tab: CleaningTabState,
    typing_tab: TypingTabState,
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
        ai_enabled: bool,
        return_to_launcher_flag: Arc<AtomicBool>,
    ) -> Self {
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
        spawn_loader_thread(pages, tx, cache_pages);
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
            on_top_focus_mode: canvas.state.on_top_focus_mode.as_str().to_string(),
            scale_bubbles: canvas.state.scale_bubbles,
            aside_scale_pct: canvas.state.aside_scale_pct,
            auto_insert_last_character: canvas.state.auto_insert_last_character,
            spellcheck_original: canvas.state.spellcheck_original,
            spellcheck_translation: canvas.state.spellcheck_translation,
            tabs_autosync_enabled: canvas.state.tabs_autosync_enabled,
            cache_pages: canvas.state.cache_pages,
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
        ai_backend_capabilities::set_torch_available(if ai_enabled { None } else { Some(false) });
        let ai_backend_health = Arc::new(Mutex::new(if ai_enabled {
            AiBackendHealthSnapshot::default()
        } else {
            AiBackendHealthSnapshot::disabled()
        }));
        let mut ai_backend_probe_tx = None;
        let mut ai_backend_probe_thread = None;
        if ai_enabled {
            let (tx, handle) = spawn_ai_backend_probe(Arc::clone(&ai_backend_health));
            ai_backend_probe_tx = Some(tx);
            ai_backend_probe_thread = Some(handle);
        }
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
        let mut settings_tab = SettingsTabState::new(
            ai_enabled,
            Arc::clone(&ai_backend_health),
            ai_backend_probe_tx.clone(),
            Arc::clone(&memory_manager),
        );
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
            ai_backend_health,
            ai_backend_probe_tx: ai_backend_probe_tx.clone(),
            ai_backend_probe_thread,
            translation_tab,
            cleaning_tab,
            typing_tab,
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
            exit_dialog: None,
            pending_exit_cleanup: None,
            has_unsaved_changes_cached,
            next_unsaved_dir_check_s: 0.0,
            #[cfg(target_os = "windows")]
            maximize_root_window_on_first_frame: true,
            return_to_launcher: false,
            return_to_launcher_flag,
        }
    }

    /// Returns true if there are in-session changes not yet merged into the project folder.
    fn has_unsaved_changes(&self) -> bool {
        self.has_unsaved_changes_cached
    }

    fn refresh_unsaved_changes_cache(&mut self, now: f64) {
        if let Ok(model) = self.bubbles_model.lock()
            && model.has_unsaved_changes()
        {
            self.has_unsaved_changes_cached = true;
        }
        if let Ok(model) = self.clean_overlays_model.lock()
            && model.has_project_unsaved_changes()
        {
            self.has_unsaved_changes_cached = true;
        }
        if !self.has_unsaved_changes_cached && now >= self.next_unsaved_dir_check_s {
            self.has_unsaved_changes_cached = self.project.paths.unsaved_dir.exists();
            self.next_unsaved_dir_check_s = now + 1.0;
        }
    }

    /// Snapshot dirty overlay pages, save them in a background thread, then merge the
    /// unsaved staging dir into the project folder and remove the staging dir on completion.
    fn start_save_to_project(&mut self) {
        if self.save_to_project_rx.is_some() {
            return;
        }
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
            let result = merge_unsaved_into_project(&unsaved_dir, &project_dir);
            let _ = tx.send(result);
        });
        self.save_to_project_rx = Some(rx);
        self.save_to_project_status = Some(("Сохраняется…".to_string(), 0.0));
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
                self.save_to_project_status = Some(("Сохранено ✓".to_string(), now));
                runtime_log::log_info("[save_to_project] merge complete");
                true
            }
            Ok(Err(err)) => {
                self.save_to_project_rx = None;
                let msg = format!("Ошибка сохранения: {err}");
                runtime_log::log_error(format!("[save_to_project] {msg}"));
                self.save_to_project_status = Some((msg, now));
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.save_to_project_rx = None;
                let msg = "Ошибка: поток сохранения завершился неожиданно.".to_string();
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
                    format!(
                        "Не удалось удалить временную папку {}: {err}",
                        unsaved_dir.display()
                    )
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
        let rx = Self::spawn_unsaved_delete_job(self.project.paths.unsaved_dir.clone());
        self.pending_exit_cleanup = Some(PendingExitCleanup { action, rx });
        self.save_to_project_status = Some(("Завершается очистка сессии…".to_string(), 0.0));
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
                self.save_to_project_status = Some(("Временная сессия очищена ✓".to_string(), now));
                runtime_log::log_info("[exit] unsaved cleanup complete");
                self.finalize_close(ctx, action);
            }
            Ok(Err(err)) => {
                self.pending_exit_cleanup = None;
                self.exit_dialog = None;
                let msg = format!("Ошибка завершения: {err}");
                self.save_to_project_status = Some((msg.clone(), now));
                runtime_log::log_error(format!("[exit] {msg}"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending_exit_cleanup = None;
                let msg = "Ошибка: очистка временной сессии завершилась неожиданно.".to_string();
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
            ExitDialogKind::WindowClose => "Выход",
            ExitDialogKind::ReturnToLauncher => "Выход в лаунчер",
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                match kind {
                    ExitDialogKind::WindowClose => {
                        ui.label("В сессии есть несохранённые изменения.");
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Не выходить").clicked() {
                                close_dialog = true;
                            }
                            if ui.button("Сохранить главу").clicked() {
                                self.start_save_to_project();
                                // Wait for the save to complete before we exit.
                                // We set a flag so the next poll triggers the close.
                                close_dialog = true;
                                // We'll close after save completes — handled by
                                // a pending rx; the close below is re-attempted next frame.
                                // For simplicity: schedule delayed close after merge.
                                self.exit_dialog = Some(ExitDialogKind::WindowClose);
                                handled = true; // skip the unconditional close_dialog below
                            }
                            if ui.button("Не сохранять").clicked() {
                                self.start_exit_cleanup(PendingCloseAction::Exit);
                                close_dialog = true;
                                handled = true;
                            }
                        });
                    }
                    ExitDialogKind::ReturnToLauncher => {
                        ui.label("В сессии есть несохранённые изменения.");
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Не выходить").clicked() {
                                close_dialog = true;
                            }
                            if ui.button("Сохранить главу").clicked() {
                                self.pending_save_completion_action =
                                    Some(PendingCloseAction::ReturnToLauncher);
                                self.start_save_to_project();
                                close_dialog = true;
                                handled = true;
                            }
                            if ui.button("Не сохранять").clicked() {
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
            let user_message = "Не удалось сохранить тип комикса.\nПроверьте, доступен ли settings.json для записи.";
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
            on_top_focus_mode: self.canvas.state.on_top_focus_mode.as_str().to_string(),
            scale_bubbles: self.canvas.state.scale_bubbles,
            aside_scale_pct: self.canvas.state.aside_scale_pct,
            auto_insert_last_character: self.canvas.state.auto_insert_last_character,
            spellcheck_original: self.canvas.state.spellcheck_original,
            spellcheck_translation: self.canvas.state.spellcheck_translation,
            tabs_autosync_enabled: self.canvas.state.tabs_autosync_enabled,
            cache_pages: self.canvas.state.cache_pages,
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

        egui::Window::new("Выберите тип комикса")
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .show(ctx, |ui| {
                if let Some(message) = self.comic_type_prompt_error.as_ref() {
                    ui.colored_label(egui::Color32::from_rgb(220, 90, 90), message);
                    ui.add_space(8.0);
                }
                if ui.button("Страничный (манга)").clicked() {
                    self.apply_comic_type_preset(ComicType::Pages);
                }
                if ui.button("Вебтун (манхва, маньхуа)").clicked() {
                    self.apply_comic_type_preset(ComicType::Ribbon);
                }
            });
    }

    fn refresh_ai_device_selection_prompt(&mut self) {
        if self.ai_device_prompt_open {
            return;
        }

        let snapshot = match self.ai_backend_health.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
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

        let snapshot = match self.ai_backend_health.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
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

        egui::Window::new("Выбор видеокарты")
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .show(ctx, |ui| {
                ui.label("Выберите самую мощную видеокарту из доступных");
                ui.add_space(8.0);

                if let Some(message) = self.ai_device_prompt_error.as_ref() {
                    ui.colored_label(egui::Color32::from_rgb(220, 90, 90), message);
                    ui.add_space(8.0);
                }
                if self.ai_device_prompt_applying {
                    ui.small("Применяется выбор устройства...");
                    ui.add_space(8.0);
                }

                if snapshot.torch_device_needs_selection && !snapshot.device_options.is_empty() {
                    let selected_text = option_label(
                        &snapshot.device_options,
                        &self.ai_device_prompt_pytorch_device,
                    );
                    egui::ComboBox::from_label("Устройство PyTorch")
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
                    egui::ComboBox::from_label("Устройство DirectML")
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
                            egui::Button::new("Подтвердить"),
                        )
                        .clicked()
                    {
                        if snapshot.onnx_device_needs_selection
                            && self.ai_device_prompt_directml_device_id.trim().is_empty()
                        {
                            self.ai_device_prompt_error =
                                Some("Выберите устройство DirectML.".to_string());
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
                                    Some("Не удалось отправить выбор в AI backend.".to_string());
                            }
                        } else {
                            self.ai_device_prompt_error =
                                Some("AI backend сейчас недоступен.".to_string());
                        }
                    }
                });
            });
    }

    fn current_ai_backend_version_mismatch(&self) -> Option<(String, String)> {
        let snapshot = match self.ai_backend_health.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if !snapshot.connected {
            return None;
        }

        let studio_version = env!("CARGO_PKG_VERSION").trim().to_string();
        let backend_version = snapshot
            .backend_version
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("неизвестна")
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

        egui::Window::new("Предупреждение")
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

    fn active_tab_is_canvas(&self) -> bool {
        matches!(
            self.active_tab,
            AppTab::Translation | AppTab::Cleaning | AppTab::Typing
        )
    }

    fn apply_shared_viewport_to_active_canvas(&mut self) {
        if !self.active_tab_is_canvas() {
            return;
        }
        if self.active_viewport_owner_tab == Some(self.active_tab) {
            return;
        }
        let snapshot = self.shared_canvas_viewport;
        match self.active_tab {
            AppTab::Translation => self.canvas.apply_viewport_snapshot(snapshot),
            AppTab::Cleaning => self.cleaning_tab.apply_viewport_snapshot(snapshot),
            AppTab::Typing => self.typing_tab.apply_viewport_snapshot(snapshot),
            _ => {}
        }
        self.active_viewport_owner_tab = Some(self.active_tab);
    }

    fn publish_shared_viewport_from_active_canvas(&mut self) {
        if !self.active_tab_is_canvas() {
            return;
        }
        self.shared_canvas_viewport = match self.active_tab {
            AppTab::Translation => self.canvas.viewport_snapshot(),
            AppTab::Cleaning => self.cleaning_tab.viewport_snapshot(),
            AppTab::Typing => self.typing_tab.viewport_snapshot(),
            _ => self.shared_canvas_viewport,
        };
        self.active_viewport_owner_tab = Some(self.active_tab);
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
                        origin_px: egui::vec2(tile.origin_px[0] as f32, tile.origin_px[1] as f32),
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
            AppTab::Characters
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
            AppTab::Characters
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

    fn tab_gpu_memory_snapshot(&self, pinned_pages: &BTreeSet<usize>) -> Vec<CacheResourceInfo> {
        let mut resources = Vec::new();
        resources.extend(self.canvas.clean_overlay_gpu_memory_snapshot(pinned_pages));
        resources.extend(
            self.translation_tab
                .text_detector_gpu_memory_snapshot(pinned_pages),
        );
        resources.extend(
            self.cleaning_tab
                .clean_overlay_gpu_memory_snapshot(pinned_pages),
        );
        resources.extend(
            self.cleaning_tab
                .cleaning_mask_gpu_memory_snapshot(pinned_pages),
        );
        resources.extend(
            self.typing_tab
                .clean_overlay_gpu_memory_snapshot(pinned_pages),
        );
        resources.extend(self.typing_tab.gpu_memory_snapshot(pinned_pages));
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
        let _ = self.tab_gpu_memory_snapshot(&request.pinned_pages);
        if request.pressure != MemoryPressure::Normal {
            let _ = self.evict_tab_gpu_caches(&request);
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
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
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
                if ui.button("Выйти в лаунчер").clicked() {
                    if self.has_unsaved_changes() {
                        self.exit_dialog = Some(ExitDialogKind::ReturnToLauncher);
                    } else {
                        self.finalize_close(ui.ctx(), PendingCloseAction::ReturnToLauncher);
                    }
                }
                let save_busy = self.save_to_project_rx.is_some();
                if ui
                    .add_enabled(!save_busy, egui::Button::new("Сохранить проект"))
                    .clicked()
                {
                    self.start_save_to_project();
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

impl Drop for MangaApp {
    fn drop(&mut self) {
        if let Some(tx) = self.ai_backend_probe_tx.as_ref() {
            let _ = tx.send(AiBackendProbeCommand::Stop);
        }
        if let Some(handle) = self.ai_backend_probe_thread.take() {
            let _ = handle.join();
        }
    }
}

impl eframe::App for MangaApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
        let save_completed_this_frame = self.poll_save_to_project(now);
        self.poll_pending_exit_cleanup(ctx, now);
        self.refresh_unsaved_changes_cache(now);
        if let Some((_, ts)) = &self.save_to_project_status {
            if *ts > 0.0 && now - ts > 5.0 {
                self.save_to_project_status = None;
            } else {
                ctx.request_repaint_after(std::time::Duration::from_secs(1));
            }
        }

        // After a "save then close" sequence: close once the save job finishes.
        if save_completed_this_frame
            && self.pending_save_completion_action.is_some()
            && self.save_to_project_rx.is_none()
        {
            if self.has_unsaved_changes() {
                let msg = format!(
                    "Ошибка сохранения: временная папка {} не была удалена.",
                    self.project.paths.unsaved_dir.display()
                );
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
        self.poll_loader_events();
        self.upload_textures_incremental(ctx);
        self.ensure_overlay_loader_started();
        self.poll_overlay_loader_events();
        self.ensure_page_cache_loader_started();
        self.poll_page_cache_loader_events();
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

        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            self.draw_tab_bar(ui);
        });

        // Show exit/leave dialog on top of all other content.
        self.draw_exit_dialog(ctx);
        self.apply_shared_viewport_to_active_canvas();

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

                egui::CentralPanel::default().show(ctx, |ui| {
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
                egui::CentralPanel::default().show(ctx, |ui| {
                    cleaning.draw(ctx, ui, project, page_infos, textures, status);
                });
            }
            AppTab::Typing => {
                let status = CanvasUiStatus {
                    loaded_pages: self.loaded_pages_count(),
                    total_pages: self.project.pages.len(),
                    load_errors_count: self.load_errors.len(),
                };
                let project = &self.project;
                let page_infos = &self.page_infos;
                let textures = &mut self.textures;
                let typing = &mut self.typing_tab;
                egui::CentralPanel::default().show(ctx, |ui| {
                    typing.draw(ctx, ui, project, page_infos, textures, status);
                });
            }
            AppTab::Characters => {
                let project = &self.project;
                let tab = &mut self.characters_tab;
                let mut actions = Vec::new();
                egui::CentralPanel::default().show(ctx, |ui| {
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
                egui::CentralPanel::default().show(ctx, |ui| {
                    changed = tab.draw(ctx, ui, project);
                });
                if changed {
                    self.notes_tab.notify_terms_changed();
                }
            }
            AppTab::Notes => {
                let project = &self.project;
                let tab = &mut self.notes_tab;
                egui::CentralPanel::default().show(ctx, |ui| tab.draw(ctx, ui, project));
            }
            AppTab::Settings => {
                let settings_tab = &mut self.settings_tab;
                let hotkeys_v2 = &mut self.input_manager_v2;
                egui::CentralPanel::default().show(ctx, |ui| settings_tab.draw(ui, hotkeys_v2));
            }
            AppTab::Wiki => {
                egui::CentralPanel::default().show(ctx, |ui| self.wiki_tab.draw(ui));
            }
        }
        self.trim_source_page_gpu_after_active_canvas();
        self.trim_tab_gpu_after_active_canvas();
        self.poll_memory_pressure_and_evict();

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
    }
}

fn ai_backend_version_warning_message(studio_version: &str, backend_version: &str) -> String {
    format!(
        "Версии студии и ИИ бэкенда не соответствуют: {studio_version}/{backend_version}. Возможна некорректная работа некоторых ИИ сервисов"
    )
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
        title: "Увеличить масштаб",
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
        title: "Уменьшить масштаб",
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
        title: "Сбросить масштаб",
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
        title: "Удалить выбранный пузырь",
        section: "Пузыри",
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
        title: "Создать пузырь в позиции курсора",
        section: "Пузыри",
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
        title: "Увеличить масштаб",
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
        title: "Уменьшить масштаб",
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
        title: "Сбросить масштаб",
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
                "нет данных".to_string()
            } else {
                selected_id.to_string()
            }
        })
}

fn spawn_loader_thread(
    pages: Vec<(usize, PathBuf)>,
    tx: SyncSender<LoaderEvent>,
    cache_pages_immediately: bool,
) {
    thread::spawn(move || {
        if pages.is_empty() {
            let _ = tx.send(LoaderEvent::Finished);
            return;
        }

        let worker_count = decode_worker_count(pages.len());
        let jobs = Arc::new(Mutex::new(VecDeque::from(pages)));
        let mut workers = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let tx = tx.clone();
            let jobs = Arc::clone(&jobs);
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

fn decode_page(idx: usize, path: &PathBuf, cache_page_rgba: bool) -> Result<DecodedPage, String> {
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

fn decode_overlay(idx: usize, path: &PathBuf) -> Result<PreparedOverlay, String> {
    let image = decode_image_rgba(idx, path)?;
    let size = [image.width() as usize, image.height() as usize];
    Ok(PreparedOverlay {
        color: ColorImage::from_rgba_unmultiplied(size, image.as_raw()),
        rgba: Arc::new(image),
    })
}

fn decode_page_rgba(idx: usize, path: &PathBuf) -> Result<image::RgbaImage, String> {
    decode_image_rgba(idx, path)
}

fn decode_image_rgba(idx: usize, path: &PathBuf) -> Result<image::RgbaImage, String> {
    match try_decode_image_rgba_with_ffmpeg(path.as_path()) {
        Ok(Some(decoded)) => return Ok(decoded),
        Ok(None) => {}
        Err(err) => {
            eprintln!(
                "warning: idx={idx} gpu decode failed for {}: {err}; fallback to CPU decoder",
                path.display()
            );
        }
    }
    image::open(path)
        .map_err(|e| format!("idx={idx} decode failed: {e}"))
        .map(|img| img.to_rgba8())
}

fn try_decode_image_rgba_with_ffmpeg(path: &Path) -> Result<Option<image::RgbaImage>, String> {
    if !should_try_gpu_decode(path) {
        return Ok(None);
    }

    let (width, height) =
        image::image_dimensions(path).map_err(|e| format!("image_dimensions failed: {e}"))?;
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

fn should_try_gpu_decode(path: &Path) -> bool {
    if !gpu_decode_requested() || !ffmpeg_available() || !is_gpu_decode_extension_supported(path) {
        return false;
    }
    let Ok((width, height)) = image::image_dimensions(path) else {
        return false;
    };
    let pixels = (width as u64).saturating_mul(height as u64);
    pixels >= gpu_decode_min_pixels()
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

/// Recursively copies every file from `unsaved_dir` into `project_dir` (overwriting),
/// then removes `unsaved_dir`.  Called from a background thread by `start_save_to_project`.
fn merge_unsaved_into_project(unsaved_dir: &Path, project_dir: &Path) -> Result<(), String> {
    if !unsaved_dir.is_dir() {
        // Nothing to merge — treat as success.
        return Ok(());
    }
    copy_dir_overwrite(unsaved_dir, project_dir)?;
    fs::remove_dir_all(unsaved_dir).map_err(|e| {
        format!(
            "Не удалось удалить временную папку {}: {e}",
            unsaved_dir.display()
        )
    })?;
    Ok(())
}

/// Recursively copies files from `src` into `dst`, creating subdirectories as needed.
/// Existing files in `dst` are overwritten.
fn copy_dir_overwrite(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst)
        .map_err(|e| format!("Не удалось создать папку {}: {e}", dst.display()))?;
    let entries = fs::read_dir(src)
        .map_err(|e| format!("Не удалось прочитать папку {}: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Ошибка чтения записи в {}: {e}", src.display()))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_overwrite(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path).map_err(|e| {
                format!(
                    "Не удалось скопировать {} → {}: {e}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ai_backend_version_warning_message;

    #[test]
    fn formats_ai_backend_version_warning() {
        assert_eq!(
            ai_backend_version_warning_message("3.4.0", "3.3.0"),
            "Версии студии и ИИ бэкенда не соответствуют: 3.4.0/3.3.0. Возможна некорректная работа некоторых ИИ сервисов"
        );
    }
}
